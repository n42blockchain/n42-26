//! Per-tx execution output and final block-output assembly.

use crate::types::{AccountWrite, ParallelEvmError, ReadEntry, TxIdx};
use crate::{ParallelExecutionOutput, TxResult};
use alloy_primitives::{Address, Log, U256};
use parking_lot::Mutex;
use revm::state::{Account, EvmStorageSlot, TransactionId};
use std::collections::HashMap;

/// Internal per-tx output (before merging into the block state).
pub(crate) struct TxOutputInternal {
    pub(crate) gas_used: u64,
    pub(crate) success: bool,
    pub(crate) logs: Vec<Log>,
    pub(crate) account_writes: Vec<(Address, AccountWrite)>,
    pub(crate) storage_writes: Vec<(Address, U256, U256)>,
    pub(crate) read_set: Vec<ReadEntry>,
    /// Commutative balance delta this tx credited to the deferred beneficiary
    /// (gas fee + value sent to it), if deferral is active. Applied at commit.
    pub(crate) bene_delta: Option<U256>,
}

/// Merge a single transaction's writes into the accumulated block state.
pub(crate) fn merge_tx_state(
    state_changes: &mut HashMap<Address, Account>,
    tx_idx: TxIdx,
    account_writes: Vec<(Address, AccountWrite)>,
    storage_writes: Vec<(Address, U256, U256)>,
) {
    // revm 40 tracks the originating transaction via a TransactionId (NonMaxU32).
    // The parallel_execute entry rejects blocks with >= u32::MAX txs, so tx_idx
    // is always a valid NonMaxU32 here.
    let tx_id = TransactionId::new(tx_idx)
        .expect("tx_idx < u32::MAX guaranteed by parallel_execute entry check");
    for (addr, write) in account_writes {
        let account = state_changes
            .entry(addr)
            .or_insert_with(|| Account::new_not_existing(tx_id));
        match write {
            AccountWrite::Updated(info) => {
                account.info = info;
                account.mark_touch();
            }
            AccountWrite::Destroyed => {
                account.mark_selfdestruct();
            }
        }
    }
    for (addr, slot, value) in storage_writes {
        let account = state_changes
            .entry(addr)
            .or_insert_with(|| Account::new_not_existing(tx_id));
        account
            .storage
            .insert(slot, EvmStorageSlot::new_changed(U256::ZERO, value, tx_id));
    }
}

/// Assemble the final block output from the per-tx result slots (block order).
pub(crate) fn build_output(
    outputs: Vec<Mutex<Option<TxOutputInternal>>>,
) -> Result<ParallelExecutionOutput, ParallelEvmError> {
    let mut results = Vec::with_capacity(outputs.len());
    let mut state_changes: HashMap<Address, Account> = HashMap::new();

    for (tx_idx, output_mutex) in outputs.into_iter().enumerate() {
        let output = output_mutex
            .into_inner()
            .ok_or_else(|| ParallelEvmError::Database(format!("missing output for tx {tx_idx}")))?;

        results.push(TxResult {
            gas_used: output.gas_used,
            success: output.success,
            logs: output.logs,
        });

        merge_tx_state(
            &mut state_changes,
            tx_idx,
            output.account_writes,
            output.storage_writes,
        );
    }

    Ok(ParallelExecutionOutput {
        results,
        state_changes,
    })
}
