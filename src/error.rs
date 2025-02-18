use std::io;

#[derive(Debug)]
pub enum NodeError {
    NetworkIO(String),
    NetworkNotSupported,
    IoError(io::Error),
    BitcoinConsensusError(bitcoin::consensus::encode::Error),
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeError::NetworkIO(e) => write!(f, "{}", e),
            NodeError::NetworkNotSupported => write!(f, "network not supported"),
            NodeError::IoError(e) => write!(f, "{}", e),
            NodeError::BitcoinConsensusError(e) => write!(f, "{}", e),
        }
    }
}

impl From<io::Error> for NodeError {
    fn from(value: io::Error) -> Self {
        NodeError::IoError(value)
    }
}

impl From<bitcoin::consensus::encode::Error> for NodeError {
    fn from(value: bitcoin::consensus::encode::Error) -> Self {
        NodeError::BitcoinConsensusError(value)
    }
}
