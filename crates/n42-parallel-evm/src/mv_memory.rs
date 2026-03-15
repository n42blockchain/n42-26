//! Multi-Version Memory (MVCC store) for Block-STM parallel execution.
//!
//! Each storage location maintains a sorted map of `(TxIdx → Value)` entries.
//! A reader at index `i` sees the latest value written by any tx with index < i.

use crate::types::{AccountWrite, TxIdx};
use alloy_primitives::{Address, B256, U256};
use dashmap::DashMap;
use revm::state::{AccountInfo, Bytecode};
use std::collections::BTreeMap;
use std::sync::RwLock;

/// Result of reading from MvMemory.
#[derive(Debug)]
pub enum MvRead<T> {
    /// Found a concrete value written by the given tx.
    Value(TxIdx, T),
    /// No preceding tx wrote this key — read from base state.
    NotFound,
}

/// Thread-safe multi-version memory backing the Block-STM parallel execution.
pub struct MvMemory {
    /// Account info: address → { tx_idx → account_info }
    accounts: DashMap<Address, RwLock<BTreeMap<TxIdx, Option<AccountInfo>>>>,
    /// Storage slots: (address, slot) → { tx_idx → value }
    storage: DashMap<(Address, U256), RwLock<BTreeMap<TxIdx, U256>>>,
    /// Code cache (immutable per block — no versioning).
    code: DashMap<B256, Bytecode>,
}

impl Default for MvMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl MvMemory {
    pub fn new() -> Self {
        Self {
            accounts: DashMap::new(),
            storage: DashMap::new(),
            code: DashMap::new(),
        }
    }

    /// Read account info visible to `reader_idx` (latest version from a tx < reader_idx).
    pub fn read_account(&self, reader_idx: TxIdx, addr: Address) -> MvRead<Option<AccountInfo>> {
        if let Some(entry) = self.accounts.get(&addr) {
            let versions = entry.read().unwrap_or_else(|e| e.into_inner());
            if let Some((&src_tx, info)) = versions.range(..reader_idx).next_back() {
                return MvRead::Value(src_tx, info.clone());
            }
        }
        MvRead::NotFound
    }

    /// Write account info from `writer_idx`.
    pub fn write_account(&self, writer_idx: TxIdx, addr: Address, write: &AccountWrite) {
        let info = match write {
            AccountWrite::Updated(info) => Some(info.clone()),
            AccountWrite::Destroyed => None,
        };
        self.accounts
            .entry(addr)
            .or_insert_with(|| RwLock::new(BTreeMap::new()))
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(writer_idx, info);
    }

    /// Read storage slot visible to `reader_idx`.
    pub fn read_storage(&self, reader_idx: TxIdx, addr: Address, slot: U256) -> MvRead<U256> {
        if let Some(entry) = self.storage.get(&(addr, slot)) {
            let versions = entry.read().unwrap_or_else(|e| e.into_inner());
            if let Some((&src_tx, &val)) = versions.range(..reader_idx).next_back() {
                return MvRead::Value(src_tx, val);
            }
        }
        MvRead::NotFound
    }

    /// Write storage slot from `writer_idx`.
    pub fn write_storage(&self, writer_idx: TxIdx, addr: Address, slot: U256, value: U256) {
        self.storage
            .entry((addr, slot))
            .or_insert_with(|| RwLock::new(BTreeMap::new()))
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(writer_idx, value);
    }

    /// Cache code by hash (immutable, no versioning).
    pub fn cache_code(&self, hash: B256, code: Bytecode) {
        self.code.entry(hash).or_insert(code);
    }

    /// Get cached code.
    pub fn get_code(&self, hash: &B256) -> Option<Bytecode> {
        self.code.get(hash).map(|v| v.clone())
    }

    /// Remove all entries written by `tx_idx` (used before re-execution).
    pub fn clear_tx(&self, tx_idx: TxIdx) {
        for entry in self.accounts.iter() {
            entry
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&tx_idx);
        }
        for entry in self.storage.iter() {
            entry
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&tx_idx);
        }
    }

    /// Check if the latest writer for a key visible to `reader_idx` matches `expected_origin`.
    /// Used during validation to detect conflicts.
    pub fn latest_account_writer(&self, reader_idx: TxIdx, addr: &Address) -> Option<TxIdx> {
        self.accounts.get(addr).and_then(|entry| {
            entry
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .range(..reader_idx)
                .next_back()
                .map(|(&idx, _)| idx)
        })
    }

    pub fn latest_storage_writer(
        &self,
        reader_idx: TxIdx,
        addr: &Address,
        slot: &U256,
    ) -> Option<TxIdx> {
        self.storage.get(&(*addr, *slot)).and_then(|entry| {
            entry
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .range(..reader_idx)
                .next_back()
                .map(|(&idx, _)| idx)
        })
    }
}
