//! The controller fabric and commissioning of discovered devices onto it.

use core::cell::RefCell;

use core::num::NonZeroU8;
use core::pin::pin;
use std::collections::BTreeMap;

use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use log::{info, warn};
use serde::{Deserialize, Serialize};

use rs_matter::cert::gen::VALID_FOREVER;
use rs_matter::cert::MAX_CERT_TLV_AND_ASN1_LEN;
use rs_matter::crypto::{
    CanonAeadKey, CanonPkcSecretKey, Crypto, Rng as _, SecretKey, SigningSecretKey,
};
use rs_matter::dm::clusters::net_comm::{
    NetworkCommissioningClient, NetworkCommissioningStatusEnum,
};
use rs_matter::dm::endpoints::ROOT_ENDPOINT_ID;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::fabric::FabricPersist;
use rs_matter::onboard::cac::{IcacGenerator, RcacGenerator};
use rs_matter::onboard::noc::NocGenerator;
use rs_matter::onboard::{CommissionOptions, Commissioner};
use rs_matter::pairing::qr::QrPayload;
use rs_matter::persist::{KvBlobStoreAccess, VENDOR_KEYS_START};
use rs_matter::tlv::OctetStr;
use rs_matter::transport::exchange::Exchange;
use rs_matter::transport::network::btp::Btp;
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::transport::network::Address;
use rs_matter::Matter;

use crate::ble;
use crate::control::Light;
use crate::discovery::{DiscoveredNode, Discovery};

const FABRIC_ID: u64 = 1;
const CONTROLLER_NODE_ID: u64 = 112233;
const FIRST_DEVICE_NODE_ID: u64 = 1;
/// The test vendor ID.
const ADMIN_VENDOR_ID: u16 = 0xFFF1;

const ICAC_KEY_KEY: u16 = VENDOR_KEYS_START;
const NODES_KEY: u16 = VENDOR_KEYS_START + 1;
const NEXT_NODE_ID_KEY: u16 = VENDOR_KEYS_START + 2;

const PHASE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long to scan Bluetooth for a device matching the pairing code.
const BLE_SCAN_TIMEOUT: Duration = Duration::from_secs(30);

const COMMISSION_OPTIONS: CommissionOptions = CommissionOptions {
    // Long enough for a device commissioned over Bluetooth to also join Wi-Fi before CASE.
    fail_safe_secs: 180,
    // rs-matter cannot verify device attestation against the DCL yet;
    // `false` makes every commissioning fail.
    allow_test_attestation: true,
};

/// Load the controller fabric and its ICAC signing key from `kv`, or create
/// and persist both on first start.
///
/// `matter.startup(kv)` must have run before.
pub fn load_or_create_fabric<C: Crypto, K: KvBlobStoreAccess>(
    matter: &Matter<'_>,
    crypto: C,
    kv: K,
) -> Result<(NonZeroU8, CanonPkcSecretKey), Error> {
    let fab_idx = matter.with_state(|state| state.fabrics.iter().next().map(|f| f.fab_idx()));
    let icac_key = kv.access(|store, buf| {
        store
            .load(ICAC_KEY_KEY, buf)?
            .map(|data| {
                let mut key = CanonPkcSecretKey::new();
                key.try_load_from_slice(data)?;
                Ok::<_, Error>(key)
            })
            .transpose()
    })?;

    match (fab_idx, icac_key) {
        (Some(fab_idx), Some(icac_key)) => {
            info!("Loaded controller fabric {fab_idx}");
            Ok((fab_idx, icac_key))
        }
        (None, None) => create_fabric(matter, crypto, kv),
        _ => panic!(
            "Storage is inconsistent: the controller fabric and its ICAC key must exist together"
        ),
    }
}

fn create_fabric<C: Crypto, K: KvBlobStoreAccess>(
    matter: &Matter<'_>,
    crypto: C,
    kv: K,
) -> Result<(NonZeroU8, CanonPkcSecretKey), Error> {
    let mut rcac_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut rcac_generator = RcacGenerator::new(&mut rcac_buf);
    let (rcac_key, rcac) = rcac_generator.generate(&crypto, FABRIC_ID, VALID_FOREVER)?;

    let mut icac_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut icac_generator = IcacGenerator::new(&mut icac_buf);
    let (icac_key, icac) =
        icac_generator.generate(&crypto, rcac_key.reference(), rcac, VALID_FOREVER)?;
    // The RCAC key is only needed to sign the ICAC.
    drop(rcac_key);

    let controller_key = crypto.generate_secret_key()?;
    let mut csr_buf = [0u8; 256];
    let csr = controller_key.csr(&mut csr_buf)?;
    let mut controller_key_canon = CanonPkcSecretKey::new();
    controller_key.write_canon(&mut controller_key_canon)?;

    let mut noc_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut noc_generator = NocGenerator::create(icac_key.reference(), rcac, icac, &mut noc_buf)?;
    let noc = noc_generator.generate(&crypto, csr, CONTROLLER_NODE_ID, &[], VALID_FOREVER)?;

    let mut ipk = CanonAeadKey::new();
    crypto.rand()?.fill_bytes(ipk.access_mut());

    let fab_idx = matter.with_state(|state| {
        let fabric = state.fabrics.add(
            &crypto,
            controller_key_canon.reference(),
            rcac,
            noc,
            icac,
            Some(ipk.reference()),
            ADMIN_VENDOR_ID,
            CONTROLLER_NODE_ID,
        )?;
        FabricPersist::new(&kv).store(fabric)?;
        Ok::<_, Error>(fabric.fab_idx())
    })?;

    kv.access(|store, buf| store.store(ICAC_KEY_KEY, icac_key.reference().access(), buf))?;

    info!("Created controller fabric {fab_idx}");

    Ok((fab_idx, icac_key))
}

#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub node_id: u64,
    /// The node's operational mDNS record, if it is currently advertised.
    pub operational: Option<DiscoveredNode>,
}

/// The Wi-Fi network a device commissioned over Bluetooth is put on.
#[derive(Debug, Clone, Deserialize)]
pub struct WiFi {
    pub ssid: String,
    pub password: String,
}

pub struct Controller<'a, C: Crypto, K> {
    pub matter: &'a Matter<'a>,
    pub crypto: C,
    pub discovery: &'a Discovery,
    pub btp: &'a Btp,
    pub kv: K,
    pub fab_idx: NonZeroU8,
    pub nodes: RefCell<Vec<u64>>,
    /// The light endpoint of each node, looked up on first use.
    pub lights: RefCell<BTreeMap<u64, Light>>,
    /// Serializes commissioning: one device at a time.
    pub commissioner: Mutex<NoopRawMutex, Commissioner<'a, 'a, C>>,
}

impl<'a, C: Crypto, K: KvBlobStoreAccess> Controller<'a, C, K> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        matter: &'a Matter<'a>,
        crypto: C,
        discovery: &'a Discovery,
        btp: &'a Btp,
        kv: K,
        fab_idx: NonZeroU8,
        noc_generator: &'a mut NocGenerator<'a>,
        commissioner_buf: &'a mut [u8],
    ) -> Result<Self, Error>
    where
        C: Copy,
    {
        let nodes = kv.access(|store, buf| {
            Ok::<_, Error>(
                store
                    .load(NODES_KEY, buf)?
                    .map(|data| {
                        data.chunks_exact(8)
                            .map(|id| u64::from_le_bytes(id.try_into().unwrap()))
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        })?;

        Ok(Self {
            matter,
            crypto,
            discovery,
            btp,
            kv,
            fab_idx,
            nodes: RefCell::new(nodes),
            lights: RefCell::new(BTreeMap::new()),
            commissioner: Mutex::new(Commissioner::new(
                matter,
                crypto,
                fab_idx,
                noc_generator,
                commissioner_buf,
            )),
        })
    }

    pub fn compressed_fabric_id(&self) -> u64 {
        self.matter.with_state(|state| {
            state
                .fabrics
                .get(self.fab_idx)
                .map(|fabric| fabric.compressed_fabric_id())
                .expect("controller fabric is installed")
        })
    }

    pub fn nodes(&self) -> Vec<Node> {
        let compressed_fabric_id = self.compressed_fabric_id();

        self.nodes
            .borrow()
            .iter()
            .map(|&node_id| Node {
                node_id,
                operational: self.discovery.operational(compressed_fabric_id, node_id),
            })
            .collect()
    }

    /// Commission the device matching `filter` (see [`parse_code`]).
    ///
    /// Devices discovered over mDNS are commissioned over IP. Otherwise, given
    /// `wifi`, the device is searched over Bluetooth and put on that network.
    ///
    /// Returns `None` if no such device has been found.
    pub async fn commission(
        &self,
        filter: &CommissionableFilter,
        passcode: u32,
        wifi: Option<&WiFi>,
    ) -> Result<Option<Node>, Error> {
        let candidates = self.discovery.commissionable(filter);
        if candidates.is_empty() && wifi.is_none() {
            return Ok(None);
        }

        let mut commissioner = self.commissioner.lock().await;
        let node_id = self.allocate_node_id()?;

        match wifi {
            Some(wifi) if candidates.is_empty() => {
                info!("No device on the network matches, scanning Bluetooth");
                let connection = match select(
                    pin!(ble::Connection::connect(filter)),
                    Timer::after(BLE_SCAN_TIMEOUT),
                )
                .await
                {
                    Either::First(connection) => connection?,
                    Either::Second(()) => return Ok(None),
                };

                info!(
                    "Commissioning over Bluetooth onto {:?} as node {node_id}",
                    wifi.ssid
                );
                commission_ble(
                    &mut commissioner,
                    self.matter,
                    &self.crypto,
                    self.btp,
                    self.discovery,
                    self.compressed_fabric_id(),
                    connection,
                    passcode,
                    wifi,
                    node_id,
                )
                .await?;
            }
            _ => {
                // A short discriminator (manual pairing code) can match several devices; the passcode tells them apart.
                let mut result = Ok(());
                for candidate in &candidates {
                    let addr = candidate
                        .socket_addr(self.discovery.interface.index)
                        .ok_or(ErrorCode::NoNetworkInterface)?;

                    info!(
                        "Commissioning {} at {addr} as node {node_id}",
                        candidate.instance
                    );
                    result = commission_one(&mut commissioner, addr, passcode, node_id).await;
                    match &result {
                        Ok(()) => break,
                        Err(e) => warn!("Commissioning {} failed: {e:?}", candidate.instance),
                    }
                }
                result?;
            }
        }

        self.nodes.borrow_mut().push(node_id);
        self.store_nodes()?;
        info!("Commissioned node {node_id}");

        Ok(Some(
            self.nodes()
                .into_iter()
                .find(|node| node.node_id == node_id)
                .expect("node was just added"),
        ))
    }

    /// A node ID no commissioning attempt has used before: a device from a failed
    /// attempt keeps announcing its ID until its fail-safe expires.
    pub fn allocate_node_id(&self) -> Result<u64, Error> {
        self.kv.access(|store, buf| {
            let node_id = match store.load(NEXT_NODE_ID_KEY, buf)? {
                Some(data) => {
                    u64::from_le_bytes(data.try_into().map_err(|_| ErrorCode::InvalidData)?)
                }
                // First commissioning with the counter: continue after the nodes stored so far.
                None => self
                    .nodes
                    .borrow()
                    .iter()
                    .max()
                    .map_or(FIRST_DEVICE_NODE_ID, |id| id + 1),
            };
            store.store(NEXT_NODE_ID_KEY, &(node_id + 1).to_le_bytes(), buf)?;
            Ok(node_id)
        })
    }

    fn store_nodes(&self) -> Result<(), Error> {
        let data: Vec<u8> = self
            .nodes
            .borrow()
            .iter()
            .flat_map(|id| id.to_le_bytes())
            .collect();

        self.kv
            .access(|store, buf| store.store(NODES_KEY, &data, buf))
    }
}

/// Parse a manual pairing code (`3497-0112-332`) or a QR code payload (`MT:...`).
pub fn parse_code(code: &str) -> Result<(CommissionableFilter, u32), Error> {
    if code.starts_with("MT:") {
        let mut buf = [0u8; 256];
        let payload = QrPayload::parse(code, &mut buf)?;
        Ok((payload.commissionable_filter(), payload.passcode()))
    } else {
        let payload = QrPayload::parse_pairing_code(code)?;
        Ok((payload.commissionable_filter(), payload.passcode()))
    }
}

async fn commission_one<C: Crypto>(
    commissioner: &mut Commissioner<'_, '_, C>,
    addr: core::net::SocketAddr,
    passcode: u32,
    node_id: u64,
) -> Result<(), Error> {
    let phase1 = with_timeout(commissioner.commission(
        Address::Udp(addr),
        passcode,
        &COMMISSION_OPTIONS,
        node_id,
        VALID_FOREVER,
    ))
    .await?;

    // Resolves the node's operational address over mDNS (served by `Discovery`), then CASE + CommissioningComplete.
    with_timeout(commissioner.complete_via_case(&phase1)).await
}

/// PASE, the operational credentials and the Wi-Fi network over Bluetooth, then
/// CASE over the network the device has just joined.
#[allow(clippy::too_many_arguments)]
async fn commission_ble<C: Crypto>(
    commissioner: &mut Commissioner<'_, '_, C>,
    matter: &Matter<'_>,
    crypto: impl Crypto,
    btp: &Btp,
    discovery: &Discovery,
    compressed_fabric_id: u64,
    connection: ble::Connection,
    passcode: u32,
    wifi: &WiFi,
    node_id: u64,
) -> Result<(), Error> {
    // Every Bluetooth device goes by `ble::PEER`, so a PASE session left from the previous one would be reused.
    matter.with_state(|state| {
        state
            .sessions
            .remove_where(|session| session.get_peer_addr().is_btp())
    });
    matter.transport().notify_session_removed();

    let addr = Address::Btp(ble::PEER);
    let over_ble = async {
        let phase1 = with_timeout(commissioner.commission(
            addr,
            passcode,
            &COMMISSION_OPTIONS,
            node_id,
            VALID_FOREVER,
        ))
        .await?;
        with_timeout(connect_wifi(matter, &crypto, addr, passcode, wifi)).await?;
        Ok(phase1)
    };

    let phase1 = match select(pin!(connection.run(btp)), pin!(over_ble)).await {
        Either::First(result) => result.map(|never| match never {}),
        Either::Second(result) => result,
    };
    // Devices may drop the Bluetooth link once they are on Wi-Fi; CASE does not need it.
    connection.disconnect().await?;
    let phase1 = phase1?;

    info!("Waiting for node {node_id} to appear on the network");
    with_timeout(async {
        discovery
            .wait_operational(compressed_fabric_id, node_id)
            .await;
        Ok(())
    })
    .await?;

    with_timeout(commissioner.complete_via_case(&phase1)).await
}

/// `AddOrUpdateWiFiNetwork` + `ConnectNetwork` over the device's PASE session.
async fn connect_wifi(
    matter: &Matter<'_>,
    crypto: impl Crypto,
    addr: Address,
    passcode: u32,
    wifi: &WiFi,
) -> Result<(), Error> {
    let ssid = OctetStr::new(wifi.ssid.as_bytes());

    let exchange = Exchange::initiate_pase(matter, &crypto, addr, passcode).await?;
    let handle = exchange
        .network_commissioning()
        .add_or_update_wi_fi_network(ROOT_ENDPOINT_ID, |req| {
            req.ssid(ssid)?
                .credentials(OctetStr::new(wifi.password.as_bytes()))?
                .breadcrumb(None)?
                .end()
        })
        .await?;
    let status = handle.response()?.networking_status()?;
    handle.complete().await?;
    check_networking_status("AddOrUpdateWiFiNetwork", status)?;

    // Answered once the device has joined the network - but many devices hand their radio over to
    // Wi-Fi and never answer over Bluetooth. CASE over the network then shows whether the join worked.
    let exchange = Exchange::initiate_pase(matter, &crypto, addr, passcode).await?;
    match exchange
        .network_commissioning()
        .connect_network(ROOT_ENDPOINT_ID, |req| {
            req.network_id(ssid)?.breadcrumb(None)?.end()
        })
        .await
    {
        Ok(handle) => {
            let status = handle.response()?.networking_status()?;
            handle.complete().await?;
            check_networking_status("ConnectNetwork", status)
        }
        Err(e) if e.is_peer_unresponsive() => {
            warn!("No ConnectNetwork response over Bluetooth ({e}), continuing over the network");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn check_networking_status(
    command: &str,
    status: NetworkCommissioningStatusEnum,
) -> Result<(), Error> {
    if status != NetworkCommissioningStatusEnum::Success {
        warn!("{command} failed: {status:?}");
        return Err(ErrorCode::Failure.into());
    }

    Ok(())
}

async fn with_timeout<T>(
    fut: impl core::future::Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    match select(pin!(fut), Timer::after(PHASE_TIMEOUT)).await {
        Either::First(result) => result,
        Either::Second(()) => Err(ErrorCode::RxTimeout.into()),
    }
}
