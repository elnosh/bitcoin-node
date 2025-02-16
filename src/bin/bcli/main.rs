use std::sync::Arc;

use bitcoin::{consensus, Block};
use bitcoin_node::{self, network::BitcoinNetwork};
use bitcoinkernel::{ChainType, ChainstateManager, ChainstateManagerOptions, ContextBuilder};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command()]
struct Cli {
    #[arg(short, long, default_value = "regtest", value_enum)]
    network: BitcoinNetwork,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Getblockcount,
    Getblock { height: i32 },
}

fn main() {
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

    match cli.command {
        Commands::Getblockcount => {
            let tip = chain_manager.get_block_index_tip();
            println!("{}", tip.height())
        }
        Commands::Getblock { height } => {
            let index = chain_manager.get_block_index_by_height(height).unwrap();
            let block: Vec<u8> = chain_manager.read_block_data(&index).unwrap().into();
            let block: Block = consensus::deserialize(&block).unwrap();

            println!("{}", serde_json::to_string_pretty(&block).unwrap());
        }
    }
}
