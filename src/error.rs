use std::io;

use bitcoin::block::ValidationError as BitcoinValidationError;

#[derive(Debug)]
pub enum NodeError {
    NetworkIO(String),
    NetworkNotSupported,
    IoError(io::Error),
    BitcoinConsensusError(bitcoin::consensus::encode::Error),
    KernelError(bitcoinkernel::KernelError),
    SerdeJsonError(serde_json::Error),
    HeaderChainError(HeaderChainError),
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeError::NetworkIO(e) => write!(f, "{}", e),
            NodeError::NetworkNotSupported => write!(f, "network not supported"),
            NodeError::IoError(e) => write!(f, "{}", e),
            NodeError::BitcoinConsensusError(e) => write!(f, "{}", e),
            NodeError::KernelError(e) => write!(f, "{}", e),
            NodeError::SerdeJsonError(e) => write!(f, "{}", e),
            NodeError::HeaderChainError(e) => write!(f, "{}", e),
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

impl From<serde_json::Error> for NodeError {
    fn from(value: serde_json::Error) -> Self {
        NodeError::SerdeJsonError(value)
    }
}

impl From<HeaderChainError> for NodeError {
    fn from(value: HeaderChainError) -> Self {
        NodeError::HeaderChainError(value)
    }
}

#[derive(Debug)]
pub enum HeaderChainError {
    DatabaseError(redb::DatabaseError),
    DatabaseTransationError(redb::TransactionError),
    DatabaseTableError(redb::TableError),
    DatabaseStorageError(redb::StorageError),
    DatabaseCommitError(redb::CommitError),
    SerdeJsonError(serde_json::Error),
    ValidationError(HeaderValidationError),
}

impl std::fmt::Display for HeaderChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeaderChainError::DatabaseError(e) => write!(f, "{}", e),
            HeaderChainError::DatabaseTransationError(e) => write!(f, "{}", e),
            HeaderChainError::DatabaseTableError(e) => write!(f, "{}", e),
            HeaderChainError::DatabaseStorageError(e) => write!(f, "{}", e),
            HeaderChainError::DatabaseCommitError(e) => write!(f, "{}", e),
            HeaderChainError::SerdeJsonError(e) => write!(f, "{}", e),
            HeaderChainError::ValidationError(e) => write!(f, "{}", e),
        }
    }
}

impl From<redb::DatabaseError> for HeaderChainError {
    fn from(value: redb::DatabaseError) -> Self {
        HeaderChainError::DatabaseError(value)
    }
}

impl From<redb::TransactionError> for HeaderChainError {
    fn from(value: redb::TransactionError) -> Self {
        HeaderChainError::DatabaseTransationError(value)
    }
}

impl From<redb::TableError> for HeaderChainError {
    fn from(value: redb::TableError) -> Self {
        HeaderChainError::DatabaseTableError(value)
    }
}

impl From<redb::StorageError> for HeaderChainError {
    fn from(value: redb::StorageError) -> Self {
        HeaderChainError::DatabaseStorageError(value)
    }
}

impl From<redb::CommitError> for HeaderChainError {
    fn from(value: redb::CommitError) -> Self {
        HeaderChainError::DatabaseCommitError(value)
    }
}

impl From<serde_json::Error> for HeaderChainError {
    fn from(value: serde_json::Error) -> Self {
        HeaderChainError::SerdeJsonError(value)
    }
}

impl From<HeaderValidationError> for HeaderChainError {
    fn from(value: HeaderValidationError) -> Self {
        HeaderChainError::ValidationError(value)
    }
}

// errors while processing blockchain data
#[derive(Debug)]
pub enum HeaderValidationError {
    BitcoinValidationError(BitcoinValidationError),
    PrevBlockNotFound,
}

impl From<BitcoinValidationError> for HeaderValidationError {
    fn from(value: BitcoinValidationError) -> Self {
        HeaderValidationError::BitcoinValidationError(value)
    }
}

impl std::fmt::Display for HeaderValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeaderValidationError::BitcoinValidationError(e) => write!(f, "{}", e),
            HeaderValidationError::PrevBlockNotFound => {
                write!(f, "header previous block does not exist")
            }
        }
    }
}
