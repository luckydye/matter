//! Bluetooth LE: continuous scanning for commissionable devices, and the
//! transport for commissioning devices that are not on the network yet.
//!
//! A [`Connection`] connects to a commissionable device as GATT Central and
//! shuttles BTP segments between the device's `C1`/`C2` characteristics and
//! rs-matter's [`Btp`] (in its initiator role), which the Matter transport sends
//! `Address::Btp` packets through.

use core::cell::RefCell;
use core::convert::Infallible;
use core::pin::pin;

use std::collections::BTreeMap;
use std::time::Instant;

use bluest::{Adapter, AdvertisingDevice, Characteristic, Device, Uuid};
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Timer};
use futures_lite::StreamExt;
use log::{info, warn};
use serde::Serialize;

use rs_matter::error::{Error, ErrorCode};
use rs_matter::transport::network::btp::{
    AdvData, Btp, C1_CHARACTERISTIC_UUID, C2_CHARACTERISTIC_UUID, MATTER_BLE_SERVICE_UUID,
};
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::transport::network::BtAddr;

use crate::discovery::EXPIRY_SECS;

/// CoreBluetooth does not expose peer MAC addresses, and BTP runs one session
/// at a time, so the connected device always goes by this address.
pub const PEER: BtAddr = BtAddr([0x02, 0, 0, 0, 0, 0x01]);

/// The ATT header on top of a GATT write payload.
const GATT_HEADER_SIZE: usize = 3;
/// CoreBluetooth reports each device once per scan, so the scan is restarted
/// this often to refresh devices that are still advertising.
const SCAN_WINDOW: Duration = Duration::from_secs(30);

/// A device advertising over Bluetooth that it is in commissioning mode.
#[derive(Debug, Clone, Serialize)]
pub struct BleDevice {
    /// The OS's identifier for the device (CoreBluetooth does not expose MAC addresses).
    pub id: String,
    pub name: Option<String>,
    pub discriminator: u16,
    pub vendor_id: u16,
    pub product_id: u16,
    pub rssi: Option<i16>,
    pub seen_secs_ago: u64,
    #[serde(skip)]
    pub last_seen: Instant,
}

pub struct BleScanner {
    pub devices: RefCell<BTreeMap<String, BleDevice>>,
}

impl BleScanner {
    pub fn new() -> Self {
        Self {
            devices: RefCell::new(BTreeMap::new()),
        }
    }

    pub fn devices(&self) -> Vec<BleDevice> {
        let now = Instant::now();

        self.devices
            .borrow()
            .values()
            .map(|device| BleDevice {
                seen_secs_ago: now.duration_since(device.last_seen).as_secs(),
                ..device.clone()
            })
            .collect()
    }

    pub async fn run(&self) -> Result<(), Error> {
        let Some(adapter) = Adapter::default().await else {
            // Devices already on the network are still found over mDNS.
            info!("No Bluetooth adapter, not scanning");
            return core::future::pending().await;
        };
        let services = [Uuid::from_u128(MATTER_BLE_SERVICE_UUID)];

        loop {
            adapter.wait_available().await.map_err(ble_error)?;
            info!("Bluetooth scan running");

            // Ends early when Bluetooth is turned off; the outer loop then waits for it to come back.
            let mut scan = adapter.scan(&services).await.map_err(ble_error)?;
            let mut window = pin!(Timer::after(SCAN_WINDOW));
            while let Either::First(Some(found)) = select(scan.next(), &mut window).await {
                self.record(&found);
            }
            drop(scan);

            self.devices
                .borrow_mut()
                .retain(|_, device| device.last_seen.elapsed().as_secs() < EXPIRY_SECS);
        }
    }

    fn record(&self, found: &AdvertisingDevice) {
        let Some(adv) = commissionable_adv(found) else {
            return;
        };

        let device = BleDevice {
            id: found.device.id().to_string(),
            name: found.device.name().ok(),
            discriminator: adv.discriminator(),
            vendor_id: adv.vid(),
            product_id: adv.pid(),
            rssi: found.rssi,
            seen_secs_ago: 0,
            last_seen: Instant::now(),
        };

        if !self.devices.borrow().contains_key(&device.id) {
            info!(
                "Bluetooth: {:?} is in commissioning mode (discriminator {}, VID {:#06x}, PID {:#06x})",
                device.name.as_deref().unwrap_or_default(),
                device.discriminator,
                device.vendor_id,
                device.product_id
            );
        }
        self.devices.borrow_mut().insert(device.id.clone(), device);
    }
}

pub struct Connection {
    pub adapter: Adapter,
    pub device: Device,
    /// Written by us (Central → device).
    pub c1: Characteristic,
    /// Indicated by the device (device → Central).
    pub c2: Characteristic,
}

impl Connection {
    /// Scan until a commissionable device matching `filter` advertises, then connect to it.
    pub async fn connect(filter: &CommissionableFilter) -> Result<Self, Error> {
        let adapter = Adapter::default()
            .await
            .ok_or(ErrorCode::NoNetworkInterface)?;
        adapter.wait_available().await.map_err(ble_error)?;

        let service = Uuid::from_u128(MATTER_BLE_SERVICE_UUID);
        let services = [service];
        let device = {
            let mut scan = adapter.scan(&services).await.map_err(ble_error)?;
            loop {
                let found = scan.next().await.ok_or(ErrorCode::NoNetworkInterface)?;
                if let Some(adv) = commissionable_adv(&found).filter(|adv| adv.matches(filter)) {
                    info!(
                        "Found {:?} over Bluetooth (discriminator {}, VID {:#06x}, PID {:#06x})",
                        found.device.name().unwrap_or_default(),
                        adv.discriminator(),
                        adv.vid(),
                        adv.pid()
                    );
                    break found.device;
                }
            }
        };

        adapter.connect_device(&device).await.map_err(ble_error)?;

        let service = device
            .discover_services_with_uuid(service)
            .await
            .map_err(ble_error)?
            .into_iter()
            .next()
            .ok_or(ErrorCode::NotFound)?;
        let characteristic = async |uuid| {
            service
                .discover_characteristics_with_uuid(Uuid::from_u128(uuid))
                .await
                .map_err(ble_error)?
                .into_iter()
                .next()
                .ok_or(Error::from(ErrorCode::NotFound))
        };
        let c1 = characteristic(C1_CHARACTERISTIC_UUID).await?;
        let c2 = characteristic(C2_CHARACTERISTIC_UUID).await?;

        Ok(Self {
            adapter,
            device,
            c1,
            c2,
        })
    }

    /// Run the BTP session with the device until the connection fails.
    pub async fn run(&self, btp: &Btp) -> Result<Infallible, Error> {
        let gatt_mtu =
            Some((self.c1.max_write_len().map_err(ble_error)? + GATT_HEADER_SIZE) as u16);
        let mut buf = [0u8; 512];

        btp.reset();
        btp.set_initiator(true);

        // Write the BTP Handshake Request to C1; the device answers it on C2 once we subscribe.
        self.flush(btp, gatt_mtu, &mut buf).await?;
        let mut indications = self.c2.notify().await.map_err(ble_error)?;

        loop {
            match select(indications.next(), btp.wait_outgoing()).await {
                Either::First(Some(data)) => {
                    btp.process_incoming(gatt_mtu, PEER, &data.map_err(ble_error)?)?
                }
                Either::First(None) => {
                    warn!("Bluetooth device stopped sending indications");
                    return Err(ErrorCode::NoNetworkInterface.into());
                }
                Either::Second(()) => {}
            }

            self.flush(btp, gatt_mtu, &mut buf).await?;
        }
    }

    /// Write every BTP segment that is ready to go out to C1.
    async fn flush(&self, btp: &Btp, gatt_mtu: Option<u16>, buf: &mut [u8]) -> Result<(), Error> {
        loop {
            let len = btp.process_outgoing(gatt_mtu, buf)?;
            if len == 0 {
                return Ok(());
            }
            self.c1.write(&buf[..len]).await.map_err(ble_error)?;
        }
    }

    pub async fn disconnect(self) -> Result<(), Error> {
        self.adapter
            .disconnect_device(&self.device)
            .await
            .map_err(ble_error)
    }
}

/// The Matter commissioning advertisement of `found`, if it has one.
fn commissionable_adv(found: &AdvertisingDevice) -> Option<AdvData> {
    found
        .adv_data
        .service_data
        .get(&Uuid::from_u128(MATTER_BLE_SERVICE_UUID))
        .and_then(|data| AdvData::parse_service_data(data))
}

fn ble_error(e: bluest::Error) -> Error {
    warn!("Bluetooth error: {e}");
    ErrorCode::NoNetworkInterface.into()
}
