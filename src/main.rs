use p2p::PeerManager;

mod error;
mod p2p;

fn main() {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();

    let peer_mngr = PeerManager::new(bitcoin::Network::Regtest);
    peer_mngr.run()
}
