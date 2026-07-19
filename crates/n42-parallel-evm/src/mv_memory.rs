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
    StorageWipe(Address),
}

/// Thread-safe multi-version memory backing the Block-STM parallel execution.
pub struct MvMemory {
    /// Account info: address → { tx_idx → account_info }
    accounts: DashMap<Address, RwLock<BTreeMap<TxIdx, Option<AccountInfo>>>>,
    /// Storage slots: (address, slot) → { tx_idx → value }
    storage: DashMap<(Address, U256), RwLock<BTreeMap<TxIdx, U256>>>,
    /// Whole-account storage wipes caused by SELFDESTRUCT. Slot reads compare
    /// the latest wipe with the latest per-slot write so untouched historical
    /// slots become zero while writes after a recreation remain visible.
    storage_wipes: DashMap<Address, RwLock<BTreeMap<TxIdx, ()>>>,
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
            storage_wipes: DashMap::new(),
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
            AccountWrite::Updated(info) | AccountWrite::Recreated(info) => Some(info.clone()),
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
        if matches!(write, AccountWrite::Destroyed | AccountWrite::Recreated(_)) {
            self.storage_wipes
                .entry(addr)
                .or_insert_with(|| RwLock::new(BTreeMap::new()))
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(writer_idx, ());
            self.tx_writes
                .entry(writer_idx)
                .or_default()
                .push(WriteKey::StorageWipe(addr));
        }
    }

    /// Read storage slot visible to `reader_idx`.
    pub fn read_storage(&self, reader_idx: TxIdx, addr: Address, slot: U256) -> MvRead<U256> {
        let latest_slot = self.storage.get(&(addr, slot)).and_then(|entry| {
            let versions = entry.read().unwrap_or_else(|e| e.into_inner());
            versions
                .range(..reader_idx)
                .next_back()
                .map(|(&tx, &value)| (tx, value))
        });
        let latest_wipe = self.storage_wipes.get(&addr).and_then(|entry| {
            let versions = entry.read().unwrap_or_else(|e| e.into_inner());
            versions.range(..reader_idx).next_back().map(|(&tx, _)| tx)
        });

        match (latest_slot, latest_wipe) {
            (Some((slot_tx, _value)), Some(wipe_tx)) if wipe_tx > slot_tx => {
                MvRead::Value(wipe_tx, U256::ZERO)
            }
            (Some((slot_tx, value)), _) => MvRead::Value(slot_tx, value),
            (None, Some(wipe_tx)) => MvRead::Value(wipe_tx, U256::ZERO),
            (None, None) => MvRead::NotFound,
        }
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
        // Drop any deferred beneficiary delta from the prior execution.
        self.bene_deltas.remove(&tx_idx);
        let writes_count = if let Some((_, writes)) = self.tx_writes.remove(&tx_idx) {
            for w in &writes {
                match w {
                    WriteKey::Account(addr) => {
                        if let Some(entry) = self.accounts.get(addr) {
                            entry
                                .write()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&tx_idx);
                        }
                    }
                    WriteKey::Storage(addr, slot) => {
                        if let Some(entry) = self.storage.get(&(*addr, *slot)) {
                            entry
                                .write()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&tx_idx);
                        }
                    }
                    WriteKey::StorageWipe(addr) => {
                        if let Some(entry) = self.storage_wipes.get(addr) {
                            entry
                                .write()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&tx_idx);
                        }
                    }
                }
            }
            writes.len()
        } else {
            0
        };
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        metrics::histogram!("n42_mv_memory_clear_tx_ms").record(elapsed_ms);
        metrics::histogram!("n42_mv_memory_clear_tx_writes").record(writes_count as f64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u64) -> Address {
        Address::from_word(U256::from(n).into())
    }

    #[test]
    fn selfdestruct_wipe_shadows_older_slots_but_not_recreated_writes() {
        let mv = MvMemory::new();
        let account = addr(1);
        let old_slot = U256::from(7);
        let new_slot = U256::from(8);

        mv.write_storage(0, account, old_slot, U256::from(11));
        mv.write_account(1, account, &AccountWrite::Destroyed);

        assert!(matches!(
            mv.read_storage(2, account, old_slot),
            MvRead::Value(1, value) if value.is_zero()
        ));
        assert!(matches!(
            mv.read_storage(2, account, new_slot),
            MvRead::Value(1, value) if value.is_zero()
        ));

        mv.write_account(
            2,
            account,
            &AccountWrite::Recreated(AccountInfo::default()),
        );
        mv.write_storage(2, account, new_slot, U256::from(22));

        assert!(matches!(
            mv.read_storage(3, account, old_slot),
            MvRead::Value(2, value) if value.is_zero()
        ));
        assert!(matches!(
            mv.read_storage(3, account, new_slot),
            MvRead::Value(2, value) if value == U256::from(22)
        ));

        mv.clear_tx(1);
        assert!(matches!(
            mv.read_storage(3, account, old_slot),
            MvRead::Value(2, value) if value.is_zero()
        ));
        mv.clear_tx(2);
        assert!(matches!(
            mv.read_storage(3, account, old_slot),
            MvRead::Value(0, value) if value == U256::from(11)
        ));
    }

    #[test]
    fn recreated_account_hides_unmaterialized_base_storage() {
        let mv = MvMemory::new();
        let account = addr(2);
        let unread_base_slot = U256::from(99);

        mv.write_account(
            4,
            account,
            &AccountWrite::Recreated(AccountInfo::default()),
        );

        assert!(matches!(
            mv.read_storage(5, account, unread_base_slot),
            MvRead::Value(4, value) if value.is_zero()
        ));
    }
}
