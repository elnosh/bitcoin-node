use bitcoin::{
    consensus::{self, Decodable},
    io::Cursor,
    p2p::{
        message::{CommandString, NetworkMessage, RawNetworkMessage},
        message_network::VersionMessage,
        Address, Magic, ServiceFlags,
    },
    Network,
};
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
    sync::{
        broadcast::{Receiver, Sender},
        watch::Receiver as WatchReceiver,
    },
};

use crate::{error::NodeError, node::BlockIndex};
pub struct PeerManager {
    block_height: BlockIndex,
    // receiver will receive messages that the node wants written on the connection
    // NOTE: only the receiver portion of the channel should be used but
    // the receiver alone can't be cloned and need to clone them since each peer needs one.
    // only way to create another receiver is from the sender.
    tcp_conn_channel: (Sender<RawNetworkMessage>, Receiver<RawNetworkMessage>),
    // channel for peer manager to send back messages it received on tcp connection
    // to node for processing
    msg_sender: Sender<RawNetworkMessage>,
    shutdown_signal: WatchReceiver<bool>,
    network: Network,
}

impl PeerManager {
    pub fn new(
        block_height: BlockIndex,
        tcp_conn_channel: (Sender<RawNetworkMessage>, Receiver<RawNetworkMessage>),
        msg_sender: Sender<RawNetworkMessage>,
        shutdown_signal: WatchReceiver<bool>,
        network: Network,
    ) -> Self {
        PeerManager {
            block_height,
            tcp_conn_channel,
            msg_sender,
            shutdown_signal,
            network,
        }
    }

    // long running method that will:
    //      - connect to peers (just 1 for now)
    //      - receive block data in channel from peers
    //      - pass blocks to chainstate mgr to be validated.
    pub async fn run(&self) {
        let ips = get_nodes(&self.network).unwrap();
        info!("got nodes to connect {:?}", ips);

        let block_height = self.block_height.clone();
        let tcp_conn_sender = self.tcp_conn_channel.0.clone();
        let tcp_conn_receiver = tcp_conn_sender.subscribe();

        let msg_sender = self.msg_sender.clone();
        let shutdown_signal = self.shutdown_signal.clone();
        let network = self.network;

        // task to handle peer connections.
        // only connects to ips[0] so just one peer for now. Eventually handle multiple
        // connections
        tokio::spawn(async move {
            let peer = Peer::new(
                ips[0],
                block_height,
                (msg_sender, tcp_conn_receiver),
                shutdown_signal,
                network,
            );
            // handle_connection runs indefinitely and only returns if there was an error.
            // if there was error, it is logged there.
            peer.handle_connection().await;
            // remove peer since something went wrong in the connection
            info!("handle_connection returned. removing peer...");
        });

        return;
    }
}

struct Peer {
    addr: (IpAddr, u16),
    current_block: Arc<Mutex<BlockIndex>>,
    // sender used to communicate messages it received on the connection back to the node
    // received used to receive messages that the node wants written in the connection
    tcp_conn_channel: (Sender<RawNetworkMessage>, Receiver<RawNetworkMessage>),
    shutdown_signal: WatchReceiver<bool>,
    network: Network,
}

impl Peer {
    fn new(
        ip: IpAddr,
        current_block: BlockIndex,
        tcp_conn_channel: (Sender<RawNetworkMessage>, Receiver<RawNetworkMessage>),
        shutdown_signal: WatchReceiver<bool>,
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
            current_block: Arc::new(Mutex::new(current_block)),
            tcp_conn_channel,
            shutdown_signal,
            network,
        }
    }

    // long running method that will handle peer message exchange.
    // any received messages deemed worth communicating (like block data) should be sent to peer manager.
    // it returns if an unrecoverable error happens like closed connection
    // handshake error, etc.
    async fn handle_connection(mut self) {
        let conn = match TcpStream::connect(self.addr).await {
            Ok(conn) => conn,
            Err(e) => {
                error!("could not establish connection to peer {}", e);
                return;
            }
        };
        info!("successfully connected to peer");

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

        info!("initial peer handshake complete");

        let mut shutdown_1 = self.shutdown_signal.clone();
        let mut shutdown_2 = self.shutdown_signal.clone();
        // write on a different task
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_1.changed() => {
                        info!("shutting down writer");
                        break;
                    }
                    msg_to_send = self.tcp_conn_channel.1.recv() => {
                        let msg_to_send = msg_to_send.unwrap();
                        let msg_to_send = consensus::serialize(&msg_to_send);
                        conn_writer.write_all(&msg_to_send).await.unwrap();
                    }
                }
            }
        });

        let tcp_comms_send = self.tcp_conn_channel.0.clone();
        // - read messages from peer:
        //      - receive inv message with list of blocks
        //      - send getdata to get blocks. Blocks received are full blocks with tx data
        //      - send this block data to `Node` which then passes it to chainstate manager to process
        //      and validate block
        //      - pass block headers to be validated during headers sync
        loop {
            tokio::select! {
                _ = shutdown_2.changed() => {
                    info!("shutting msg reader");
                    break;
                }

                network_msg = read_network_msg(&mut conn_reader) => {
                    // pass read message from connection to node
                    let network_msg = network_msg.unwrap();
                    tcp_comms_send.send(network_msg).unwrap();
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
                NetworkMessage::Version(_) => {
                    version_received = true;
                    self.tcp_conn_channel.0.send(network_msg).unwrap();
                }
                // ignore other messages during handshake. This is most likely not ideal
                _ => (),
            }

            if verack_received && version_received {
                break;
            }
        }

        // send verack to peer after receiving version msg
        let verack_msg = consensus::serialize(&build_raw_network_msg(
            self.network,
            NetworkMessage::Verack,
        )?);
        writer.write_all(&verack_msg).await?;
        info!("sent verack msg to peer");

        Ok(())
    }
}

fn get_nodes(network: &Network) -> Result<Vec<IpAddr>, NodeError> {
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

pub fn build_raw_network_msg(
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
