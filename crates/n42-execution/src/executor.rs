use crate::{N42EvmConfig, state_diff::StateDiff, witness::ExecutionWitness};
use alloy_evm::ToTxEnv;
use metrics::histogram;
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::{
    ConfigureEvm, Database,
    execute::{BlockExecutionError, BlockExecutionOutput, Executor},
};
use reth_primitives_traits::{NodePrimitives, RecoveredBlock};
use revm::database_interface::DatabaseRef;
use std::time::Instant;
use tracing::{debug, info, warn};

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

// ---------------------------------------------------------------------------
// Parallel EVM execution (Block-STM)
// ---------------------------------------------------------------------------

/// Returns true if parallel EVM execution is enabled via `N42_PARALLEL_EVM=1`.
///
/// Disabled by default because under 48K-cap workloads (simple transfers),
/// MVCC overhead exceeds sequential execution time.
pub fn parallel_evm_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("N42_PARALLEL_EVM")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Summary of a parallel block execution run.
#[derive(Debug, Clone)]
pub struct ParallelExecutionSummary {
    /// Number of transactions in the block.
    pub tx_count: usize,
    /// Total gas consumed across all transactions.
    pub total_gas: u64,
    /// Number of transactions that executed successfully.
    pub success_count: usize,
    /// Wall-clock execution time in milliseconds.
    pub elapsed_ms: u64,
}

/// Executes a block using Block-STM parallel execution.
///
/// Enable with `N42_PARALLEL_EVM=1`. Disabled by default because under
/// 48K-cap workloads (simple transfers), MVCC overhead exceeds execution time.
/// Beneficial for blocks with complex contract interactions.
///
/// This function provides a **standalone** parallel execution result.
/// It does **not** replace reth's built-in sequential executor.
/// Callers choose which path to use.
pub fn execute_block_parallel<DB: DatabaseRef + Send + Sync>(
    evm_config: &N42EvmConfig,
    db: &DB,
    block: &RecoveredBlock<<EthPrimitives as NodePrimitives>::Block>,
) -> Result<ParallelExecutionSummary, BlockExecutionError>
where
    DB::Error: core::error::Error + Send + Sync + 'static,
{
    let start = Instant::now();

    // 1. Derive CfgEnv + BlockEnv from the block header.
    let evm_env = evm_config
        .evm_env(block.header())
        .map_err(BlockExecutionError::other)?;

    // 2. Convert each recovered transaction to a revm TxEnv.
    let txs: Vec<revm::context::TxEnv> = block
        .transactions_recovered()
        .map(|recovered| recovered.to_tx_env())
        .collect();
    let tx_count = txs.len();
    debug!(target: "n42::execution", tx_count, "execute_block_parallel starting");

    // 3. Run parallel execution.
    let output = n42_parallel_evm::parallel_execute(&txs, db, evm_env.cfg_env, evm_env.block_env)
        .map_err(BlockExecutionError::other)?;

    // 4. Build summary.
    let total_gas: u64 = output.results.iter().map(|r| r.gas_used).sum();
    let success_count = output.results.iter().filter(|r| r.success).count();
    let elapsed_ms = start.elapsed().as_millis() as u64;

    histogram!("n42_parallel_execution_block_ms").record(elapsed_ms as f64);
    info!(
        target: "n42::execution",
        tx_count,
        total_gas,
        success_count,
        elapsed_ms,
        "execute_block_parallel complete"
    );

    Ok(ParallelExecutionSummary {
        tx_count,
        total_gas,
        success_count,
        elapsed_ms,
    })
}
