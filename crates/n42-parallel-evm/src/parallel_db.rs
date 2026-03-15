//! Database adapter for parallel EVM execution.
//!
//! Each transaction gets its own `ParallelDb` that reads from MvMemory (for values
//! written by preceding txs) with fallback to the base state. All reads are recorded
//! for later conflict validation.

use crate::mv_memory::{MvMemory, MvRead};
use crate::types::{LocationKey, ReadEntry, ReadOrigin, TxIdx};
use alloy_primitives::{Address, B256, U256};
use parking_lot::Mutex;
use revm::database_interface::{DBErrorMarker, Database, DatabaseRef};
use revm::state::{AccountInfo, Bytecode};
use std::fmt;
use std::sync::Arc;

/// Error type wrapping the base database's error.
#[derive(Debug, Clone)]
pub struct ParallelDbError(pub String);

impl fmt::Display for ParallelDbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ParallelDbError {}
impl DBErrorMarker for ParallelDbError {}

/// Shared read-set collector. Cloned into ParallelDb so we can retrieve reads
/// after the EVM (which takes ownership of the db) finishes.
pub type SharedReadSet = Arc<Mutex<Vec<ReadEntry>>>;

/// A per-transaction database view backed by MvMemory + base state.
pub struct ParallelDb<'a, DB> {
    tx_idx: TxIdx,
    base: &'a DB,
    mv: &'a MvMemory,
    read_set: SharedReadSet,
}

impl<'a, DB> ParallelDb<'a, DB> {
    pub fn new(tx_idx: TxIdx, base: &'a DB, mv: &'a MvMemory, read_set: SharedReadSet) -> Self {
        Self {
            tx_idx,
            base,
            mv,
            read_set,
        }
    }

    fn record_read(&self, key: LocationKey, origin: ReadOrigin) {
        self.read_set.lock().push(ReadEntry { key, origin });
    }
}

impl<DB: DatabaseRef> Database for ParallelDb<'_, DB>
where
    DB::Error: fmt::Display,
{
    type Error = ParallelDbError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        match self.mv.read_account(self.tx_idx, address) {
            MvRead::Value(src_tx, info) => {
                self.record_read(LocationKey::Account(address), ReadOrigin::Tx(src_tx));
                Ok(info)
            }
            MvRead::NotFound => {
                let info = self
                    .base
                    .basic_ref(address)
                    .map_err(|e| ParallelDbError(e.to_string()))?;
                self.record_read(LocationKey::Account(address), ReadOrigin::Base);
                Ok(info)
            }
        }
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if let Some(code) = self.mv.get_code(&code_hash) {
            return Ok(code);
        }
        let code = self
            .base
            .code_by_hash_ref(code_hash)
            .map_err(|e| ParallelDbError(e.to_string()))?;
        self.mv.cache_code(code_hash, code.clone());
        Ok(code)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        match self.mv.read_storage(self.tx_idx, address, index) {
            MvRead::Value(src_tx, value) => {
                self.record_read(LocationKey::Storage(address, index), ReadOrigin::Tx(src_tx));
                Ok(value)
            }
            MvRead::NotFound => {
                let value = self
                    .base
                    .storage_ref(address, index)
                    .map_err(|e| ParallelDbError(e.to_string()))?;
                self.record_read(LocationKey::Storage(address, index), ReadOrigin::Base);
                Ok(value)
            }
        }
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.base
            .block_hash_ref(number)
            .map_err(|e| ParallelDbError(e.to_string()))
    }
}
