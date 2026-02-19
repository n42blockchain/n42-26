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
///
/// Uses `execute_with_state_closure` to access the EVM state after execution
/// (before `take_bundle()` consumes it) to record all accessed state for
/// witness generation.
///
/// The witness contains all state needed for an independent verifier to
/// re-execute the block without access to the full state trie.
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
///
/// This is the primary execution method for distribution nodes (IDC),
/// which need:
/// 1. The execution output for consensus (receipts, state root)
/// 2. The witness for mobile device verification
/// 3. The state diff for light node sync and audit
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

    // Extract state diff from the bundle state in the output
    let diff = StateDiff::from_bundle_state(&output.state);

    // Consistency check: witness accounts should be a superset of diff accounts.
    // The witness captures all state *accessed* (reads + writes), while the diff only
    // captures state that *changed* (writes). So witness.accounts >= diff.accounts.
    // This holds because any changed account must have been accessed during execution.
    if witness.hashed_state.accounts.len() < diff.len() {
        warn!(
            target: "n42::execution",
            witness = witness.hashed_state.accounts.len(),
            diff = diff.len(),
            "witness accounts fewer than diff accounts — possible inconsistency"
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
