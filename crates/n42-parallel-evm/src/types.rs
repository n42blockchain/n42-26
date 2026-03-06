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

/// The origin of a value read during transaction execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOrigin {
    /// Read from the base (pre-block) state.
    Base,
    /// Read a value written by the given tx.
    Tx(TxIdx),
}

/// A single entry in a transaction's read set.
#[derive(Debug, Clone)]
pub struct ReadEntry {
    pub key: LocationKey,
    pub origin: ReadOrigin,
}

/// Per-transaction write to account info.
#[derive(Debug, Clone)]
pub enum AccountWrite {
    Updated(AccountInfo),
    Destroyed,
}

/// Error type for the parallel execution engine.
#[derive(Debug, thiserror::Error)]
pub enum ParallelEvmError {
    #[error("database error: {0}")]
    Database(String),
    #[error("execution did not converge after {0} rounds")]
    TooManyRounds(usize),
}
