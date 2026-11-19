use crate::{N42EvmConfig, state_diff::StateDiff, witness::ExecutionWitness};
use evm2::evm::{CacheDB, Database, Db};
use metrics::histogram;
use n42_tx_types::N42Primitives;
use reth_evm::{
    ConfigureEvm,
    execute::{BlockExecutionError, BlockExecutionOutput, Executor},
};
use reth_execution_types::BundleSource;
use reth_primitives_traits::{NodePrimitives, RecoveredBlock};
use std::time::Instant;
use tracing::{debug, warn};

type Output = BlockExecutionOutput<<N42Primitives as NodePrimitives>::Receipt>;

/// Executes a block over a cache of everything it reads, then folds the
/// block's changes into that cache: the state the witness is recorded from.
fn execute_recording<DB: Database>(
    evm_config: &N42EvmConfig,
    db: DB,
    block: &RecoveredBlock<<N42Primitives as NodePrimitives>::Block>,
) -> Result<(Output, ExecutionWitness), BlockExecutionError> {
    let mut state = CacheDB::new(Db::new(db));
    let output = evm_config.executor(&mut state).execute(block)?;
    state.commit_source(&BundleSource(&output.state));
    let witness = ExecutionWitness::from_state(&state);
    Ok((output, witness))
}

/// Result of block execution with witness data.
pub struct ExecutionWithWitness {
    /// Standard block execution output (bundle state + receipts).
    pub output: BlockExecutionOutput<<N42Primitives as NodePrimitives>::Receipt>,
    /// Execution witness capturing all state accessed during execution.
    pub witness: ExecutionWitness,
    /// Wall-clock execution time in milliseconds.
    pub elapsed_ms: u64,
}

/// Result of block execution with both witness and state diff.
pub struct FullExecutionResult {
    /// Standard block execution output.
    pub output: BlockExecutionOutput<<N42Primitives as NodePrimitives>::Receipt>,
    /// Execution witness for independent verification.
    pub witness: ExecutionWitness,
    /// State diff showing all changes made by this block.
    pub diff: StateDiff,
    /// Wall-clock execution time in milliseconds.
    pub elapsed_ms: u64,
}

/// Executes a block and captures the execution witness.
/// Executes over a cache of the block's reads to access the state after execution.
/// Witness contains all state needed for independent re-execution.
pub fn execute_block_with_witness<DB: Database>(
    evm_config: &N42EvmConfig,
    db: DB,
    block: &RecoveredBlock<<N42Primitives as NodePrimitives>::Block>,
) -> Result<ExecutionWithWitness, BlockExecutionError> {
    let start = Instant::now();
    debug!(target: "n42::execution", "execute_block_with_witness starting");

    let (output, witness) = execute_recording(evm_config, db, block)?;

    let elapsed_ms = start.elapsed().as_millis() as u64;
    histogram!("n42_execution_block_ms").record(elapsed_ms as f64);
    debug!(
        target: "n42::execution",
        elapsed_ms,
        witness_accounts = witness.hashed_state.accounts.len(),
        witness_codes = witness.codes.len(),
        "execute_block_with_witness complete"
    );

    Ok(ExecutionWithWitness {
        output,
        witness,
        elapsed_ms,
    })
}

/// Executes a block and captures both witness and state diff.
/// Primary method for distribution nodes: execution output, witness, and diff.
pub fn execute_block_full<DB: Database>(
    evm_config: &N42EvmConfig,
    db: DB,
    block: &RecoveredBlock<<N42Primitives as NodePrimitives>::Block>,
) -> Result<FullExecutionResult, BlockExecutionError> {
    let start = Instant::now();
    debug!(target: "n42::execution", "execute_block_full starting");

    let (output, witness) = execute_recording(evm_config, db, block)?;

    let diff = StateDiff::from_bundle_state(&output.state);

    if witness.hashed_state.accounts.len() < diff.len() {
        // This can happen legitimately when accounts are first created (CREATE
        // opcode) or coinbase rewards are applied without an explicit read — the
        // account appears in diff but not in the read-set captured by from_state.
        warn!(
            target: "n42::execution",
            witness = witness.hashed_state.accounts.len(),
            diff = diff.len(),
            "witness accounts < diff accounts"
        );
        histogram!("n42_execution_witness_gap")
            .record((diff.len() - witness.hashed_state.accounts.len()) as f64);
    }

    let elapsed_ms = start.elapsed().as_millis() as u64;
    histogram!("n42_execution_block_ms").record(elapsed_ms as f64);
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
