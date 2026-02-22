use crate::{state_diff::StateDiff, witness::ExecutionWitness, N42EvmConfig};
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::{
    execute::{BlockExecutionError, BlockExecutionOutput, Executor},
    ConfigureEvm, Database,
};
use reth_primitives_traits::{NodePrimitives, RecoveredBlock};
use std::time::Instant;
use tracing::{debug, warn};

/// Result of block execution with witness data.
pub struct ExecutionWithWitness {
    /// Standard block execution output (bundle state + receipts).
    pub output: BlockExecutionOutput<<EthPrimitives as NodePrimitives>::Receipt>,
    /// Execution witness capturing all state accessed during execution.
    pub witness: ExecutionWitness,
    /// Wall-clock execution time in milliseconds.
    pub elapsed_ms: u64,
}

/// Result of block execution with both witness and state diff.
pub struct FullExecutionResult {
    /// Standard block execution output.
    pub output: BlockExecutionOutput<<EthPrimitives as NodePrimitives>::Receipt>,
    /// Execution witness for independent verification.
    pub witness: ExecutionWitness,
    /// State diff showing all changes made by this block.
    pub diff: StateDiff,
    /// Wall-clock execution time in milliseconds.
    pub elapsed_ms: u64,
}

/// Executes a block and captures the execution witness.
/// Uses `execute_with_state_closure` to access the state after execution.
/// Witness contains all state needed for independent re-execution.
pub fn execute_block_with_witness<DB: Database>(
    evm_config: &N42EvmConfig,
    db: DB,
    block: &RecoveredBlock<<EthPrimitives as NodePrimitives>::Block>,
) -> Result<ExecutionWithWitness, BlockExecutionError> {
    let start = Instant::now();
    debug!(target: "n42::execution", "execute_block_with_witness starting");

    let executor = evm_config.executor(db);
    let mut witness = ExecutionWitness::default();

    let output = executor.execute_with_state_closure(block, |state| {
        witness = ExecutionWitness::from_state(state);
    })?;

    let elapsed_ms = start.elapsed().as_millis() as u64;
    debug!(
        target: "n42::execution",
        elapsed_ms,
        witness_accounts = witness.hashed_state.accounts.len(),
        witness_codes = witness.codes.len(),
        "execute_block_with_witness complete"
    );

    Ok(ExecutionWithWitness { output, witness, elapsed_ms })
}

/// Executes a block and captures both witness and state diff.
/// Primary method for distribution nodes: execution output, witness, and diff.
pub fn execute_block_full<DB: Database>(
    evm_config: &N42EvmConfig,
    db: DB,
    block: &RecoveredBlock<<EthPrimitives as NodePrimitives>::Block>,
) -> Result<FullExecutionResult, BlockExecutionError> {
    let start = Instant::now();
    debug!(target: "n42::execution", "execute_block_full starting");

    let executor = evm_config.executor(db);
    let mut witness = ExecutionWitness::default();

    let output = executor.execute_with_state_closure(block, |state| {
        witness = ExecutionWitness::from_state(state);
    })?;

    let diff = StateDiff::from_bundle_state(&output.state);

    if witness.hashed_state.accounts.len() < diff.len() {
        warn!(
            target: "n42::execution",
            witness = witness.hashed_state.accounts.len(),
            diff = diff.len(),
            "witness accounts < diff accounts"
        );
    }

    let elapsed_ms = start.elapsed().as_millis() as u64;
    debug!(
        target: "n42::execution",
        elapsed_ms,
        witness_accounts = witness.hashed_state.accounts.len(),
        diff_accounts = diff.len(),
        "execute_block_full complete"
    );

    Ok(FullExecutionResult {
        output,
        witness,
        diff,
        elapsed_ms,
    })
}
