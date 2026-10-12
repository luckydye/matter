//! One entry per physical device, merged from its mDNS records and its Bluetooth advertisement.

use serde::Serialize;

use crate::ble::{BleDevice, BleScanner};
use crate::discovery::{DiscoveredNode, Discovery, ServiceKind};

#[derive(Debug, Clone, Default, Serialize)]
pub struct Device {
    /// Its `_matterc._udp` record, while it advertises one (TXT `CM` tells whether it is in commissioning mode).
    pub commissionable: Option<DiscoveredNode>,
    /// Its `_matter._tcp` records, one per fabric it is commissioned onto.
    pub operational: Vec<DiscoveredNode>,
    /// Its Bluetooth advertisement, while it is in commissioning mode.
    pub bluetooth: Option<BleDevice>,
}

impl Device {
    /// Whether `node` is announced from one of the addresses of this device's mDNS records.
    pub fn shares_address(&self, node: &DiscoveredNode) -> bool {
        self.commissionable
            .iter()
            .chain(&self.operational)
            .any(|record| {
                record
                    .addresses
                    .iter()
                    .any(|ip| node.addresses.contains(ip))
            })
    }

    /// Whether `ble` advertises the discriminator and vendor/product of this device's commissionable record.
    pub fn advertises(&self, ble: &BleDevice) -> bool {
        let Some(node) = &self.commissionable else {
            return false;
        };
        let vendor = ble.vendor_id.to_string();
        let vendor_product = format!("{vendor}+{}", ble.product_id);

        node.txt.get("D") == Some(&ble.discriminator.to_string())
            // `VP` is optional, and may carry the vendor only.
            && node
                .txt
                .get("VP")
                .is_none_or(|vp| *vp == vendor || *vp == vendor_product)
    }
}

pub fn devices(discovery: &Discovery, ble: &BleScanner) -> Vec<Device> {
    let mut devices: Vec<Device> = Vec::new();

    let (commissionable, operational): (Vec<_>, Vec<_>) = discovery
        .nodes()
        .into_iter()
        .partition(|node| node.kind == ServiceKind::Commissionable);
    for node in commissionable.into_iter().chain(operational) {
        let index = devices
            .iter()
            .position(|device| device.shares_address(&node))
            .unwrap_or_else(|| {
                devices.push(Device::default());
                devices.len() - 1
            });
        match node.kind {
            ServiceKind::Commissionable => devices[index].commissionable = Some(node),
            ServiceKind::Operational => devices[index].operational.push(node),
        }
    }

    for advertisement in ble.devices() {
        match devices
            .iter_mut()
            .find(|device| device.advertises(&advertisement))
        {
            Some(device) => device.bluetooth = Some(advertisement),
            None => devices.push(Device {
                bluetooth: Some(advertisement),
                ..Device::default()
            }),
        }
    }

    devices
}
