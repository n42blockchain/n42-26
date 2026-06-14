//! Database adapter for parallel EVM execution.
//!
//! Each transaction gets its own `ParallelDb` that reads from MvMemory (for values
//! written by preceding txs) with fallback to the base state. All reads are recorded
//! for later conflict validation.

use crate::mv_memory::{MvMemory, MvRead};
use crate::types::{AccountSnapshot, LocationKey, ReadEntry, ReadValue, TxIdx};
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
    /// When `Some`, this address is the deferred block beneficiary: its account
    /// is read "blind" (base value, NOT recorded in the read set), so the fee
    /// credit creates no read/write dependency and never aborts other txs. The
    /// balance delta is extracted post-execution. See `docs/devlog-67`.
    deferred_bene: Option<Address>,
}

impl<'a, DB> ParallelDb<'a, DB> {
    pub fn new(
        tx_idx: TxIdx,
        base: &'a DB,
        mv: &'a MvMemory,
        read_set: SharedReadSet,
        deferred_bene: Option<Address>,
    ) -> Self {
        Self {
            tx_idx,
            base,
            mv,
            read_set,
            deferred_bene,
        }
    }

    fn record_read(&self, key: LocationKey, value: ReadValue) {
        self.read_set.lock().push(ReadEntry { key, value });
    }
}

impl<DB: DatabaseRef> Database for ParallelDb<'_, DB>
where
    DB::Error: fmt::Display,
{
    type Error = ParallelDbError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        // Deferred beneficiary: blind base read, no read-set entry. revm credits
        // the fee against this base value; the delta is recovered after execution
        // and accumulated commutatively, so the beneficiary never conflicts.
        if self.deferred_bene == Some(address) {
            return self
                .base
                .basic_ref(address)
                .map_err(|e| ParallelDbError(e.to_string()));
        }
        let info = match self.mv.read_account(self.tx_idx, address) {
            MvRead::Value(_src_tx, info) => info,
            MvRead::NotFound => self
                .base
                .basic_ref(address)
                .map_err(|e| ParallelDbError(e.to_string()))?,
        };
        self.record_read(
            LocationKey::Account(address),
            ReadValue::Account(AccountSnapshot::of(&info)),
        );
        Ok(info)
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
        let value = match self.mv.read_storage(self.tx_idx, address, index) {
            MvRead::Value(_src_tx, value) => value,
            MvRead::NotFound => self
                .base
                .storage_ref(address, index)
                .map_err(|e| ParallelDbError(e.to_string()))?,
        };
        self.record_read(LocationKey::Storage(address, index), ReadValue::Storage(value));
        Ok(value)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.base
            .block_hash_ref(number)
            .map_err(|e| ParallelDbError(e.to_string()))
    }
}
