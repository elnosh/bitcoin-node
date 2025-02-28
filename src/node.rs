use std::{collections::HashMap, fs, sync::Arc};

use bitcoin::{
    block::Header, consensus, constants::genesis_block, hashes::Hash, params::Params, Block,
    BlockHash, Network,
};
use bitcoinkernel::{ChainType, ChainstateManager, ChainstateManagerOptions, ContextBuilder};
use log::{error, info};
use redb::{Database, ReadOnlyTable, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use tokio::sync::{
    broadcast::{self, Receiver as BroadcastReceiver, Sender as BroadcastSender},
    mpsc::{self, Receiver},
    watch::{Receiver as WatchReceiver, Sender as WatchSender},
    Mutex,
};

use crate::{error::NodeError, network::BitcoinNetwork, p2p::PeerManager};

const HEADER_TIP_KEY: &str = "header_tip";
const HEADER_TIP_TABLE: TableDefinition<&str, i32> = TableDefinition::new("header_tip");
const HEADER_CHAIN_TABLE: TableDefinition<[u8; 32], Vec<u8>> = TableDefinition::new("header_tree");

pub type BlockIndex = (i32, [u8; 32]);
pub type Height = i32;

#[derive(Clone)]
pub enum SyncState {
    IBD(IBDState),
    InSync(BlockIndex),
}

#[derive(Clone)]
pub enum IBDState {
    HeaderSync,
    BlockSync(BlockIndex),
}

pub struct Node {
    db: Database,
    sync_state: SyncState,
    headers_chain: HashMap<BlockHash, BlockHeader>,
    peer_manager: Arc<Mutex<PeerManager>>,
    chain_manager: ChainstateManager,
    block_header_receiver: Receiver<Vec<Header>>,
    block_receiver: Receiver<Block>,
    sync_state_channel: (BroadcastSender<SyncState>, BroadcastReceiver<SyncState>),
    peer_height: Receiver<Height>,
    shutdown_signal: (WatchSender<bool>, WatchReceiver<bool>),
}

#[derive(Serialize, Deserialize, Debug)]
struct BlockHeader {
    block_hash: BlockHash,
    previous_block: BlockHash,
    merkle_root: [u8; 32],
    time: u32,
    nonce: u32,
    next_block: Option<NextBlock>,
    height: Height,
}

#[derive(Serialize, Deserialize, Debug)]
enum NextBlock {
    // Nodes in the chain should only point to one next block
    Chain(BlockHash),
    // if we are at the tip there could be forks and hence multiple next blocks
    Tip(Vec<BlockHash>),
}

impl BlockHeader {
    fn from_bitcoin_header(header: Header, height: Height) -> Self {
        BlockHeader {
            block_hash: header.block_hash(),
            previous_block: header.prev_blockhash,
            merkle_root: *header.merkle_root.as_ref(),
            time: header.time,
            nonce: header.nonce,
            next_block: None,
            height,
        }
    }
}

impl Node {
    // use builder (?) seen throughout many codebases having builders for bigger structs
    pub fn new(
        node_dir: &str,
        network: BitcoinNetwork,
        shutdown_signal: (WatchSender<bool>, WatchReceiver<bool>),
    ) -> Result<Node, NodeError> {
        let data_dir = format!("{}/{}", node_dir, network);
        let blocks_dir = format!("{}/blocks", data_dir);
        info!("data dir {}", data_dir);

        fs::create_dir_all(&data_dir)?;
        let db = Database::create(format!("{}/header", data_dir))?;

        let context = Arc::new(
            ContextBuilder::new()
                .chain_type(ChainType::from(network))
                .build()?,
        );

        let chain_manager_options =
            ChainstateManagerOptions::new(&context, &data_dir, &blocks_dir)?;
        let chain_manager = ChainstateManager::new(chain_manager_options, Arc::clone(&context))?;
        chain_manager.import_blocks()?;

        let read_txn = db.begin_read()?;
        let sync_state = match read_txn.open_table(HEADER_TIP_TABLE) {
            Ok(table) => {
                // if HEADER_TIP_TABLE exists with the tip present it means we finished the
                // headers sync first. We then need to compare to the tip from chain manager to see
                // which blocks we are missing
                match table.get(HEADER_TIP_KEY)? {
                    Some(header) => {
                        let chain_tip = chain_manager.get_block_index_tip();
                        let chain_tip = (chain_tip.height(), chain_tip.block_hash().hash);
                        info!("syncing blocks from height {}", chain_tip.0);

                        // if the tip from the headers sync is greater than tip from chain manager
                        // it means we finished the header sync but didn't finish full block sync
                        if header.value() > chain_tip.0 {
                            SyncState::IBD(IBDState::BlockSync(chain_tip))
                        } else {
                            SyncState::InSync(chain_tip)
                        }
                    }
                    None => SyncState::IBD(IBDState::HeaderSync),
                }
            }

            Err(e) => match e {
                // if this table does not exist, it means we are just starting IBD
                redb::TableError::TableDoesNotExist(_) => SyncState::IBD(IBDState::HeaderSync),
                _ => return Err(NodeError::DatabaseTableError(e)),
            },
        };

        // channels on which to communicate new block info received from a peer
        let (block_header_sender, block_header_receiver) = mpsc::channel(5);
        let (block_sender, block_receiver) = mpsc::channel(10);

        // TODO: better check how using this value in peer
        let block_height = match sync_state {
            SyncState::IBD(IBDState::HeaderSync) => {
                let genesis = chain_manager.get_block_index_genesis();
                (genesis.height(), genesis.block_hash().hash)
            }
            SyncState::IBD(IBDState::BlockSync(height)) => height,
            SyncState::InSync(height) => height,
        };

        // channel to get the height of peer. Received in the version message
        let (peer_height_sender, peer_height_receiver) = mpsc::channel(1);

        let headers_chain = if let SyncState::IBD(IBDState::HeaderSync) = &sync_state {
            info!("node is just starting. will do header synchronization");
            // if just starting, initiate header chain with the genesis block header
            let genesis_block = genesis_block(Params::new(Network::from(network)));
            let genesis_header = BlockHeader::from_bitcoin_header(genesis_block.header, 0);
            let headers_chain = HashMap::from([(genesis_block.block_hash(), genesis_header)]);
            headers_chain
        } else {
            // if in anything other than header sync then we should have a header chain stored in
            // the db
            let read_txn = db.begin_read()?;
            let chain_header_table = read_txn.open_table(HEADER_CHAIN_TABLE)?;
            construct_headers_chain(chain_header_table)?
        };

        let (sync_state_send, sync_state_recv) = broadcast::channel(1);
        let send_2 = sync_state_send.clone();
        let recv_2 = send_2.subscribe();

        let shutdown_recv = shutdown_signal.1.clone();
        let peer_manager = Arc::new(Mutex::new(PeerManager::new(
            sync_state.clone(),
            block_height,
            block_header_sender,
            block_sender,
            (send_2, recv_2),
            peer_height_sender,
            shutdown_recv,
            Network::from(network),
        )));

        Ok(Node {
            db,
            sync_state,
            headers_chain,
            peer_manager,
            chain_manager,
            block_header_receiver,
            block_receiver,
            sync_state_channel: (sync_state_send, sync_state_recv),
            peer_height: peer_height_receiver,
            shutdown_signal,
        })
    }

    fn valid_header(&self, header: Header) -> bool {
        // still TODO:
        // - validate timestamps (inside 2 hour range)
        // - validate difficulty adjustments
        // - validate height

        // check that previous block points to a previously seen one
        if !self.headers_chain.contains_key(&header.prev_blockhash) {
            return false;
        }

        // validate the pow
        if let Err(_) = header.validate_pow(header.target()) {
            return false;
        }

        true
    }

    pub async fn run(mut self) {
        let peer_manager = Arc::clone(&self.peer_manager);
        tokio::spawn(async move {
            peer_manager.lock().await.run().await;
        });

        // if doing header sync, wait to get the height from version msg from peer
        let best_height = match self.sync_state {
            SyncState::IBD(IBDState::HeaderSync) => self.peer_height.recv().await.unwrap(),
            _ => 0,
        };

        let progress_points = vec![
            (best_height as f64 * 0.16) as i32,
            (best_height as f64 * 0.33) as i32,
            (best_height as f64 * 0.5) as i32,
            (best_height as f64 * 0.66) as i32,
            (best_height as f64 * 0.83) as i32,
            (best_height as f64 * 0.99) as i32,
        ];
        let mut current_progress_idx = 0;

        // when this reaches best_height, we are done with header sync
        let mut header_sync_height = 0;
        // receive either block headers or blocks from peer and process them
        loop {
            tokio::select! {
                _ = self.shutdown_signal.1.changed() => {
                    break
                }
                block_headers = self.block_header_receiver.recv() => {
                    let block_headers = match block_headers {
                        Some(b) => b,
                        None => {
                            error!("terminating... channel closed");
                            break;
                        }
                    };

                    for block_header in block_headers {
                        if self.valid_header(block_header) {
                            // if header is valid, add to header chain map
                            let previous_height = self.headers_chain.get(&block_header.prev_blockhash).unwrap().height;
                            let new_block_header = BlockHeader::from_bitcoin_header(block_header, previous_height + 1);
                            self.headers_chain.insert(block_header.block_hash(), new_block_header);
                            header_sync_height += 1;
                            // TODO: change previous block header entry to have a next block

                            if current_progress_idx < progress_points.len() {
                                if header_sync_height >= progress_points[current_progress_idx] {
                                    info!("verifying headers - progress {}%", (current_progress_idx as f64 / progress_points.len() as f64 * 100.0) as u32);
                                    current_progress_idx += 1;
                                }
                            }
                        }
                    }

                    // we will only send on this channel when the entire header sync is done.
                    // for now do not send intermediate progress for header sync
                    if header_sync_height == best_height {
                        info!("finished with headers sync at height {}. Will start full block sync.", header_sync_height);
                        let genesis_index = self.chain_manager.get_block_index_genesis();
                        // send new sync state. We are done with header sync and want to start full
                        // block sync from genesis block
                        if let Err(_) = self
                            .sync_state_channel.0
                            .send(SyncState::IBD(IBDState::BlockSync((
                                genesis_index.height(),
                                genesis_index.block_hash().hash,
                            ))))
                        {
                            error!("could not send new sync state.");
                        };


                        self.save_headers_chain(header_sync_height).unwrap();
                        info!("saved headers chain to db");
                    };
                }
                block = self.block_receiver.recv() => {
                    let block = match block {
                        Some(b) => b,
                        None => {
                            error!("terminating... channel closed");
                            break;
                        }
                    };

                    let raw_block = consensus::encode::serialize(&block);
                    let kernel_block = match bitcoinkernel::Block::try_from(raw_block.as_slice()) {
                        Ok(b) => b,
                        Err(err) => {
                            error!("invalid block {:?} ignoring for now", err);
                            continue
                        },
                    };

                    // NOTE: according to comments in code from lib, first bool is
                    // if the block was valid and 2nd is if it's a new block.
                    // if valid and not a duplicate, update last block/height in peer
                    let (valid, new) = self.chain_manager.process_block(&kernel_block);
                    if valid && new {
                        let tip_idx = self.chain_manager.get_block_index_tip();
                        info!("received new valid block. Updating to new tip height {} in peer", tip_idx.height());

                        if let Err(_) = self
                            .sync_state_channel.0
                            .send(SyncState::InSync((
                                tip_idx.height(),
                                tip_idx.block_hash().hash,
                            )))
                        {
                            error!("could not send new tip on channel to peer");
                        };
                    }
                }
            }
        }
    }

    // save current state of headers chain to db along with the tip
    fn save_headers_chain(&self, height: i32) -> Result<(), NodeError> {
        let write_txn = self.db.begin_write()?;
        {
            let mut headers_chain_table = write_txn.open_table(HEADER_CHAIN_TABLE)?;
            for (key, value) in &self.headers_chain {
                let serialized_header = serde_json::to_vec(value)?;
                headers_chain_table.insert(key.as_ref(), &serialized_header)?;
            }
        }
        write_txn.commit()?;

        let write_tip_txn = self.db.begin_write()?;
        {
            let mut header_tip_table = write_tip_txn.open_table(HEADER_TIP_TABLE)?;
            header_tip_table.insert(HEADER_TIP_KEY, height)?;
        }
        write_tip_txn.commit()?;

        Ok(())
    }
}

// construct headers chain from db
fn construct_headers_chain(
    header_chain_table: ReadOnlyTable<[u8; 32], Vec<u8>>,
) -> Result<HashMap<BlockHash, BlockHeader>, NodeError> {
    let table_iter = header_chain_table.iter()?;
    let mut header_chain = HashMap::new();

    for result in table_iter {
        let (key, value) = result?;
        let block_hash = BlockHash::from_byte_array(key.value());
        let deserialized_header: BlockHeader = serde_json::from_slice(value.value().as_slice())?;
        header_chain.insert(block_hash, deserialized_header);
    }

    Ok(header_chain)
}
