//! A standalone Matter controller: discovers Matter nodes over mDNS and
//! commissions them onto its own fabric, driven over an HTTP JSON API.
//!
//! Usage: `matter-server [--listen 127.0.0.1:5580] [--interface en0] [--storage ./matter-storage]`

use core::pin::pin;

use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;

use async_executor::LocalExecutor;
use embassy_futures::select::{select4, Either4};
use static_cell::StaticCell;

use rs_matter::cert::{MAX_CERT_TLV_AND_ASN1_LEN, MAX_CERT_TLV_LEN};
use rs_matter::crypto::default_crypto;
use rs_matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::error::Error;
use rs_matter::onboard::noc::NocGenerator;
use rs_matter::persist::DirKvBlobStore;
use rs_matter::transport::network::btp::Btp;
use rs_matter::transport::network::{Address, ChainedNetwork, NoNetwork};
use rs_matter::utils::init::InitMaybeUninit;
use rs_matter::Matter;

use crate::ble::BleScanner;
use crate::controller::{load_or_create_fabric, Controller};
use crate::discovery::{Discovery, Interface};

mod ble;
mod control;
mod controller;
mod devices;
mod discovery;
mod http;

static MATTER: StaticCell<Matter> = StaticCell::new();

pub struct Args {
    pub listen: SocketAddr,
    pub interface: Option<String>,
    pub storage: PathBuf,
}

fn parse_args() -> Args {
    let mut args = Args {
        listen: "127.0.0.1:5580".parse().unwrap(),
        interface: None,
        storage: PathBuf::from("matter-storage"),
    };

    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--listen" => args.listen = value.parse().expect("--listen must be ip:port"),
            "--interface" => args.interface = Some(value),
            "--storage" => args.storage = PathBuf::from(value),
            _ => panic!("unknown argument {flag}"),
        }
    }

    args
}

fn main() -> Result<(), Error> {
    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
    );

    // bluest's Linux backend (bluer) needs a Tokio runtime; its worker threads drive bluer's tasks.
    #[cfg(target_os = "linux")]
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    #[cfg(target_os = "linux")]
    let _tokio = tokio.enter();

    futures_lite::future::block_on(run(parse_args()))
}

async fn run(args: Args) -> Result<(), Error> {
    std::fs::create_dir_all(&args.storage)?;

    // The controller's own (non-commissionable) node identity; only the fabric added below matters.
    let matter =
        &*MATTER
            .uninit()
            .init_with(Matter::init(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, 0));
    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let kv = matter.kv(DirKvBlobStore::new(args.storage));
    matter.startup(&kv)?;

    let (fab_idx, icac_key) = load_or_create_fabric(matter, &crypto, &kv)?;

    let discovery = Discovery::new(Interface::select(args.interface.as_deref())?);
    let btp = Btp::new();
    let ble = BleScanner::new();

    let mut noc_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut noc_generator = NocGenerator::new(matter, icac_key.reference(), fab_idx, &mut noc_buf)?;
    let mut commissioner_buf = [0u8; MAX_CERT_TLV_LEN];
    let controller = Controller::new(
        matter,
        &crypto,
        &discovery,
        &btp,
        &kv,
        fab_idx,
        &mut noc_generator,
        &mut commissioner_buf,
    )?;

    let socket = async_io::Async::<UdpSocket>::bind(SocketAddr::from(([0u16; 8], 0)))?;

    let executor = LocalExecutor::new();
    // `Address::Btp` packets go to the Bluetooth device being commissioned, everything else over UDP.
    let transport = pin!(matter.run(
        &crypto,
        ChainedNetwork::new(Address::is_btp, &btp, &socket),
        ChainedNetwork::new(Address::is_btp, &btp, &socket),
        NoNetwork,
    ));
    let mdns = pin!(discovery.run(matter));
    let ble_scan = pin!(ble.run());
    let http = pin!(http::serve(
        args.listen,
        &executor,
        &discovery,
        &ble,
        &controller
    ));

    executor
        .run(async {
            match select4(transport, mdns, ble_scan, http).await {
                Either4::First(r) | Either4::Second(r) | Either4::Third(r) | Either4::Fourth(r) => {
                    r
                }
            }
        })
        .await
}
