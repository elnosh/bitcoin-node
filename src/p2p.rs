use bitcoin::{
    consensus::{Decodable, Encodable},
    p2p::{
        message::{NetworkMessage, RawNetworkMessage},
        message_network::VersionMessage,
        Address, Magic, ServiceFlags,
    },
    Network,
};
use dns_lookup::lookup_host;
use log::{error, info};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream},
    str::FromStr,
    sync::mpsc::{channel, Receiver, Sender},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::error::NodeError;

fn get_nodes(network: Network) -> Result<Vec<IpAddr>, String> {
    let seed = match network {
        Network::Regtest => return Ok(vec![IpAddr::from_str("127.0.0.1").unwrap()]),
        Network::Bitcoin => "seed.bitcoin.sipa.be.",
        Network::Signet => "seed.signet.bitcoin.sprovoost.nl.",
        _ => return Err("network not supported".to_string()),
    };
    let ips: Vec<IpAddr> = lookup_host(seed).unwrap();
    Ok(ips)
}

pub struct PeerManager {
    chain_tip: u32,
    peers: Vec<Peer>,
    // receive new blocks on this channel. This should check if block was already seen
    // if so, ignore. If not, process block by passing to ChainstateManager
    blocks_channel: (Sender<u32>, Receiver<u32>),
    network: Network,
}

impl PeerManager {
    pub fn new(network: Network) -> Self {
        PeerManager {
            chain_tip: 0,
            peers: Vec::new(),
            blocks_channel: channel(),
            network,
        }
    }

    // long running thread that will:
    //      - maintain chain tip.
    //      - receive block data in channel from peers
    //      - pass block to chainstate mgr to be validated.
    pub fn run(self) {
        let ips = get_nodes(self.network).unwrap();
        info!("got nodes to connect {:?}", ips);

        // sender can be cloned and passed to multiple peers
        // so each one can communicate new blocks
        // through the channel to the single receiver
        let sender = self.blocks_channel.0.clone();

        // spawn threads here to handle peer connections
        // only connect to ips[0] so just one peer for now. Eventually handle multiple
        // connections
        let _ = thread::spawn(move || {
            let mut peer = set_peer(ips[0], sender, self.network).unwrap();
            info!("successfully connected to peer!");
            // handle_connection runs indefinitely and only returns if there was an error.
            // if there was error, it is logged there.
            peer.handle_connection();
            // remove peer since something went wrong in the connection
            error!("handle_connection returned. removing peer...");
        })
        .join();
    }
}

struct Peer {
    conn: TcpStream,
    last_block: i32,
    // channel on which to send (block) data to peer manager
    blocks_channel: Sender<u32>,
    network: Network,
    // - type of node
}

fn set_peer(ip: IpAddr, blocks_channel: Sender<u32>, network: Network) -> Result<Peer, String> {
    let port = match network {
        Network::Regtest => 18444,
        Network::Signet => 38333,
        Network::Bitcoin => 8333,
        _ => return Err("network not supported".to_string()),
    };
    let conn = TcpStream::connect((ip, port)).unwrap();
    Ok(Peer {
        conn,
        last_block: 0,
        blocks_channel,
        network,
    })
}

impl Peer {
    // long running thread that will handle peer message exchange.
    // any received messages deemed worth communicating (like block data) should be sent to peer manager
    // this method returns if an unrecoverable error happens like closed connection
    // handshake error, etc.
    fn handle_connection(&mut self) {
        // do handshake with peer first
        if let Some(err) = self.peer_handshake().err() {
            error!("error doing peer handshake {:?}", err);
            return ();
        }
        info!("initial peer handshake complete!");

        // - start asking for blockchain data:
        //      - send getblocks message
        //      - receive inv message with list of blocks
        //      - send getdata to get blocks. Blocks received are full blocks with tx data
        //      - send this block data to peer mngr which then passes it to chainstate manager to process
        //      and validate block
        //          - peer manager will check if the block received from peer should be sent to the
        //          chainstate mgr
        let _ = thread::spawn(|| {
            info!("doing peer msg exchange stuff");
            thread::sleep(Duration::from_secs(5));
        })
        .join();
    }

    // do initial handshake by exchanging version messages
    fn peer_handshake(&mut self) -> Result<(), NodeError> {
        let version_msg =
            self.build_raw_network_msg(NetworkMessage::Version(self.build_version_msg()));

        // send version msg
        version_msg.consensus_encode(&mut self.conn).map_err(|e| {
            NodeError::NetworkIO(format!("error sending version msg to peer {:?}", e))
        })?;
        info!("sent version msg to peer");

        let mut verack_received = false;
        let mut version_received = false;

        loop {
            let network_msg = RawNetworkMessage::consensus_decode(&mut self.conn).map_err(|e| {
                NodeError::NetworkIO(format!("error reading msg from peer {:?}", e))
            })?;
            info!("received {} msg", network_msg.command());

            match network_msg.payload() {
                NetworkMessage::Verack => verack_received = true,
                // not verifying anything in the version msg right now
                NetworkMessage::Version(_) => version_received = true,
                // ignore other messages during handshake. This is most likely not ideal
                _ => (),
            }

            if verack_received && version_received {
                break;
            }
        }

        // send verack to peer after receiving verson msg
        NetworkMessage::Verack
            .consensus_encode(&mut self.conn)
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
            self.last_block,
        )
    }
}
