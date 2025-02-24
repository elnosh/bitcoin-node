use std::io;

#[derive(Debug)]
pub enum NodeError {
    NetworkIO(String),
    NetworkNotSupported,
    IoError(io::Error),
    BitcoinConsensusError(bitcoin::consensus::encode::Error),
    KernelError(bitcoinkernel::KernelError),
    DatabaseError(redb::DatabaseError),
    DatabaseTransationError(redb::TransactionError),
    DatabaseTableError(redb::TableError),
    DatabaseStorageError(redb::StorageError),
    DatabaseCommitError(redb::CommitError),
    SerdeJsonError(serde_json::Error),
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeError::NetworkIO(e) => write!(f, "{}", e),
            NodeError::NetworkNotSupported => write!(f, "network not supported"),
            NodeError::IoError(e) => write!(f, "{}", e),
            NodeError::BitcoinConsensusError(e) => write!(f, "{}", e),
            NodeError::KernelError(e) => write!(f, "{}", e),
            NodeError::DatabaseError(e) => write!(f, "{}", e),
            NodeError::DatabaseTransationError(e) => write!(f, "{}", e),
            NodeError::DatabaseTableError(e) => write!(f, "{}", e),
            NodeError::DatabaseStorageError(e) => write!(f, "{}", e),
            NodeError::DatabaseCommitError(e) => write!(f, "{}", e),
            NodeError::SerdeJsonError(e) => write!(f, "{}", e),
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

impl From<bitcoinkernel::KernelError> for NodeError {
    fn from(value: bitcoinkernel::KernelError) -> Self {
        NodeError::KernelError(value)
    }
}

impl From<redb::DatabaseError> for NodeError {
    fn from(value: redb::DatabaseError) -> Self {
        NodeError::DatabaseError(value)
    }
}

impl From<redb::TransactionError> for NodeError {
    fn from(value: redb::TransactionError) -> Self {
        NodeError::DatabaseTransationError(value)
    }
}

impl From<redb::TableError> for NodeError {
    fn from(value: redb::TableError) -> Self {
        NodeError::DatabaseTableError(value)
    }
}

impl From<redb::StorageError> for NodeError {
    fn from(value: redb::StorageError) -> Self {
        NodeError::DatabaseStorageError(value)
    }
}

impl From<redb::CommitError> for NodeError {
    fn from(value: redb::CommitError) -> Self {
        NodeError::DatabaseCommitError(value)
    }
}

impl From<serde_json::Error> for NodeError {
    fn from(value: serde_json::Error) -> Self {
        NodeError::SerdeJsonError(value)
    }
}

// errors while processing blockchain data
#[derive(Debug)]
pub enum ValidationError {
    InvalidHeader(String),
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::InvalidHeader(e) => write!(f, "invalid header: {}", e),
        }
    }
}
