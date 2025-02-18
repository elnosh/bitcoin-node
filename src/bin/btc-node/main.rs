use std::{
    process,
    sync::{Arc, Once},
};

use bitcoin::Network;
use bitcoin_node::{network::BitcoinNetwork, p2p::PeerManager};
use bitcoinkernel::{
    ChainType, ChainstateManager, ChainstateManagerOptions, ContextBuilder, Log, Logger,
};
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
    let data_dir = format!(".bitcoin-node/{}", cli.network);
    let blocks_dir = format!("{}/blocks", data_dir);

    let context = Arc::new(
        ContextBuilder::new()
            .chain_type(ChainType::from(cli.network))
            .build()
            .expect("unable to set context"),
    );

    let chain_manager_options =
        ChainstateManagerOptions::new(&context, &data_dir, &blocks_dir).unwrap();
    let chain_manager =
        ChainstateManager::new(chain_manager_options, Arc::clone(&context)).unwrap();
    chain_manager.import_blocks().unwrap();

    let (shutdown_send, shutdown_receive) = bounded(1);
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        shutdown_send.send(true).unwrap();
    });

    let peer_mngr = PeerManager::new(chain_manager, shutdown_receive, Network::from(cli.network));
    peer_mngr.run().await;
    process::exit(0);
}
