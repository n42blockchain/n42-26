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

/// Per-tx write key for clear_tx indexing.
#[derive(Debug, Clone)]
enum WriteKey {
    Account(Address),
    Storage(Address, U256),
}

/// Thread-safe multi-version memory backing the Block-STM parallel execution.
pub struct MvMemory {
    /// Account info: address → { tx_idx → account_info }
    accounts: DashMap<Address, RwLock<BTreeMap<TxIdx, Option<AccountInfo>>>>,
    /// Storage slots: (address, slot) → { tx_idx → value }
    storage: DashMap<(Address, U256), RwLock<BTreeMap<TxIdx, U256>>>,
    /// Code cache (immutable per block — no versioning).
    code: DashMap<B256, Bytecode>,
    /// Per-tx write index: tx_idx → list of locations that tx wrote.
    /// Used by `clear_tx` to avoid O(|all_locations|) scans on re-execution.
    tx_writes: DashMap<TxIdx, Vec<WriteKey>>,
    /// Deferred-coinbase accumulator: tx_idx → balance delta this tx credited to
    /// the block beneficiary (gas fee + any value sent to it). Kept OUT of the
    /// versioned `accounts` map so they never create read/write conflicts
    /// (balance increments commute), eliminating the Block-STM coinbase cascade.
    /// Summed in `sum_bene_deltas` at commit; `clear_tx` drops a tx's entry on
    /// re-execution. See `docs/devlog-67`.
    bene_deltas: DashMap<TxIdx, U256>,
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
            tx_writes: DashMap::new(),
            bene_deltas: DashMap::new(),
        }
    }

    /// Record this tx's commutative balance delta to the block beneficiary.
    pub fn record_bene_delta(&self, tx_idx: TxIdx, delta: U256) {
        self.bene_deltas.insert(tx_idx, delta);
    }

    /// Sum every recorded beneficiary delta (commit-time materialization).
    pub fn sum_bene_deltas(&self) -> U256 {
        self.bene_deltas
            .iter()
            .fold(U256::ZERO, |acc, e| acc.saturating_add(*e.value()))
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
        self.tx_writes
            .entry(writer_idx)
            .or_default()
            .push(WriteKey::Account(addr));
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
        self.tx_writes
            .entry(writer_idx)
            .or_default()
            .push(WriteKey::Storage(addr, slot));
    }

    /// Cache code by hash (immutable, no versioning).
    pub fn cache_code(&self, hash: B256, code: Bytecode) {
        self.code.entry(hash).or_insert(code);
    }

    /// Get cached code.
    pub fn get_code(&self, hash: &B256) -> Option<Bytecode> {
        self.code.get(hash).map(|v| v.value().clone())
    }

    /// Remove all entries written by `tx_idx` (used before re-execution).
    ///
    /// O(|writes_of_tx|) via the per-tx write index. A missing index entry means
    /// the tx has never written anything (`write_account`/`write_storage` always
    /// register the index), so it is a no-op — the previous full-table-scan
    /// fallback ran on EVERY first execution (no prior writes) and made the
    /// whole block O(n^2).
    pub fn clear_tx(&self, tx_idx: TxIdx) {
        let start = std::time::Instant::now();
        let mut writes_count: usize = 0;
        let used_fallback = false;
        // Drop any deferred beneficiary delta from the prior execution.
        self.bene_deltas.remove(&tx_idx);
        if let Some((_, writes)) = self.tx_writes.remove(&tx_idx) {
            writes_count = writes.len();
            for w in writes {
                match w {
                    WriteKey::Account(addr) => {
                        if let Some(entry) = self.accounts.get(&addr) {
                            entry
                                .write()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&tx_idx);
                        }
                    }
                    WriteKey::Storage(addr, slot) => {
                        if let Some(entry) = self.storage.get(&(addr, slot)) {
                            entry
                                .write()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&tx_idx);
                        }
                    }
                }
            }
        }
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        metrics::histogram!(
            "n42_mv_memory_clear_tx_ms",
            "path" => if used_fallback { "fallback_scan" } else { "indexed" }
        )
        .record(elapsed_ms);
        if !used_fallback {
            metrics::histogram!("n42_mv_memory_clear_tx_writes").record(writes_count as f64);
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
