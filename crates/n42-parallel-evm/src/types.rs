//! Shared types for the parallel EVM execution engine.

use alloy_primitives::{Address, U256};
use revm::state::AccountInfo;

/// Transaction index within a block.
pub type TxIdx = usize;

/// Identifies a piece of state accessed by the EVM.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LocationKey {
    /// Account basic info (balance, nonce, code_hash).
    Account(Address),
    /// Storage slot.
    Storage(Address, U256),
}

/// The value a tx observed for an account read — the fields that affect
/// execution. Storing the value (not just the writer) lets validation detect a
/// lower tx that RE-EXECUTED and wrote a *different* value under the same index:
/// writer-identity validation would wrongly pass such a stale read (the
/// non-deterministic hot-account bug; see `docs/devlog-67`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSnapshot {
    pub balance: U256,
    pub nonce: u64,
    pub code_hash: alloy_primitives::B256,
}

impl AccountSnapshot {
    /// Snapshot the execution-relevant fields of an account read.
    pub fn of(info: &Option<AccountInfo>) -> Option<Self> {
        info.as_ref().map(|i| Self {
            balance: i.balance,
            nonce: i.nonce,
            code_hash: i.code_hash,
        })
    }
}

/// The value observed by a single read, recorded for value-based validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadValue {
    /// Account basic info (None ⇒ account absent).
    Account(Option<AccountSnapshot>),
    /// Storage slot value.
    Storage(U256),
}

/// A single entry in a transaction's read set: the key and the value observed.
#[derive(Debug, Clone)]
pub struct ReadEntry {
    pub key: LocationKey,
    pub value: ReadValue,
}

/// Per-transaction write to account info.
#[derive(Debug, Clone)]
pub enum AccountWrite {
    Updated(AccountInfo),
    /// A newly created account whose address may have existed before an
    /// earlier SELFDESTRUCT. Its entire prior storage domain is zero before
    /// the transaction's explicit slot writes are applied.
    Recreated(AccountInfo),
    Destroyed,
}

/// Error type for the parallel execution engine.
#[derive(Debug, thiserror::Error)]
pub enum ParallelEvmError {
    #[error("database error: {0}")]
    Database(String),
    /// Currently unused: convergence failure falls back to sequential execution.
    /// Retained for future use if callers want explicit error reporting.
    #[error("execution did not converge after {0} rounds")]
    TooManyRounds(usize),
    /// revm 40's TransactionId is backed by NonMaxU32, so blocks cannot carry
    /// more than `u32::MAX - 1` transactions through the parallel engine.
    #[error("block has too many transactions ({tx_count}); parallel engine supports < u32::MAX")]
    BlockTooLarge { tx_count: usize },
}
