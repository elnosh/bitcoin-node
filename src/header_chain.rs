use std::collections::HashMap;

use bitcoin::{
    block::Header, constants::genesis_block, hashes::Hash, p2p::message_blockdata::Inventory,
    params::Params, BlockHash, Network,
};
use log::info;
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::{
    error::{HeaderChainError, HeaderValidationError},
    node::{BlockIndex, Height},
};

const HEADER_TIP: &str = "header_tip";
const HEADER_TIP_TABLE: TableDefinition<&str, Vec<u8>> = TableDefinition::new("header_tip");
const HEADER_CHAIN_TABLE: TableDefinition<[u8; 32], Vec<u8>> = TableDefinition::new("header_tree");

pub struct HeadersChain {
    db: Database,
    headers_chain_tip: BlockIndex,
    headers_chain: HashMap<BlockHash, BlockHeader>,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BlockHeader {
    block_hash: BlockHash,
    previous_block: BlockHash,
    merkle_root: [u8; 32],
    time: u32,
    nonce: u32,
    next_block: Option<NextBlock>,
    height: Height,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
enum NextBlock {
    // Nodes in the chain should only point to one next block
    Chain(BlockHash),
    // if we are at the tip there could be forks and hence multiple next blocks
    Tip(Vec<BlockHash>),
}

impl BlockHeader {
    pub fn from_bitcoin_header(header: Header, height: Height) -> Self {
        BlockHeader {
            block_hash: header.block_hash(),
            previous_block: header.prev_blockhash,
            merkle_root: *header.merkle_root.as_ref(),
            time: header.time,
            nonce: header.nonce,
            next_block: None,
            height,
        }
    }
}

impl HeadersChain {
    pub fn new(dir: &str, network: Network) -> Result<HeadersChain, HeaderChainError> {
        let db = Database::create(format!("{}/header", dir))?;

        let headers_chain_tip = match headers_chain_tip(&db)? {
            Some(tip) => tip,
            None => {
                let genesis_block = genesis_block(Params::new(Network::from(network)));
                BlockIndex(0, *genesis_block.header.block_hash().as_ref())
            }
        };

        let headers_chain = match construct_headers_chain(&db) {
            Ok(chain) => chain,
            Err(e) => match e {
                // if this table does not exist, start headers chain from genesis
                HeaderChainError::DatabaseTableError(redb::TableError::TableDoesNotExist(_)) => {
                    let genesis_block = genesis_block(Params::new(Network::from(network)));
                    let genesis_block_header =
                        BlockHeader::from_bitcoin_header(genesis_block.header, 0);
                    let mut chain = HashMap::new();
                    chain.insert(genesis_block.block_hash(), genesis_block_header);
                    chain
                }
                _ => return Err(e),
            },
        };

        Ok(HeadersChain {
            db,
            headers_chain_tip,
            headers_chain,
        })
    }

    pub fn tip(&self) -> BlockIndex {
        self.headers_chain_tip.clone()
    }

    pub fn process_headers(
        &mut self,
        headers: Vec<Header>,
    ) -> Result<Vec<BlockHeader>, HeaderChainError> {
        let mut block_headers = Vec::with_capacity(headers.len());
        for header in headers {
            self.validate_header(header)?;

            let mut previous_block = self
                .headers_chain
                .get(&header.prev_blockhash)
                .unwrap()
                .clone();
            let previous_height = previous_block.height;

            // if header is valid, add to header chain map
            let new_block_header = BlockHeader::from_bitcoin_header(header, previous_height + 1);
            block_headers.push(new_block_header.clone());

            self.headers_chain
                .insert(header.block_hash(), new_block_header.clone());
            self.headers_chain_tip = BlockIndex(previous_height + 1, *header.block_hash().as_ref());

            previous_block.next_block = Some(NextBlock::Chain(new_block_header.block_hash));
            self.headers_chain
                .insert(previous_block.block_hash, previous_block);
        }

        Ok(block_headers)
    }

    fn validate_header(&self, header: Header) -> Result<(), HeaderValidationError> {
        // still TODO:
        // - validate timestamps (inside 2 hour range)
        // - validate difficulty adjustments
        // - validate height

        // check that previous block points to a previously seen one
        if !self.headers_chain.contains_key(&header.prev_blockhash) {
            return Err(HeaderValidationError::PrevBlockNotFound);
        }

        // validate the pow
        header.validate_pow(header.target())?;

        Ok(())
    }

    pub fn save_headers(&self, headers: Vec<BlockHeader>) -> Result<(), HeaderChainError> {
        let write_txn = self.db.begin_write()?;
        {
            let mut headers_chain_table = write_txn.open_table(HEADER_CHAIN_TABLE)?;
            for header in headers {
                let serialized_header = serde_json::to_vec(&header)?;
                headers_chain_table.insert(header.block_hash.as_ref(), &serialized_header)?;
            }
        }
        write_txn.commit()?;

        let write_tip_txn = self.db.begin_write()?;
        {
            let mut header_tip_table = write_tip_txn.open_table(HEADER_TIP_TABLE)?;
            let serialized_header_tip = serde_json::to_vec(&self.headers_chain_tip)?;
            header_tip_table.insert(HEADER_TIP, serialized_header_tip)?;
        }
        write_tip_txn.commit()?;

        Ok(())
    }

    // save current state of headers chain to db along with the tip
    pub fn save_headers_chain(&self) -> Result<(), HeaderChainError> {
        let write_txn = self.db.begin_write()?;
        {
            let mut headers_chain_table = write_txn.open_table(HEADER_CHAIN_TABLE)?;
            for (key, value) in &self.headers_chain {
                let serialized_header = serde_json::to_vec(value)?;
                headers_chain_table.insert(key.as_ref(), &serialized_header)?;
            }
        }
        write_txn.commit()?;

        let write_tip_txn = self.db.begin_write()?;
        {
            let mut header_tip_table = write_tip_txn.open_table(HEADER_TIP_TABLE)?;
            let serialized_header_tip = serde_json::to_vec(&self.headers_chain_tip)?;
            header_tip_table.insert(HEADER_TIP, serialized_header_tip)?;
        }
        write_tip_txn.commit()?;

        Ok(())
    }

    // build an inventory vector of 500 of block hashes from starting block
    // to be used in a getdata message to get full block data
    pub fn get_block_inventory(
        &self,
        start_block: &BlockHash,
    ) -> Result<Vec<Inventory>, HeaderChainError> {
        // check that previous block points to a previously seen one
        let mut current_block = match self.headers_chain.get(start_block) {
            Some(block_header) => block_header,
            None => {
                return Err(HeaderChainError::ValidationError(
                    HeaderValidationError::PrevBlockNotFound,
                ));
            }
        };

        let mut inventory: Vec<Inventory> = Vec::with_capacity(500);
        for _ in 0..=500 {
            match &current_block.next_block {
                Some(next_block) => {
                    if let NextBlock::Chain(hash) = next_block {
                        let block_inventory = Inventory::WitnessBlock(*hash);
                        inventory.push(block_inventory);
                        current_block = self.headers_chain.get(hash).unwrap();
                    }
                }
                None => break,
            }
        }

        Ok(inventory)
    }
}

// construct headers chain from db
fn construct_headers_chain(
    db: &Database,
) -> Result<HashMap<BlockHash, BlockHeader>, HeaderChainError> {
    let read_txn = db.begin_read()?;
    let header_chain_table = read_txn.open_table(HEADER_CHAIN_TABLE)?;
    let table_iter = header_chain_table.iter()?;
    let mut header_chain = HashMap::new();

    for result in table_iter {
        let (key, value) = result?;
        let block_hash = BlockHash::from_byte_array(key.value());
        let deserialized_header: BlockHeader = serde_json::from_slice(value.value().as_slice())?;
        header_chain.insert(block_hash, deserialized_header);
    }

    Ok(header_chain)
}

fn headers_chain_tip(db: &Database) -> Result<Option<BlockIndex>, HeaderChainError> {
    let read_txn = db.begin_read()?;
    let header_tip_table = match read_txn.open_table(HEADER_TIP_TABLE) {
        Ok(table) => table,
        Err(e) => match e {
            // if this table does not exist, return None
            redb::TableError::TableDoesNotExist(_) => return Ok(None),
            _ => return Err(HeaderChainError::DatabaseTableError(e)),
        },
    };

    match header_tip_table.get(HEADER_TIP)? {
        Some(header_tip) => {
            let headers_chain_tip = serde_json::from_slice(header_tip.value().as_slice())?;
            info!("headers chain tip from db {:?}", headers_chain_tip);
            Ok(Some(headers_chain_tip))
        }
        None => Ok(None),
    }
}
