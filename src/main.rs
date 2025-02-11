use std::sync::Arc;

use bitcoinkernel::{
    BlockManagerOptions, ChainstateLoadOptions, ChainstateManager, ChainstateManagerOptions,
    ContextBuilder, ValidationInterfaceCallbacks,
};
use log::{error, info};
use p2p::PeerManager;

mod error;
mod p2p;

fn main() {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();

    let block_check_callback = ValidationInterfaceCallbacks {
        block_checked: Box::new(move |_block, mode, result| match mode {
            bitcoinkernel::ValidationMode::VALID => {
                info!("VALID BLOCK")
            }
            bitcoinkernel::ValidationMode::ERROR => {
                error!("ERROR VALIDATING BLOCK")
            }
            bitcoinkernel::ValidationMode::INVALID => {
                error!("INVALID BLOCK.");
                match result {
                    bitcoinkernel::BlockValidationResult::RESULT_UNSET => error!("result unset"),
                    bitcoinkernel::BlockValidationResult::CONSENSUS => error!("consensus"),
                    bitcoinkernel::BlockValidationResult::CACHED_INVALID => {
                        error!("cached invalid")
                    }
                    bitcoinkernel::BlockValidationResult::INVALID_HEADER => {
                        error!("invalid header")
                    }
                    bitcoinkernel::BlockValidationResult::MUTATED => error!("mutated"),
                    bitcoinkernel::BlockValidationResult::MISSING_PREV => error!("missing prev"),
                    bitcoinkernel::BlockValidationResult::INVALID_PREV => error!("invalid prev"),
                    bitcoinkernel::BlockValidationResult::TIME_FUTURE => error!("time future"),
                    bitcoinkernel::BlockValidationResult::CHECKPOINT => error!("checkpoint"),
                    bitcoinkernel::BlockValidationResult::HEADER_LOW_WORK => {
                        error!("header low work")
                    }
                }
            }
        }),
    };

    let context = ContextBuilder::new()
        .chain_type(bitcoinkernel::ChainType::REGTEST)
        //.validation_interface(Box::new(block_check_callback))
        .build()
        .expect("unable to set context");

    let data_dir = ".bitcoin-node";
    let blocks_dir = format!("{}/blocks", data_dir);

    let chain_manager_options = ChainstateManagerOptions::new(&context, &data_dir).unwrap();
    let block_manager_options = BlockManagerOptions::new(&context, &data_dir, &blocks_dir).unwrap();

    let chain_manager = ChainstateManager::new(
        chain_manager_options,
        block_manager_options,
        ChainstateLoadOptions::new(),
        Arc::new(context),
    )
    .expect("unable to set chain manager");

    chain_manager.import_blocks().unwrap();

    let tip = chain_manager.get_block_index_tip();
    info!("tip height {:?}", tip.height());
    info!("tip {:?}", tip.block_hash().hash);

    let peer_mngr = PeerManager::new(chain_manager, bitcoin::Network::Regtest);
    peer_mngr.run()
}
