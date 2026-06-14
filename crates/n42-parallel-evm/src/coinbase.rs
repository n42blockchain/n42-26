//! Deferred coinbase: credit the block beneficiary's gas fees as a commutative
//! accumulator instead of a versioned write, so independent txs no longer
//! cascade-abort on the shared coinbase (devlog-67). This module owns the whole
//! concern — the per-block decision, the per-tx delta extraction, and the
//! commit-time materialization — so the orchestration and tx-execution code only
//! hold an opaque `Option<&DeferredCoinbase>`.

use crate::ParallelExecutionOutput;
use crate::types::ParallelEvmError;
use alloy_primitives::{Address, U256};
use revm::context::{BlockEnv, TxEnv};
use revm::database_interface::DatabaseRef;
use revm::state::{Account, AccountInfo, TransactionId};
use std::fmt;

/// How a block's beneficiary should be handled under parallel execution.
pub(crate) enum CoinbasePlan {
    /// Accumulate the beneficiary's gas-fee credits commutatively (the
    /// beneficiary is a deferrable EOA non-sender — the production case).
    Defer(DeferredCoinbase),
    /// Non-deferred parallel path (`N42_DEFERRED_COINBASE=0`, a debug knob): the
    /// beneficiary is a normal versioned account.
    NoDefer,
    /// The beneficiary is NOT deferrable (it is a tx sender, or a contract that a
    /// tx could CALL): neither parallel path reproduces sequential crediting
    /// exactly, so the caller must run the whole block sequentially.
    Sequential,
}

/// Deferral state for one block: the beneficiary address + its block-start
/// account. Per-tx deltas are accumulated in [`MvMemory`](crate::mv_memory) and
/// summed here at commit.
pub(crate) struct DeferredCoinbase {
    bene: Address,
    base: Option<AccountInfo>,
}

impl DeferredCoinbase {
    /// Decide how to handle the beneficiary for this block.
    ///
    /// Sound deferral requires the beneficiary's balance change to be a pure
    /// commutative increment: excluded when it is a tx sender (nonce changes) or a
    /// contract (a CALL could decrease its balance with nonce/code unchanged,
    /// which the `new - base` delta would saturate to 0). A validator reward
    /// address is an EOA non-sender, so `Defer` is the normal outcome.
    pub(crate) fn plan<DB>(
        txs: &[TxEnv],
        base_db: &DB,
        block_env: &BlockEnv,
    ) -> Result<CoinbasePlan, ParallelEvmError>
    where
        DB: DatabaseRef,
        DB::Error: fmt::Display,
    {
        let enabled = std::env::var("N42_DEFERRED_COINBASE")
            .map(|v| v != "0")
            .unwrap_or(true);
        if !enabled {
            return Ok(CoinbasePlan::NoDefer);
        }

        let bene = block_env.beneficiary;
        let base = base_db
            .basic_ref(bene)
            .map_err(|e| ParallelEvmError::Database(e.to_string()))?;
        let is_sender = txs.iter().any(|t| t.caller == bene);
        // No base account (absent) ⇒ empty code ⇒ EOA.
        let is_eoa = base.as_ref().is_none_or(|i| i.is_empty_code_hash());

        if is_sender || !is_eoa {
            Ok(CoinbasePlan::Sequential)
        } else {
            Ok(CoinbasePlan::Defer(DeferredCoinbase { bene, base }))
        }
    }

    /// The deferred beneficiary address (read "blind" by [`ParallelDb`], so the
    /// fee credit creates no read/write dependency).
    ///
    /// [`ParallelDb`]: crate::parallel_db::ParallelDb
    pub(crate) fn address(&self) -> Address {
        self.bene
    }

    /// Given a tx's post-state beneficiary `account`, return `Some(delta)` to
    /// record commutatively, or `None` if it must instead be a normal versioned
    /// write (nonce/code changed — defensive; the EOA guard makes this unreachable
    /// for the deferred path in practice).
    pub(crate) fn extract_delta(&self, account: &Account) -> Option<U256> {
        let base = self.base.clone().unwrap_or_default();
        let only_balance =
            account.info.nonce == base.nonce && account.info.code_hash == base.code_hash;
        only_balance.then(|| account.info.balance.saturating_sub(base.balance))
    }

    /// Materialize the final beneficiary = `base + sum` into the block output, once
    /// at commit. Order-independent (addition commutes).
    pub(crate) fn materialize(&self, sum: U256, output: &mut ParallelExecutionOutput) {
        if sum.is_zero() {
            return;
        }
        let mut info = self.base.clone().unwrap_or_default();
        info.balance = info.balance.saturating_add(sum);
        let tx_id = TransactionId::new(0).expect("0 is a valid TransactionId");
        let account = output
            .state_changes
            .entry(self.bene)
            .or_insert_with(|| Account::new_not_existing(tx_id));
        account.info = info;
        account.mark_touch();
    }
}
