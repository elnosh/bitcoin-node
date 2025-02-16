use std::fmt::Display;

use bitcoin::Network;
use bitcoinkernel::ChainType;
use clap::ValueEnum;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum BitcoinNetwork {
    Mainnet,
    Testnet,
    Signet,
    Regtest,
}

impl Display for BitcoinNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BitcoinNetwork::Mainnet => write!(f, "mainnet"),
            BitcoinNetwork::Testnet => write!(f, "testnet"),
            BitcoinNetwork::Signet => write!(f, "signet"),
            BitcoinNetwork::Regtest => write!(f, "regtest"),
        }
    }
}

impl From<BitcoinNetwork> for ChainType {
    fn from(value: BitcoinNetwork) -> Self {
        match value {
            BitcoinNetwork::Mainnet => Self::MAINNET,
            BitcoinNetwork::Testnet => Self::TESTNET,
            BitcoinNetwork::Signet => Self::SIGNET,
            BitcoinNetwork::Regtest => Self::REGTEST,
        }
    }
}

impl From<BitcoinNetwork> for Network {
    fn from(value: BitcoinNetwork) -> Self {
        match value {
            BitcoinNetwork::Mainnet => Self::Bitcoin,
            BitcoinNetwork::Testnet => Self::Testnet4,
            BitcoinNetwork::Signet => Self::Signet,
            BitcoinNetwork::Regtest => Self::Regtest,
        }
    }
}
