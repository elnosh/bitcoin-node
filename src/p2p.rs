use bitcoin::{
    block::Header,
    consensus::{self, encode::Error, Decodable},
    hashes::sha256d::Hash,
    io::{Cursor, ErrorKind},
    p2p::{
        message::{CommandString, NetworkMessage, RawNetworkMessage},
        message_blockdata::{GetBlocksMessage, GetHeadersMessage, Inventory},
        message_network::VersionMessage,
        Address, Magic, ServiceFlags,
    },
    Block, BlockHash, Network,
};
use crossbeam_channel::{unbounded, Receiver, Sender};
use dns_lookup::lookup_host;
use log::{error, info};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    str::FromStr,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream,
    },
};

use crate::{
    error::NodeError,
    node::{BlockIndex, Height, IBDState, SyncState},
};

#[derive(Clone)]
pub struct PeerManager {
    sync_state: SyncState,
    block_height: BlockIndex,
    block_header_sender: Sender<Vec<Header>>,
    block_sender: Sender<Block>,
    sync_state_receiver: Receiver<SyncState>,
    best_height_sender: Sender<Height>,
    shutdown_signal: crossbeam_channel::Receiver<bool>,
    network: Network,
}

impl PeerManager {
    pub fn new(
        sync_state: SyncState,
        block_height: BlockIndex,
        block_header_sender: Sender<Vec<Header>>,
        block_sender: Sender<Block>,
        sync_state_receiver: Receiver<SyncState>,
        best_height_sender: Sender<Height>,
        shutdown_signal: crossbeam_channel::Receiver<bool>,
        network: Network,
    ) -> Self {
        PeerManager {
            sync_state,
            block_height,
            block_header_sender,
            block_sender,
            sync_state_receiver,
            best_height_sender,
            shutdown_signal,
            network,
        }
    }

    // long running method that will:
    //      - connect to peers (just 1 for now)
    //      - receive block data in channel from peers
    //      - pass blocks to chainstate mgr to be validated.
    pub async fn run(self) {
        let ips = get_nodes(self.network).unwrap();
        info!("got nodes to connect {:?}", ips);

        // task to handle peer connections.
        // only connects to ips[0] so just one peer for now. Eventually handle multiple
        // connections
        tokio::spawn(async move {
            // TODO: properly terminate this task on shutdown
            let mut peer = Peer::new(
                ips[0],
                self.sync_state,
                self.block_height,
                self.block_header_sender,
                self.block_sender,
                self.sync_state_receiver,
                self.best_height_sender,
                self.network,
            );
            // handle_connection runs indefinitely and only returns if there was an error.
            // if there was error, it is logged there.
            peer.handle_connection().await;
            // remove peer since something went wrong in the connection
            error!("handle_connection returned. removing peer...");
        });

        return;
    }
}

struct Peer {
    addr: (IpAddr, u16),
    starting_sync_state: SyncState,
    current_block: Arc<Mutex<BlockIndex>>,
    // channels on which to send block data to peer manager
    block_header_sender: Sender<Vec<Header>>,
    blocks_channel: Sender<Block>,
    // this channel will receive updates on longest valid chain
    // from chainstate manager
    sync_state_receiver: Receiver<SyncState>,
    best_height_sender: Sender<Height>,
    network: Network,
}

impl Peer {
    fn new(
        ip: IpAddr,
        starting_sync_state: SyncState,
        current_block: BlockIndex,
        block_header_sender: Sender<Vec<Header>>,
        blocks_channel: Sender<Block>,
        sync_state_receiver: Receiver<SyncState>,
        best_height_sender: Sender<Height>,
        network: Network,
    ) -> Peer {
        let port = match network {
            Network::Bitcoin => 8333,
            Network::Testnet4 => 18333,
            Network::Signet => 38333,
            // default to regest if none other are present
            _ => 18444,
        };
        Peer {
            addr: (ip, port),
            starting_sync_state,
            current_block: Arc::new(Mutex::new(current_block)),
            block_header_sender,
            blocks_channel,
            sync_state_receiver,
            best_height_sender,
            network,
        }
    }

    // long running method that will handle peer message exchange.
    // any received messages deemed worth communicating (like block data) should be sent to peer manager.
    // it returns if an unrecoverable error happens like closed connection
    // handshake error, etc.
    async fn handle_connection(&mut self) {
        let conn = match TcpStream::connect(self.addr).await {
            Ok(conn) => conn,
            Err(e) => {
                error!("could not establish connection to peer {}", e);
                return;
            }
        };
        info!("successfully connected to peer!");

        let peer_addr = conn.peer_addr().unwrap();
        let (mut conn_reader, mut conn_writer) = conn.into_split();

        // do handshake with peer first
        if let Some(err) = self
            .peer_handshake(&peer_addr, &mut conn_reader, &mut conn_writer)
            .await
            .err()
        {
            error!("error doing peer handshake {:?}", err);
            return ();
        }

        info!("initial peer handshake complete!");

        // channel to communicate messages to be written on the tcp connection to peer
        let tcp_comms_channel = unbounded();

        match self.starting_sync_state {
            // if we are in header sync, send getheaders msg
            SyncState::IBD(IBDState::HeaderSync) => {
                info!("doing header synchronization");
                let block_hash = BlockHash::from_raw_hash(*Hash::from_bytes_ref(
                    &self.current_block.lock().unwrap().1,
                ));
                let stop_hash = BlockHash::from_raw_hash(*Hash::from_bytes_ref(&[0; 32]));
                let get_headers_msg = GetHeadersMessage::new(vec![block_hash], stop_hash);
                if let Err(_) = tcp_comms_channel.0.send(
                    self.build_raw_network_msg(NetworkMessage::GetHeaders(get_headers_msg))
                        .unwrap(),
                ) {
                    error!("closing connection");
                    return;
                }
            }
            _ => {
                info!("sending initial getblocks msg");

                // if headers sync was finished, starts asking for block data
                // send getblocks msg to kickoff process of getting missing block data
                let block_hash = BlockHash::from_raw_hash(*Hash::from_bytes_ref(
                    &self.current_block.lock().unwrap().1,
                ));
                let stop_hash = BlockHash::from_raw_hash(*Hash::from_bytes_ref(&[0; 32]));
                let get_blocks_msg = GetBlocksMessage::new(vec![block_hash], stop_hash);
                if let Err(_) = tcp_comms_channel.0.send(
                    self.build_raw_network_msg(NetworkMessage::GetBlocks(get_blocks_msg))
                        .unwrap(),
                ) {
                    error!("closing connection");
                    return;
                }
            }
        }

        let tcp_recv = tcp_comms_channel.1.clone();
        // write on a different task
        tokio::spawn(async move {
            loop {
                let msg_to_send = tcp_recv.recv().unwrap();
                let msg_to_send = consensus::serialize(&msg_to_send);
                conn_writer.write_all(&msg_to_send).await.unwrap();
            }
        });

        let tcp_send_clone = tcp_comms_channel.0.clone();
        let current_block = Arc::clone(&self.current_block);
        let network = self.network;
        let sync_state_receiver = self.sync_state_receiver.clone();

        tokio::spawn(async move {
            loop {
                let new_sync_state = sync_state_receiver.recv().unwrap();

                let mut block_idx: Option<BlockIndex> = None;
                let in_block_sync_start = match &new_sync_state {
                    SyncState::IBD(IBDState::BlockSync(idx)) => {
                        block_idx = Some(*idx);
                        true
                    }
                    SyncState::InSync(idx) => {
                        block_idx = Some(*idx);
                        false
                    }
                    _ => false,
                };

                if let Some(idx) = block_idx {
                    info!(
                        "received new sync state. There is new tip at height {}",
                        idx.0
                    );
                    let mut current_block = current_block.lock().unwrap();

                    let block_hash = BlockHash::from_raw_hash(*Hash::from_bytes_ref(&idx.1));
                    let stop_hash = BlockHash::from_raw_hash(*Hash::from_bytes_ref(&[0; 32]));
                    let get_blocks_msg = GetBlocksMessage::new(vec![block_hash], stop_hash);

                    if in_block_sync_start {
                        tcp_send_clone
                            .send(
                                build_raw_network_msg(
                                    network,
                                    NetworkMessage::GetBlocks(get_blocks_msg),
                                )
                                .unwrap(),
                            )
                            .unwrap();
                    } else {
                        // using this number because the max number of blocks that are sent in response
                        // to a getblocks msg is 500. This is only during IBD. Should change when already
                        // synced.
                        if current_block.0 + 495 < idx.0 {
                            *current_block = (idx.0, idx.1);
                            info!("updating latest height to send new getblocks msg");
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
                    }
                };
            }
        });

        // - read messages from peer:
        //      - receive inv message with list of blocks
        //      - send getdata to get blocks. Blocks received are full blocks with tx data
        //      - send this block data to `Node` which then passes it to chainstate manager to process
        //      and validate block
        //      - pass block headers to be validated during headers sync
        loop {
            let network_msg = read_network_msg(&mut conn_reader).await;
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
                        NetworkMessage::Headers(headers) => {
                            let headers = headers.to_vec();

                            if headers.len() > 0 {
                                let last_header_received = headers.last().unwrap().block_hash();

                                self.block_header_sender.send(headers).unwrap();

                                // TODO: do not ask for next batch of headers like this.
                                let block_hash = BlockHash::from_raw_hash(*Hash::from_bytes_ref(
                                    last_header_received.as_ref(),
                                ));
                                let stop_hash =
                                    BlockHash::from_raw_hash(*Hash::from_bytes_ref(&[0; 32]));
                                let get_headers_msg =
                                    GetHeadersMessage::new(vec![block_hash], stop_hash);
                                if let Err(_) = tcp_comms_channel.0.send(
                                    self.build_raw_network_msg(NetworkMessage::GetHeaders(
                                        get_headers_msg,
                                    ))
                                    .unwrap(),
                                ) {
                                    error!("closing connection");
                                    return;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Err(err) => {
                    // TODO: detect connection closes
                    if let NodeError::BitcoinConsensusError(Error::Io(io_err)) = err {
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
    async fn peer_handshake(
        &mut self,
        peer_addr: &SocketAddr,
        reader: &mut OwnedReadHalf,
        writer: &mut OwnedWriteHalf,
    ) -> Result<(), NodeError> {
        let version_msg = build_raw_network_msg(
            self.network,
            NetworkMessage::Version(build_version_msg(
                peer_addr,
                self.current_block.lock().unwrap().0,
            )),
        )?;

        // send version msg
        let version_msg = consensus::serialize(&version_msg);
        writer.write_all(&version_msg).await?;
        info!("sent version msg to peer");

        let mut verack_received = false;
        let mut version_received = false;

        loop {
            let network_msg = read_network_msg(reader).await?;
            match network_msg.payload() {
                NetworkMessage::Verack => verack_received = true,
                // not verifying anything in the version msg right now
                // assuming that peer would be up to date with the longest valid chain
                NetworkMessage::Version(version_msg) => {
                    version_received = true;
                    match self.starting_sync_state {
                        // only send best height if in header sync
                        SyncState::IBD(IBDState::HeaderSync) => {
                            let peer_height = version_msg.start_height;
                            self.best_height_sender.send(peer_height).unwrap();
                        }
                        _ => {}
                    }
                }
                // ignore other messages during handshake. This is most likely not ideal
                _ => (),
            }

            if verack_received && version_received {
                break;
            }
        }

        // send verack to peer after receiving version msg
        let verack_msg = consensus::serialize(&self.build_raw_network_msg(NetworkMessage::Verack)?);
        writer.write_all(&verack_msg).await?;
        info!("sent verack msg to peer");

        Ok(())
    }

    fn build_raw_network_msg(&self, msg: NetworkMessage) -> Result<RawNetworkMessage, NodeError> {
        build_raw_network_msg(self.network, msg)
    }
}

fn get_nodes(network: Network) -> Result<Vec<IpAddr>, NodeError> {
    let seed = match network {
        Network::Bitcoin => "seed.bitcoin.sipa.be.",
        Network::Testnet4 => "seed.testnet4.bitcoin.sprovoost.nl.",
        Network::Signet => "seed.signet.bitcoin.sprovoost.nl.",
        Network::Regtest => return Ok(vec![IpAddr::from_str("127.0.0.1").unwrap()]),
        _ => return Err(NodeError::NetworkNotSupported),
    };
    let ips: Vec<IpAddr> = lookup_host(seed)?;
    Ok(ips)
}

async fn read_network_msg(reader: &mut OwnedReadHalf) -> Result<RawNetworkMessage, NodeError> {
    // read the first 24 bytes that include:
    // - network magic (4 bytes)
    // - command (12 bytes)
    // - payload length (4 bytes)
    // - checksum (4 bytes)
    // with payload length then we know how many bytes to read for the actual
    // contents of the msg
    let mut buf = vec![0_u8; 24];
    reader.read_exact(&mut buf).await?;
    let mut buf_reader = Cursor::new(&buf);

    // TODO: should take these into account. Using only payload length
    Magic::consensus_decode(&mut buf_reader)?;
    CommandString::consensus_decode(&mut buf_reader)?;
    let payload_length = u32::consensus_decode(&mut buf_reader)?;
    u32::consensus_decode(&mut buf_reader)?;

    // read the payload length to get the actual message
    let mut msg_buf = vec![0_u8; payload_length as usize];
    reader.read_exact(&mut msg_buf).await?;

    buf.extend_from_slice(&msg_buf);
    let mut buf_reader = Cursor::new(&buf);

    let network_msg = RawNetworkMessage::consensus_decode(&mut buf_reader)?;
    Ok(network_msg)
}

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

fn build_version_msg(peer_addr: &SocketAddr, start_height: i32) -> VersionMessage {
    VersionMessage::new(
        ServiceFlags::NONE,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64,
        Address::new(peer_addr, ServiceFlags::NONE),
        // this field is not used so it can be dummy data
        Address::new(
            &SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)),
            ServiceFlags::NONE,
        ),
        // can set it to 0 for now since it will only do outbound connections
        0,
        "bitcoin-node".to_string(),
        start_height,
    )
}
