//! Continuous mDNS discovery of commissionable (`_matterc._udp`) and
//! operational (`_matter._tcp`) Matter nodes.
//!
//! Queries are built and responses parsed with rs-matter's builtin mDNS
//! primitives. The same loop also answers rs-matter's operational resolve
//! requests (used by CASE during commissioning), so no second mDNS stack runs.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};
use std::str::FromStr;
use std::time::Instant;

use async_io::Async;
use domain::base::Name;
use embassy_futures::select::{select4, Either4};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Ticker, Timer};
use log::{info, warn};
use serde::Serialize;
use socket2::{Domain, Protocol, Socket, Type};

use rs_matter::error::{Error, ErrorCode};
use rs_matter::transport::network::mdns::builtin::{
    build_browse_query, build_resolve_query, parse_into_answer,
};
use rs_matter::transport::network::mdns::{
    score_ip_address, CommissionableFilter, DottedName, MdnsRemoteService,
    MDNS_IPV6_BROADCAST_ADDR, MDNS_PORT,
};
use rs_matter::transport::network::IpAddr;
use rs_matter::Matter;

const COMMISSIONABLE_SUFFIX: &str = "._matterc._udp.local";
const OPERATIONAL_SUFFIX: &str = "._matter._tcp.local";

/// How often the browse queries are re-sent.
const QUERY_INTERVAL: Duration = Duration::from_secs(30);
/// Nodes not heard from for this long are dropped from the registry.
pub const EXPIRY_SECS: u64 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    Commissionable,
    Operational,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredNode {
    /// Full mDNS instance name, e.g. `ABCD1234._matterc._udp.local`.
    pub instance: String,
    pub kind: ServiceKind,
    pub port: Option<u16>,
    pub addresses: Vec<IpAddr>,
    /// Raw TXT records (`D` discriminator, `VP` vendor+product, `CM` commissioning mode, `DN` name, ...).
    pub txt: BTreeMap<String, String>,
    pub seen_secs_ago: u64,
    #[serde(skip)]
    pub last_seen: Instant,
}

impl DiscoveredNode {
    /// The preferred UDP address of this node, scoped to `interface_index` if link-local.
    pub fn socket_addr(&self, interface_index: u32) -> Option<SocketAddr> {
        let port = self.port?;
        let ip = *self
            .addresses
            .iter()
            .max_by_key(|ip| score_ip_address(ip))?;

        Some(match ip {
            IpAddr::V6(ip) => {
                let scope = if ip.is_unicast_link_local() {
                    interface_index
                } else {
                    0
                };
                SocketAddr::V6(SocketAddrV6::new(ip, port, 0, scope))
            }
            IpAddr::V4(ip) => SocketAddr::new(IpAddr::V4(ip), port),
        })
    }

    /// This node as rs-matter's view of a resolved mDNS service; `scope_id` scopes link-local addresses.
    pub fn remote_service(
        &self,
        scope_id: u32,
    ) -> MdnsRemoteService<
        DottedName<'_>,
        impl Iterator<Item = IpAddr> + Clone + '_,
        impl Iterator<Item = (&str, &str)> + Clone + '_,
    > {
        MdnsRemoteService {
            instance_name: DottedName(&self.instance),
            port: self.port,
            addrs: self.addresses.iter().copied(),
            txt: self.txt.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            scope_id,
        }
    }

    fn matches(&self, filter: &CommissionableFilter) -> bool {
        filter.matches(&self.remote_service(0))
    }
}

/// The network interface mDNS runs on.
#[derive(Debug, Clone)]
pub struct Interface {
    pub name: String,
    pub index: u32,
    pub ipv6: Vec<Ipv6Addr>,
}

impl Interface {
    /// Pick the named interface, or (when `name` is `None`) the first
    /// non-loopback interface with a link-local IPv6 address and an IPv4
    /// address. Requiring IPv4 skips point-to-point and peer-to-peer
    /// interfaces (`utun*`, `awdl0`, `llw0`), which only carry link-local IPv6.
    pub fn select(name: Option<&str>) -> Result<Self, Error> {
        let all = if_addrs::get_if_addrs()?;
        let has_ipv4 = |iname: &str| {
            all.iter()
                .any(|ia| ia.name == iname && matches!(ia.addr, if_addrs::IfAddr::V4(_)))
        };
        // `is_loopback` checks the address, and `lo0` also carries the link-local `fe80::1`.
        let is_loopback = |iname: &str| all.iter().any(|ia| ia.name == iname && ia.is_loopback());

        let chosen = all
            .iter()
            .filter(|ia| !is_loopback(&ia.name))
            .find(|ia| match (name, &ia.addr) {
                (Some(name), _) => ia.name == name,
                (None, if_addrs::IfAddr::V6(v6)) => {
                    v6.ip.is_unicast_link_local() && has_ipv4(&ia.name)
                }
                (None, _) => false,
            })
            .ok_or(ErrorCode::NoNetworkInterface)?;

        let ipv6 = all
            .iter()
            .filter(|ia| ia.name == chosen.name)
            .filter_map(|ia| match &ia.addr {
                if_addrs::IfAddr::V6(v6) => Some(v6.ip),
                _ => None,
            })
            .collect();

        Ok(Self {
            name: chosen.name.clone(),
            index: chosen.index.ok_or(ErrorCode::NoNetworkInterface)?,
            ipv6,
        })
    }
}

pub struct Discovery {
    pub interface: Interface,
    nodes: RefCell<BTreeMap<String, DiscoveredNode>>,
    /// Signalled to send the browse queries right away instead of at the next interval.
    pub browse_now: Signal<NoopRawMutex, ()>,
}

impl Discovery {
    pub fn new(interface: Interface) -> Self {
        Self {
            interface,
            nodes: RefCell::new(BTreeMap::new()),
            browse_now: Signal::new(),
        }
    }

    pub fn nodes(&self) -> Vec<DiscoveredNode> {
        let now = Instant::now();

        self.nodes
            .borrow()
            .values()
            .map(|node| DiscoveredNode {
                seen_secs_ago: now.duration_since(node.last_seen).as_secs(),
                ..node.clone()
            })
            .collect()
    }

    pub fn commissionable(&self, filter: &CommissionableFilter) -> Vec<DiscoveredNode> {
        self.nodes()
            .into_iter()
            .filter(|node| node.kind == ServiceKind::Commissionable && node.matches(filter))
            .collect()
    }

    pub fn find(&self, instance: &str) -> Option<DiscoveredNode> {
        let instance = instance.trim_end_matches('.');

        self.nodes()
            .into_iter()
            .find(|node| node.instance.eq_ignore_ascii_case(instance))
    }

    pub fn operational(&self, compressed_fabric_id: u64, node_id: u64) -> Option<DiscoveredNode> {
        self.find(&format!(
            "{compressed_fabric_id:016X}-{node_id:016X}{OPERATIONAL_SUFFIX}"
        ))
    }

    /// Wait until the node announces itself on our fabric. Browses every few seconds:
    /// a device that has just joined Wi-Fi often misses the first (multicast) queries.
    pub async fn wait_operational(&self, compressed_fabric_id: u64, node_id: u64) {
        while self.operational(compressed_fabric_id, node_id).is_none() {
            self.browse_now.signal(());
            Timer::after(Duration::from_secs(2)).await;
        }
    }

    pub async fn run(&self, matter: &Matter<'_>) -> Result<(), Error> {
        let socket = self.bind()?;
        let mut ticker = Ticker::every(QUERY_INTERVAL);
        let mut buf = [0u8; 9000];

        info!(
            "mDNS discovery running on {} (index {})",
            self.interface.name, self.interface.index
        );

        self.browse(&socket).await?;

        loop {
            match select4(
                socket.recv_from(&mut buf),
                ticker.next(),
                matter.transport().wait_mdns_resolve_request(),
                self.browse_now.wait(),
            )
            .await
            {
                Either4::First(received) => {
                    let (len, _) = received?;
                    self.handle_response(&socket, matter, &buf[..len]).await?;
                }
                Either4::Second(()) => {
                    self.expire();
                    self.browse(&socket).await?;
                }
                Either4::Third(service) => {
                    let mut instance = heapless::String::<128>::new();
                    service.instance_name(&mut instance);

                    // Wi-Fi devices often miss multicast queries, so answer from the registry when the node is known.
                    if let Some(node) = self.find(&instance) {
                        matter.transport().try_deposit_mdns_resolve(
                            &node.remote_service(self.interface.index),
                            &self.interface.ipv6,
                        );
                    }

                    let name =
                        Name::<Vec<u8>>::from_str(&instance).map_err(|_| ErrorCode::InvalidData)?;
                    let mut query = [0u8; 512];
                    let len = build_resolve_query(name, &mut query)?;
                    self.send(&socket, &query[..len]).await?;
                }
                Either4::Fourth(()) => self.browse(&socket).await?,
            }
        }
    }

    fn bind(&self) -> Result<Async<UdpSocket>, Error> {
        let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        // The OS mDNS daemon (mDNSResponder / avahi) usually holds port 5353 already.
        socket.set_reuse_address(true)?;
        socket.set_reuse_port(true)?;
        socket.set_only_v6(true)?;
        socket.set_multicast_if_v6(self.interface.index)?;
        socket.bind(&SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), MDNS_PORT).into())?;
        socket.join_multicast_v6(&MDNS_IPV6_BROADCAST_ADDR, self.interface.index)?;

        Ok(Async::new(UdpSocket::from(socket))?)
    }

    async fn browse(&self, socket: &Async<UdpSocket>) -> Result<(), Error> {
        for service in [&COMMISSIONABLE_SUFFIX[1..], &OPERATIONAL_SUFFIX[1..]] {
            let name = Name::<Vec<u8>>::from_str(service).map_err(|_| ErrorCode::InvalidData)?;
            let mut query = [0u8; 512];
            let len = build_browse_query(name, &mut query)?;
            self.send(socket, &query[..len]).await?;
        }

        Ok(())
    }

    async fn send(&self, socket: &Async<UdpSocket>, data: &[u8]) -> Result<(), Error> {
        let group = SocketAddrV6::new(MDNS_IPV6_BROADCAST_ADDR, MDNS_PORT, 0, self.interface.index);
        socket.send_to(data, SocketAddr::V6(group)).await?;

        Ok(())
    }

    async fn handle_response(
        &self,
        socket: &Async<UdpSocket>,
        matter: &Matter<'_>,
        data: &[u8],
    ) -> Result<(), Error> {
        let answer = match parse_into_answer(data, Some(self.interface.index)) {
            Ok(Some(answer)) => answer,
            Ok(None) => return Ok(()),
            Err(e) => {
                warn!("Ignoring malformed mDNS packet: {e:?}");
                return Ok(());
            }
        };

        matter
            .transport()
            .try_deposit_mdns_resolve(&answer, &self.interface.ipv6);

        let instance = answer.instance_name.to_string();
        let instance = instance.trim_end_matches('.');
        let kind = if instance.ends_with(COMMISSIONABLE_SUFFIX) {
            ServiceKind::Commissionable
        } else if instance.ends_with(OPERATIONAL_SUFFIX) {
            ServiceKind::Operational
        } else {
            return Ok(());
        };

        let resolved = {
            let mut nodes = self.nodes.borrow_mut();
            let node = nodes
                .entry(instance.to_string())
                .or_insert_with(|| DiscoveredNode {
                    instance: instance.to_string(),
                    kind,
                    port: None,
                    addresses: Vec::new(),
                    txt: BTreeMap::new(),
                    seen_secs_ago: 0,
                    last_seen: Instant::now(),
                });

            // A response packet may carry only a subset of the records, so only overwrite what it has.
            node.port = answer.port.or(node.port);
            let addresses: Vec<_> = answer.addrs.collect();
            if !addresses.is_empty() {
                node.addresses = addresses;
            }
            let txt: BTreeMap<_, _> = answer
                .txt
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            if !txt.is_empty() {
                node.txt = txt;
            }
            node.last_seen = Instant::now();

            node.port.is_some() && !node.addresses.is_empty()
        };

        if !resolved {
            // A bare PTR answer: ask the node for its SRV/TXT/AAAA records.
            let mut query = [0u8; 512];
            let len = build_resolve_query(answer.instance_name, &mut query)?;
            self.send(socket, &query[..len]).await?;
        }

        Ok(())
    }

    fn expire(&self) {
        self.nodes
            .borrow_mut()
            .retain(|_, node| node.last_seen.elapsed().as_secs() < EXPIRY_SECS);
    }
}
