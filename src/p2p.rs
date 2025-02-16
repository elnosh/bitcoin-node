use bitcoin::{
    consensus::{self, encode::Error, Decodable, Encodable},
    hashes::sha256d::Hash,
    io::ErrorKind,
    p2p::{
        message::{NetworkMessage, RawNetworkMessage},
        message_blockdata::{GetBlocksMessage, Inventory},
        message_network::VersionMessage,
        Address, Magic, ServiceFlags,
    },
    Block, BlockHash, Network,
};
use bitcoinkernel::ChainstateManager;
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use dns_lookup::lookup_host;
use log::{error, info};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream},
    str::FromStr,
    sync::{Arc, Mutex},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::error::NodeError;

fn build_raw_network_msg(
    network: Network,
    msg: NetworkMessage,
) -> Result<RawNetworkMessage, NodeError> {
    let network_magic = match network {
        Network::Bitcoin => Magic::BITCOIN,
        Network::Testnet4 => Magic::TESTNET4,
        Network::Signet => Magic::SIGNET,
        Network::Regtest => Magic::REGTEST,
        _ => return Err(NodeError::NetworkNotSupported),
    };
    Ok(RawNetworkMessage::new(network_magic, msg))
}

fn get_nodes(network: Network) -> Result<Vec<IpAddr>, NodeError> {
    let seed = match network {
        Network::Bitcoin => "seed.bitcoin.sipa.be.",
        Network::Testnet4 => "seed.testnet4.bitcoin.sprovoost.nl.",
        Network::Signet => "seed.signet.bitcoin.sprovoost.nl.",
        Network::Regtest => return Ok(vec![IpAddr::from_str("127.0.0.1").unwrap()]),
        _ => return Err(NodeError::NetworkNotSupported),
    };
    let ips: Vec<IpAddr> = lookup_host(seed).unwrap();
    Ok(ips)
}

pub struct PeerManager {
    chain_manager: ChainstateManager,
    //peers: Vec<Peer>,
    // receive blocks on this channel. This should check if a block was already seen.
    // If so, ignore. If not, process block by passing to ChainstateManager
    blocks_channel: (Sender<Block>, Receiver<Block>),
    shutdown_signal: Receiver<bool>,
    network: Network,
}

impl PeerManager {
    pub fn new(
        chain_manager: ChainstateManager,
        shutdown_signal: crossbeam_channel::Receiver<bool>,
        network: Network,
    ) -> Self {
        PeerManager {
            chain_manager,
            //peers: Vec::new(),
            blocks_channel: unbounded(),
            shutdown_signal,
            network,
        }
    }

    // long running method that will:
    //      - connect to peers (just 1 for now)
    //      - receive block data in channel from peers
    //      - pass blocks to chainstate mgr to be validated.
    pub fn run(self) {
        let ips = get_nodes(self.network).unwrap();
        info!("got nodes to connect {:?}", ips);

        let current_tip = self.chain_manager.get_block_index_tip();

        // channel that will be used by peer struct to communicate new blocks
        let block_sender = self.blocks_channel.0.clone();

        // channel on which to communicate updated tip
        let (tip_sender, tip_receiver) = unbounded();

        // spawn threads here to handle peer connections
        // only connect to ips[0] so just one peer for now. Eventually handle multiple
        // connections
        thread::spawn(move || {
            // TODO: properly terminate this thread on shutdown

            let mut peer = set_peer(
                ips[0],
                (current_tip.height(), current_tip.block_hash().hash),
                block_sender,
                tip_receiver,
                self.network,
            )
            .unwrap();
            info!("successfully connected to peer!");
            // handle_connection runs indefinitely and only returns if there was an error.
            // if there was error, it is logged there.
            peer.handle_connection();
            // remove peer since something went wrong in the connection
            error!("handle_connection returned. removing peer...");
        });

        // thread here to receive on blocks channel and pass them to chainstate
        // manager to be processed
        let _ = thread::spawn(move || {
            loop {
                select! {
                    recv(self.shutdown_signal) -> _ => {
                        return;
                    },
                    recv(self.blocks_channel.1) -> msg => {
                        let block = match msg {
                            Ok(b) => b,
                            Err(_) => {
                                error!("terminating... channel disconnected");
                                return;
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
                        let (b1, b2) = self.chain_manager.process_block(&kernel_block);
                        if b1 && b2 {
                            let tip_idx = self.chain_manager.get_block_index_tip();
                            info!("received new valid block. Updating to new tip height {} in peer", tip_idx.height());

                            if let Err(_) = tip_sender.send((tip_idx.height(), tip_idx.block_hash().hash)) {
                                error!("could not send new tip on channel to peer");
                            };
                        }
                    },
                }
            }
        })
        .join();
    }
}

type BlockIndex = (i32, [u8; 32]);

struct Peer {
    conn: Arc<TcpStream>,
    current_block: Arc<Mutex<BlockIndex>>,
    // channel on which to send block data to peer manager
    blocks_channel: Sender<Block>,
    // this channel will receive updates on longest valid chain
    // from chainstate manager
    tip: Receiver<BlockIndex>,
    network: Network,
    // - type of node
}

fn set_peer(
    ip: IpAddr,
    current_block: BlockIndex,
    blocks_channel: Sender<Block>,
    tip: Receiver<BlockIndex>,
    network: Network,
) -> Result<Peer, NodeError> {
    let port = match network {
        Network::Bitcoin => 8333,
        Network::Testnet4 => 18333,
        Network::Signet => 38333,
        Network::Regtest => 18444,
        _ => return Err(NodeError::NetworkNotSupported),
    };
    let conn = TcpStream::connect((ip, port)).unwrap();
    Ok(Peer {
        conn: Arc::new(conn),
        current_block: Arc::new(Mutex::new(current_block)),
        blocks_channel,
        tip,
        network,
    })
}

impl Peer {
    // long running method that will handle peer message exchange.
    // any received messages deemed worth communicating (like block data) should be sent to peer manager
    // it returns if an unrecoverable error happens like closed connection
    // handshake error, etc.
    fn handle_connection(&mut self) {
        // do handshake with peer first
        if let Some(err) = self.peer_handshake().err() {
            error!("error doing peer handshake {:?}", err);
            return ();
        }
        info!("initial peer handshake complete!");

        // channel to communicate messages to be written on the tcp connection to peer
        let tcp_comms_channel = unbounded();

        info!("sending initial getblocks msg");

        // send getblocks msg to kickoff process of getting missing block data
        let block_hash =
            BlockHash::from_raw_hash(*Hash::from_bytes_ref(&self.current_block.lock().unwrap().1));
        let stop_hash = BlockHash::from_raw_hash(*Hash::from_bytes_ref(&[0; 32]));
        let get_blocks_msg = GetBlocksMessage::new(vec![block_hash], stop_hash);
        tcp_comms_channel
            .0
            .send(
                self.build_raw_network_msg(NetworkMessage::GetBlocks(get_blocks_msg))
                    .unwrap(),
            )
            .unwrap();

        let conn_clone = Arc::clone(&self.conn);

        // write on a different thread
        thread::spawn(move || {
            loop {
                // TODO: REMOVE THESE UNWRAPS
                let msg_to_send = tcp_comms_channel.1.recv().unwrap();
                msg_to_send.consensus_encode(&mut (&*conn_clone)).unwrap();
            }
        });

        let tcp_send_clone = tcp_comms_channel.0.clone();
        let tip_rec = self.tip.clone();
        let current_block = Arc::clone(&self.current_block);
        let network = self.network;

        thread::spawn(move || {
            loop {
                let new_tip = tip_rec.recv();
                if let Ok(new_tip) = new_tip {
                    let mut current_block = current_block.lock().unwrap();
                    info!(
                        "received new tip on channel {}. CURRENT self.height is {}",
                        new_tip.0, current_block.0
                    );

                    // using this number because the max number of blocks that are sent in response
                    // to a getblocks msg is 500. This is only during IBD. Should change when already
                    // synced.
                    if current_block.0 + 495 < new_tip.0 {
                        info!("updating latest height to send new getblocks msg");
                        *current_block = (new_tip.0, new_tip.1);

                        let block_hash =
                            BlockHash::from_raw_hash(*Hash::from_bytes_ref(&current_block.1));

                        let stop_hash = BlockHash::from_raw_hash(*Hash::from_bytes_ref(&[0; 32]));
                        let get_blocks_msg = GetBlocksMessage::new(vec![block_hash], stop_hash);
                        tcp_send_clone
                            .send(
                                build_raw_network_msg(
                                    network,
                                    NetworkMessage::GetBlocks(get_blocks_msg),
                                )
                                .unwrap(),
                            )
                            .unwrap();
                    }
                };
            }
        });

        // - start asking for blockchain data:
        //      - send getblocks message
        //      - receive inv message with list of blocks
        //      - send getdata to get blocks. Blocks received are full blocks with tx data
        //      - send this block data to peer mngr which then passes it to chainstate manager to process
        //      and validate block
        loop {
            let network_msg = RawNetworkMessage::consensus_decode(&mut (&*self.conn));
            match network_msg {
                Ok(raw_msg) => {
                    match raw_msg.payload() {
                        // on inv msg "MSG_BLOCK", send getdata msg to get full blocks
                        NetworkMessage::Inv(inv) => {
                            // NOTE: getting everything from inventory. Eventually should first
                            // check if needed and then only ask for what's needed
                            info!("received 'inv' msg. Sending getdata");

                            // only handling "MSG_BLOCK" for now. Items in the Inventory vector
                            // will be "MSG_BLOCK" but when sending "getdata" msg to get those
                            // blocks, need to change items from "MSG_BLOCK" to "MSG_WITNESS_BLOCK"
                            // to get hash of blocks with witness data according to BIP-144.
                            let block_hashes: Vec<bitcoin::BlockHash> = inv
                                .iter()
                                .filter_map(|inv| match inv {
                                    Inventory::Block(hash) => Some(*hash),
                                    _ => None,
                                })
                                .collect();

                            let inventory: Vec<Inventory> = block_hashes
                                .into_iter()
                                .map(|hash| Inventory::WitnessBlock(hash.clone()))
                                .collect();

                            let get_data_msg = NetworkMessage::GetData(inventory);
                            tcp_comms_channel
                                .0
                                .send(self.build_raw_network_msg(get_data_msg).unwrap())
                                .unwrap();
                        }

                        NetworkMessage::Block(block) => {
                            self.blocks_channel.send(block.clone()).unwrap();
                        }
                        _ => {}
                    }
                }
                Err(err) => {
                    if let Error::Io(io_err) = err {
                        match io_err.kind() {
                            // here need to detect a closed connection. If connection got closed, return
                            // from this method. If non fatal error in connection, ignore and try to read
                            // next message
                            ErrorKind::ConnectionAborted => {
                                error!("connection to peer got closed.");
                                return ();
                            }
                            _ => continue,
                        }
                    }
                }
            }
        }
    }

    // do initial handshake by exchanging version messages
    fn peer_handshake(&mut self) -> Result<(), NodeError> {
        let version_msg =
            self.build_raw_network_msg(NetworkMessage::Version(self.build_version_msg()))?;

        // send version msg
        version_msg
            .consensus_encode(&mut (&*self.conn))
            .map_err(|e| {
                NodeError::NetworkIO(format!("error sending version msg to peer {:?}", e))
            })?;
        info!("sent version msg to peer");

        let mut verack_received = false;
        let mut version_received = false;

        loop {
            let network_msg =
                RawNetworkMessage::consensus_decode(&mut (&*self.conn)).map_err(|e| {
                    NodeError::NetworkIO(format!("error reading msg from peer {:?}", e))
                })?;
            info!("received {} msg", network_msg.command());

            match network_msg.payload() {
                NetworkMessage::Verack => verack_received = true,
                // not verifying anything in the version msg right now
                // TODO: should read 'start_height' field to know the height
                // at which this peer is up to date. For now assuming that peer would be up to
                // date with the longest valid chain
                NetworkMessage::Version(_) => version_received = true,
                // ignore other messages during handshake. This is most likely not ideal
                _ => (),
            }

            if verack_received && version_received {
                break;
            }
        }

        // send verack to peer after receiving version msg
        self.build_raw_network_msg(NetworkMessage::Verack)?
            .consensus_encode(&mut (&*self.conn))
            .map_err(|e| {
                NodeError::NetworkIO(format!("error sending verack msg to peer {:?}", e))
            })?;

        info!("sent verack msg to peer");

        Ok(())
    }

    fn build_raw_network_msg(&self, msg: NetworkMessage) -> Result<RawNetworkMessage, NodeError> {
        build_raw_network_msg(self.network, msg)
    }

    fn build_version_msg(&self) -> VersionMessage {
        VersionMessage::new(
            ServiceFlags::NONE,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
            Address::new(&self.conn.peer_addr().unwrap(), ServiceFlags::NONE),
            // this field is not used so it can be dummy data
            Address::new(
                &SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)),
                ServiceFlags::NONE,
            ),
            //random(),
            // can set it to 0 for now since it will only do outbound connections
            0,
            "bitcoin-node".to_string(),
            self.current_block.lock().unwrap().0,
        )
    }
}
