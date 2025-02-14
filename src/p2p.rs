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
    sync::Arc,
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::error::NodeError;

fn get_nodes(network: Network) -> Result<Vec<IpAddr>, NodeError> {
    let seed = match network {
        Network::Regtest => return Ok(vec![IpAddr::from_str("127.0.0.1").unwrap()]),
        Network::Signet => "seed.signet.bitcoin.sprovoost.nl.",
        Network::Bitcoin => "seed.bitcoin.sipa.be.",
        _ => return Err(NodeError::NetworkNotSupported),
    };
    let ips: Vec<IpAddr> = lookup_host(seed).unwrap();
    Ok(ips)
}

pub struct PeerManager {
    chain_manager: ChainstateManager,
    //peers: Vec<Peer>,
    // receive new blocks on this channel. This should check if block was already seen
    // if so, ignore. If not, process block by passing to ChainstateManager
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

    // long running thread that will:
    //      - maintain chain tip.
    //      - receive block data in channel from peers
    //      - pass blocks to chainstate mgr to be validated.
    pub fn run(self) {
        let ips = get_nodes(self.network).unwrap();
        info!("got nodes to connect {:?}", ips);

        let current_tip = self.chain_manager.get_block_index_tip();
        // let tip_height = current_tip.height();
        // let hash = current_tip.block_hash().hash;
        //
        // info!("current tip height {:?}", current_tip.height());
        // info!("current tip {:?}", current_tip.block_hash().hash);

        // sender can be cloned and passed to multiple peers
        // so each one can communicate new blocks
        // through the channel to the single receiver
        let block_sender = self.blocks_channel.0.clone();

        let (tip_sender, tip_receiver) = unbounded();

        // spawn threads here to handle peer connections
        // only connect to ips[0] so just one peer for now. Eventually handle multiple
        // connections
        thread::spawn(move || {
            let mut peer = set_peer(
                ips[0],
                // tip_height,
                // hash,
                current_tip.height(),
                current_tip.block_hash().hash,
                block_sender,
                tip_receiver,
                self.network,
            )
            .unwrap();
            info!("successfully connected to peer!");
            // handle_connection runs indefinitely and only returns if there was an error.
            // if there was error, it is logged there.
            peer.handle_peer_connection();
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
                        //let block = self.blocks_channel.1.recv().unwrap();
                        let block = msg.unwrap();
                        let raw_block = consensus::encode::serialize(&block);
                        let kernel_block = bitcoinkernel::Block::try_from(raw_block.as_slice()).unwrap();

                        //info!("processing block {:?}", kernel_block.get_hash().hash);

                        // NOTE: according to comments in code from lib, first bool is if the block
                        // was valid and 2nd is if it's a new block or duplicate.
                        // if valid, update last block/height in peer
                        let (b1, b2) = self.chain_manager.process_block(&kernel_block);
                        if b1 && b2 {
                            //info!("block was valid and new one. Updating tip in peer.");
                            // info!("new height {:?}", tip_idx.height());
                            let tip_idx = self.chain_manager.get_block_index_tip();

                            tip_sender
                                .send((tip_idx.height(), tip_idx.block_hash().hash))
                                .unwrap();
                        }
                    },
                }
            }
        })
        .join();
    }
}

struct Peer {
    conn: Arc<TcpStream>,
    height: i32,
    last_block: [u8; 32],
    // channel on which to send (block) data to peer manager
    blocks_channel: Sender<Block>,
    // this channel will receive updates on longest valid blockchain
    // from chainstate manager
    tip: Receiver<(i32, [u8; 32])>,
    network: Network,
    // - type of node
}

fn set_peer(
    ip: IpAddr,
    height: i32,
    last_block: [u8; 32],
    blocks_channel: Sender<Block>,
    tip: Receiver<(i32, [u8; 32])>,
    network: Network,
) -> Result<Peer, NodeError> {
    let port = match network {
        Network::Regtest => 18444,
        Network::Signet => 38333,
        Network::Bitcoin => 8333,
        _ => return Err(NodeError::NetworkNotSupported),
    };
    let conn = TcpStream::connect((ip, port)).unwrap();
    Ok(Peer {
        conn: Arc::new(conn),
        height,
        last_block,
        blocks_channel,
        tip,
        network,
    })
}

impl Peer {
    // long running thread that will handle peer message exchange.
    // any received messages deemed worth communicating (like block data) should be sent to peer manager
    // this method returns if an unrecoverable error happens like closed connection
    // handshake error, etc.
    fn handle_peer_connection(&mut self) {
        // do handshake with peer first
        if let Some(err) = self.peer_handshake().err() {
            error!("error doing peer handshake {:?}", err);
            return ();
        }
        info!("initial peer handshake complete!");

        let send_channel = unbounded();

        info!("sending initial getblocks msg");

        // send getblocks msg to kickoff process of getting missing block data
        let block_hash = BlockHash::from_raw_hash(*Hash::from_bytes_ref(&self.last_block));
        let stop_hash = BlockHash::from_raw_hash(*Hash::from_bytes_ref(&[0; 32]));
        let get_blocks_msg = GetBlocksMessage::new(vec![block_hash], stop_hash);
        send_channel
            .0
            .send(self.build_raw_network_msg(NetworkMessage::GetBlocks(get_blocks_msg)))
            .unwrap();

        let receiver = send_channel.1;

        let conn_clone = Arc::clone(&self.conn);

        // write on a different thread
        thread::spawn(move || {
            loop {
                // TODO: REMOVE THESE UNWRAPS
                let msg_to_send = receiver.recv().unwrap();
                msg_to_send.consensus_encode(&mut (&*conn_clone)).unwrap();
            }
        });

        // - start asking for blockchain data:
        //      - send getblocks message
        //      - receive inv message with list of blocks
        //      - send getdata to get blocks. Blocks received are full blocks with tx data
        //      - send this block data to peer mngr which then passes it to chainstate manager to process
        //      and validate block
        loop {
            let new_tip = self.tip.try_recv();
            if let Ok(new_tip) = new_tip {
                let height = new_tip.0;
                let block = new_tip.1;

                if self.height + 495 < height {
                    info!("updating latest height to send new getblocks msg");
                    self.height = height;
                    self.last_block = block;

                    let block_hash =
                        BlockHash::from_raw_hash(*Hash::from_bytes_ref(&self.last_block));
                    let stop_hash = BlockHash::from_raw_hash(*Hash::from_bytes_ref(&[0; 32]));
                    let get_blocks_msg = GetBlocksMessage::new(vec![block_hash], stop_hash);

                    send_channel
                        .0
                        .send(self.build_raw_network_msg(NetworkMessage::GetBlocks(get_blocks_msg)))
                        .unwrap();
                }
            };

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
                            send_channel
                                .0
                                .send(self.build_raw_network_msg(get_data_msg))
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

    // fn process_message(&self, _msg: RawNetworkMessage) -> Result<(), NodeError> {
    //     unimplemented!()
    // }

    // do initial handshake by exchanging version messages
    fn peer_handshake(&mut self) -> Result<(), NodeError> {
        let version_msg =
            self.build_raw_network_msg(NetworkMessage::Version(self.build_version_msg()));

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
        self.build_raw_network_msg(NetworkMessage::Verack)
            .consensus_encode(&mut (&*self.conn))
            .map_err(|e| {
                NodeError::NetworkIO(format!("error sending verack msg to peer {:?}", e))
            })?;

        info!("sent verack msg to peer");

        Ok(())
    }

    fn build_raw_network_msg(&self, msg: NetworkMessage) -> RawNetworkMessage {
        let network_magic = match self.network {
            Network::Signet => Magic::SIGNET,
            Network::Bitcoin => Magic::BITCOIN,
            _ => Magic::REGTEST,
        };
        RawNetworkMessage::new(network_magic, msg)
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
            self.height,
        )
    }
}
