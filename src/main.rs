use std::sync::{Arc, Once};

use bitcoinkernel::{ChainstateManager, ChainstateManagerOptions, ContextBuilder, Log, Logger};
use crossbeam_channel::bounded;
use log::info;
use p2p::PeerManager;

mod error;
mod p2p;

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

fn main() {
    START.call_once(|| {
        setup_logging();
    });

    let context = Arc::new(
        ContextBuilder::new()
            .chain_type(bitcoinkernel::ChainType::REGTEST)
            .build()
            .expect("unable to set context"),
    );

    let data_dir = ".bitcoin-node";
    let blocks_dir = format!("{}/blocks", data_dir);

    let chain_manager_options =
        ChainstateManagerOptions::new(&context, &data_dir, &blocks_dir).unwrap();
    let chain_manager =
        ChainstateManager::new(chain_manager_options, Arc::clone(&context)).unwrap();

    chain_manager.import_blocks().unwrap();

    let (shutdown_send, shutdown_receive) = bounded(1);

    ctrlc::set_handler(move || shutdown_send.send(true).unwrap()).unwrap();

    let peer_mngr = PeerManager::new(chain_manager, shutdown_receive, bitcoin::Network::Regtest);
    peer_mngr.run();
}
