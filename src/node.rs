use std::{fs, sync::Arc};

use bitcoin::{
    consensus,
    hashes::Hash,
    p2p::{
        message::{NetworkMessage, RawNetworkMessage},
        message_blockdata::{GetHeadersMessage, Inventory},
    },
    BlockHash, Network,
};
use bitcoinkernel::{ChainType, ChainstateManager, ChainstateManagerOptions, ContextBuilder};
use log::info;
use serde::{Deserialize, Serialize};
use tokio::sync::{
    broadcast::{self, Receiver, Sender},
    watch::{Receiver as WatchReceiver, Sender as WatchSender},
    Mutex,
};

use crate::{
    error::NodeError,
    header_chain::HeadersChain,
    network::BitcoinNetwork,
    p2p::{build_raw_network_msg, PeerManager},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockIndex(pub i32, pub [u8; 32]);

pub type Height = i32;

#[derive(Clone)]
pub enum SyncState {
    IBD(IBDState),
    InSync(BlockIndex),
}

#[derive(Clone)]
pub enum IBDState {
    HeaderSync(HeaderSyncState),
    BlockSync(BlockIndex),
}

#[derive(Clone, Default)]
pub struct HeaderSyncState {
    target_height: i32,
    current_sync_height: i32,
}

pub struct Node {
    sync_state: SyncState,
    headers_chain: HeadersChain,
    peer_manager: Arc<Mutex<PeerManager>>,
    chain_manager: ChainstateManager,
    // channel on which to send raw message that Peer struct should send on the connection
    // and receive messages that were read from the connection
    tcp_conn_channel: (Sender<RawNetworkMessage>, Receiver<RawNetworkMessage>),
    shutdown_signal: (WatchSender<bool>, WatchReceiver<bool>),
    network: Network,
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

        let context = Arc::new(
            ContextBuilder::new()
                .chain_type(ChainType::from(network))
                .build()?,
        );

        let chain_manager_options =
            ChainstateManagerOptions::new(&context, &data_dir, &blocks_dir)?;
        let chain_manager = ChainstateManager::new(chain_manager_options, Arc::clone(&context))?;
        chain_manager.import_blocks()?;

        let headers_chain = HeadersChain::new(&data_dir, network.into())?;
        let headers_chain_tip = headers_chain.tip();

        // if tip height is 0, it means we are just starting IBD and we need to
        // start with headers synchronization
        let sync_state = if headers_chain_tip.0 == 0 {
            SyncState::IBD(IBDState::HeaderSync(HeaderSyncState::default()))
        } else {
            // if we are done with headers sync first, we then need to compare to the
            // tip from chain manager to see which blocks we are missing
            let chain_tip = chain_manager.get_block_index_tip();
            let chain_tip = BlockIndex(chain_tip.height(), chain_tip.block_hash().hash);
            info!("syncing blocks from height {}", chain_tip.0);

            // if the tip from the headers sync is greater than tip from chain manager
            // it means we finished the header sync but didn't finish full block sync
            if headers_chain_tip.0 > chain_tip.0 {
                SyncState::IBD(IBDState::BlockSync(headers_chain_tip))
            } else {
                SyncState::InSync(chain_tip)
            }
        };

        let block_height = match &sync_state {
            SyncState::IBD(IBDState::HeaderSync(_)) => {
                let genesis = chain_manager.get_block_index_genesis();
                BlockIndex(genesis.height(), genesis.block_hash().hash)
            }
            SyncState::IBD(IBDState::BlockSync(height)) => height.clone(),
            SyncState::InSync(height) => height.clone(),
        };

        let shutdown_recv = shutdown_signal.1.clone();

        // channel sender for peer manager to communicate messages that it received on the
        // tcp connection and send back to node
        let (peer_manager_sender, msg_processing_receiver) = broadcast::channel(500);

        // sender for node to communicate to peer manager messages it wants written on the tcp
        // connection
        let (tcp_conn_sender, tcp_conn_receiver) = broadcast::channel(32);

        let network = Network::from(network);
        let peer_manager = Arc::new(Mutex::new(PeerManager::new(
            block_height,
            (tcp_conn_sender.clone(), tcp_conn_receiver),
            peer_manager_sender,
            shutdown_recv,
            network,
        )));

        Ok(Node {
            sync_state,
            headers_chain,
            peer_manager,
            chain_manager,
            tcp_conn_channel: (tcp_conn_sender, msg_processing_receiver),
            shutdown_signal,
            network,
        })
    }

    pub async fn run(mut self) {
        let peer_manager = Arc::clone(&self.peer_manager);
        tokio::spawn(async move {
            peer_manager.lock().await.run().await;
        });

        // the first message the peer struct will send in this channel will be
        // the version since the handshake is done first
        let best_height = match self.tcp_conn_channel.1.recv().await.unwrap().payload() {
            NetworkMessage::Version(version) => version.start_height,
            _ => 0,
        };

        let tcp_sender_clone = self.tcp_conn_channel.0.clone();
        match self.sync_state {
            SyncState::IBD(IBDState::HeaderSync(_)) => {
                let block_hash = BlockHash::from_byte_array(self.headers_chain.tip().1);
                let msg_to_send = self.build_get_headers_msg(vec![block_hash]).unwrap();
                tcp_sender_clone.send(msg_to_send.clone()).unwrap();

                info!("got best height {} from version message", best_height);
                info!("in header synchronization. sending getheaders message");
                self.sync_state = SyncState::IBD(IBDState::HeaderSync(HeaderSyncState {
                    target_height: best_height,
                    current_sync_height: 0,
                }));
            }
            SyncState::IBD(IBDState::BlockSync(ref block_idx)) => {
                let block_inventory = self
                    .headers_chain
                    .get_block_inventory(&BlockHash::from_byte_array(block_idx.1))
                    .unwrap();

                let msg_to_send =
                    build_raw_network_msg(self.network, NetworkMessage::GetData(block_inventory))
                        .unwrap();
                self.tcp_conn_channel.0.send(msg_to_send).unwrap();
            }
            SyncState::InSync(ref block_idx) => {
                let headers_msg = self
                    .build_get_headers_msg(vec![BlockHash::from_byte_array(block_idx.1)])
                    .unwrap();

                self.tcp_conn_channel.0.send(headers_msg).unwrap();
            }
        };

        // NOTE: send a sendheaders message here to get announce to peer we want to receive new
        // block updates through header messages instead of inv.

        // receive messages that peer sent us and process them
        loop {
            tokio::select! {
                _ = self.shutdown_signal.1.changed() => {
                    break
                }
                raw_network_msg  = self.tcp_conn_channel.1.recv() => {
                    let raw_network_msg = raw_network_msg.unwrap();

                    self.process_message(raw_network_msg).unwrap();
                }
            }
        }
    }

    fn process_message(&mut self, msg: RawNetworkMessage) -> Result<(), NodeError> {
        match msg.payload() {
            NetworkMessage::Block(block) => {
                let raw_block = consensus::encode::serialize(block);
                let kernel_block = bitcoinkernel::Block::try_from(raw_block.as_slice())?;

                // NOTE: according to comments in code from lib, first bool is
                // if the block was valid and 2nd is if it's a new block.
                // if valid and not a duplicate, update last block/height in peer
                let (valid, new) = self.chain_manager.process_block(&kernel_block);
                if valid && new {
                    let tip_idx = self.chain_manager.get_block_index_tip();
                    info!(
                        "received new valid block. Updating to new tip height {} in peer",
                        tip_idx.height()
                    );

                    // check if we have processed 500 blocks, if so, ask for next 500.
                    // not ideal to do it this way
                    let last_sync_block = match &self.sync_state {
                        SyncState::IBD(IBDState::BlockSync(block_idx)) => block_idx.clone(),
                        SyncState::InSync(block_idx) => block_idx.clone(),
                        _ => BlockIndex(0, [0; 32]),
                    };

                    if last_sync_block.0 + 499 < tip_idx.height() {
                        info!("processed block batch. asking for new 500 batch");

                        self.sync_state = SyncState::InSync(BlockIndex(
                            tip_idx.height(),
                            tip_idx.block_hash().hash,
                        ));

                        let block_inventory =
                            self.headers_chain
                                .get_block_inventory(&BlockHash::from_byte_array(
                                    tip_idx.block_hash().hash,
                                ))?;

                        let msg_to_send = build_raw_network_msg(
                            self.network,
                            NetworkMessage::GetData(block_inventory),
                        )?;
                        self.tcp_conn_channel.0.send(msg_to_send).unwrap();
                    }
                }

                Ok(())
            }
            NetworkMessage::Headers(headers) => {
                let block_headers = headers.to_vec();
                let headers_len = block_headers.len() as i32;
                let block_headers = self.headers_chain.process_headers(block_headers)?;

                info!("got {} valid headers", headers_len);

                match &self.sync_state {
                    SyncState::IBD(IBDState::HeaderSync(sync_progress)) => {
                        let new_sync_height = sync_progress.current_sync_height + headers_len;

                        if new_sync_height >= sync_progress.target_height {
                            info!(
                                "finished with headers sync at height {}. Will start full block sync.",
                                sync_progress.target_height
                            );
                            self.headers_chain.save_headers_chain()?;
                            info!("saved headers chain to db");

                            // change sync state to IBD block sync
                            let genesis_index = self.chain_manager.get_block_index_genesis();
                            self.sync_state = SyncState::IBD(IBDState::BlockSync(BlockIndex(
                                genesis_index.height(),
                                genesis_index.block_hash().hash,
                            )));

                            // send get data message to start getting full blocks
                            let block_inventory = self.headers_chain.get_block_inventory(
                                &BlockHash::from_byte_array(genesis_index.block_hash().hash),
                            )?;

                            let msg_to_send = build_raw_network_msg(
                                self.network,
                                NetworkMessage::GetData(block_inventory),
                            )?;
                            self.tcp_conn_channel.0.send(msg_to_send).unwrap();
                        } else {
                            self.sync_state =
                                SyncState::IBD(IBDState::HeaderSync(HeaderSyncState {
                                    target_height: sync_progress.target_height,
                                    current_sync_height: new_sync_height,
                                }));

                            let block_hash = BlockHash::from_byte_array(self.headers_chain.tip().1);
                            let raw_headers_msg = self.build_get_headers_msg(vec![block_hash])?;
                            self.tcp_conn_channel.0.send(raw_headers_msg).unwrap();
                        }
                        Ok(())
                    }
                    _ => {
                        let block_inventory = headers
                            .into_iter()
                            .map(|header| Inventory::WitnessBlock(header.block_hash()))
                            .collect();

                        self.headers_chain.save_headers(block_headers)?;

                        let msg_to_send = build_raw_network_msg(
                            self.network,
                            NetworkMessage::GetData(block_inventory),
                        )?;
                        self.tcp_conn_channel.0.send(msg_to_send).unwrap();

                        Ok(())
                    }
                }
            }
            NetworkMessage::Inv(_) => {
                // NOTE: should send a sendheader msg to receive notifications about new blocks
                // through headers messages rather than inv
                let block_hash = BlockHash::from_byte_array(self.headers_chain.tip().1);
                let get_headers_msg = self.build_get_headers_msg(vec![block_hash])?;
                self.tcp_conn_channel.0.send(get_headers_msg).unwrap();

                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn build_get_headers_msg(
        &self,
        locator_hash: Vec<BlockHash>,
    ) -> Result<RawNetworkMessage, NodeError> {
        let get_headers_msg =
            GetHeadersMessage::new(locator_hash, BlockHash::from_byte_array([0; 32]));
        let raw_headers_msg =
            build_raw_network_msg(self.network, NetworkMessage::GetHeaders(get_headers_msg))?;
        Ok(raw_headers_msg)
    }
}
