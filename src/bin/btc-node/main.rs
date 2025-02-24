use std::{process, sync::Once};

use bitcoin_node::{network::BitcoinNetwork, node::Node};
use bitcoinkernel::{Log, Logger};
use clap::Parser;
use crossbeam_channel::bounded;
use log::info;

struct KernelLog {}

impl Log for KernelLog {
    fn log(&self, message: &str) {
        info!(
            target: "bitcoinkernel", 
            "{}", message.strip_suffix("\r\n").or_else(|| message.strip_suffix('\n')).unwrap_or(message));
    }
}

static START: Once = Once::new();
static mut GLOBAL_LOG_CALLBACK_HOLDER: Option<Logger<KernelLog>> = None;

fn setup_logging() {
    let mut builder = env_logger::Builder::from_default_env();
    builder.filter(None, log::LevelFilter::Debug).init();

    unsafe { GLOBAL_LOG_CALLBACK_HOLDER = Some(Logger::new(KernelLog {}).unwrap()) };
}

#[derive(Parser)]
#[command()]
struct Cli {
    #[arg(short, long, default_value = "regtest", value_enum)]
    network: BitcoinNetwork,
}

#[tokio::main]
async fn main() {
    START.call_once(|| {
        setup_logging();
    });

    let cli = Cli::parse();

    let (shutdown_send, shutdown_receive) = bounded(1);
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        shutdown_send.send(true).unwrap();
    });

    let node = Node::new(".bitcoin-node", cli.network, shutdown_receive).unwrap();
    node.run().await;
    process::exit(0);
}
