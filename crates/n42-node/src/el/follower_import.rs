//! Another node's block executed with the parallel transfer batches before
//! the engine sees it (`N42_PARALLEL_IMPORT=1`).
//!
//! The block's leading plain transfers run in sender batches over the
//! block's parent (`n42_execution::parallel_build::import_payload`), and the
//! output is filed in the payload cache under the block's hash. The engine's
//! `newPayload` then takes it as it takes a builder's output: it does not
//! execute the block again, and it checks the output as it checks its own
//! execution -- gas, receipts, requests and the state root against the
//! header. Nothing here validates a block; an output the engine refuses is
//! dropped and the block is submitted again for the engine to execute.
//!
//! A block the batches do not take (it opens with something other than a
//! plain transfer, a fork they do not reproduce, a pre-execution change to
//! one of their accounts, a parent this node does not hold after a short
//! wait, [`parent_wait`], a block the engine already holds) is submitted
//! with no output, as without this module.
//!
//! A block's parent is read where the engine has it: the caller keeps a
//! block's execution behind its parent's import
//! ([`FollowerImport::prepare_after`]), so the parent has been validated,
//! and is in the engine tree, by the time its state is opened.

use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::{ExecutionData, PayloadStatus};
use n42_execution::{N42EvmConfig, parallel_build};
use n42_tx_types::Receipt;
use reth_evm::database::StateProviderDatabase;
use reth_execution_types::BlockExecutionOutput;
use reth_storage_api::{StateProvider, StateProviderBox};
use std::{future::Future, sync::Arc};

/// What the engine takes from the payload cache.
type CachedExecution = (BlockExecutionOutput<Receipt>, Vec<Address>);

/// Opens the state a block hash names, when this node holds it.
type OpenState = dyn Fn(B256) -> Option<StateProviderBox> + Send + Sync;

/// Executes other nodes' blocks ahead of the engine.
#[derive(Clone)]
pub struct FollowerImport {
    config: N42EvmConfig,
    open: Arc<OpenState>,
}

impl std::fmt::Debug for FollowerImport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FollowerImport").finish_non_exhaustive()
    }
}

impl FollowerImport {
    /// The import over `config`, the engine's own EVM configuration, reading
    /// parents through `open`.
    pub fn new(
        config: N42EvmConfig,
        open: impl Fn(B256) -> Option<StateProviderBox> + Send + Sync + 'static,
    ) -> Self {
        Self {
            config,
            open: Arc::new(open),
        }
    }

    /// [`Self::new`] when `N42_PARALLEL_IMPORT=1`.
    pub fn configured(
        config: N42EvmConfig,
        open: impl Fn(B256) -> Option<StateProviderBox> + Send + Sync + 'static,
    ) -> Option<Self> {
        std::env::var("N42_PARALLEL_IMPORT")
            .is_ok_and(|value| value == "1")
            .then(|| Self::new(config, open))
    }

    /// Executes `payload` and files its output for the engine. Returns
    /// whether an output was filed. `false` leaves the payload cache as it
    /// was: the block is the engine's to execute.
    pub fn prepare(&self, payload: &ExecutionData) -> bool {
        self.prepare_after(payload, || ())
    }

    /// [`Self::prepare`], running `parent_imported` before the parent's
    /// state is opened: the caller returns from it once no import of the
    /// parent is under way on this node. The work that does not read state
    /// is done before it.
    pub fn prepare_after(&self, payload: &ExecutionData, parent_imported: impl FnOnce()) -> bool {
        let hash = payload.block_hash();
        // A block this node built, or one that arrived with its leader's
        // output, already has its output filed: that one stands.
        if let Some(held) = reth_evm::payload_cache::take_payload_execution::<CachedExecution>(&hash)
        {
            reth_evm::payload_cache::store_payload_execution(hash, held);
            return false;
        }
        // A block the engine already holds (a second import of it) is
        // answered from the tree: executing it again would only take the
        // cores from the next block and leave an output nobody takes.
        if !payload.payload.transactions().is_empty() && (self.open)(hash).is_some() {
            metrics::counter!("n42_parallel_import_declined_total", "reason" => "known")
                .increment(1);
            return false;
        }
        // The transactions' root, beside the execution: the engine's
        // conversion of the payload otherwise derives it on one thread while
        // nothing else of the block is left to do (65 ms of a full block's
        // 190 in the engine). It reads the root from the cache the builder's
        // own blocks use, and still checks the block's hash over it.
        let root_job = (!payload.payload.transactions().is_empty())
            .then(|| {
                let transactions = payload.payload.transactions().clone();
                std::thread::Builder::new()
                    .name("import-tx-root".into())
                    .spawn(move || alloy_consensus::proofs::ordered_trie_root_encoded(&transactions))
                    .ok()
            })
            .flatten();
        let filed = self.execute(payload, parent_imported);
        // A block left to the engine is converted beside its execution
        // there: the job is not waited for.
        if filed && let Some(job) = root_job {
            let waited = std::time::Instant::now();
            if let Ok(root) = job.join() {
                reth_evm::payload_cache::store_payload_transactions_root(hash, root);
            }
            metrics::histogram!("n42_parallel_import_batch_stage_ms", "stage" => "tx_root_wait")
                .record(waited.elapsed().as_secs_f64() * 1_000.0);
        }
        filed
    }

    /// [`Self::prepare`]'s execution: whether an output was filed.
    fn execute(&self, payload: &ExecutionData, parent_imported: impl FnOnce()) -> bool {
        let hash = payload.block_hash();
        let started = std::time::Instant::now();
        let declined = |reason: &'static str| {
            metrics::counter!("n42_parallel_import_declined_total", "reason" => reason)
                .increment(1);
            false
        };
        if payload.payload.transactions().is_empty() {
            return declined("empty");
        }
        let Ok((transactions, senders)) = self.config.recover_payload_transactions(payload) else {
            // The engine reports what is wrong with the payload.
            return declined("senders");
        };
        let recover_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let parent = payload.parent_hash();
        parent_imported();
        let (main, waited) = wait_for(|| (self.open)(parent), parent_wait());
        let parent_wait_ms = waited.as_secs_f64() * 1_000.0;
        let Some(main) = main else {
            metrics::histogram!("n42_parallel_import_parent_wait_ms", "outcome" => "timeout")
                .record(parent_wait_ms);
            tracing::warn!(
                target: "n42::el",
                %hash,
                number = payload.block_number(),
                %parent,
                parent_wait_ms,
                "parallel import declined: neither the provider nor the engine tree holds the parent's state; the engine executes the block"
            );
            return declined("parent-state");
        };
        metrics::histogram!("n42_parallel_import_parent_wait_ms", "outcome" => "ready")
            .record(parent_wait_ms);
        let open = || {
            Some(StateProviderDatabase::new(
                (self.open)(parent)?.into_evm_state_provider(),
            ))
        };
        let imported = parallel_build::import_payload(
            &self.config,
            payload,
            &transactions,
            &senders,
            StateProviderDatabase::new(main.into_evm_state_provider()),
            &open,
        );
        match imported {
            Ok((output, stats)) => {
                reth_evm::payload_cache::store_payload_execution(hash, (output, senders));
                let batch_ms = started.elapsed().as_secs_f64() * 1_000.0;
                metrics::counter!("n42_parallel_import_executed_total")
                    .increment(stats.txs as u64);
                metrics::histogram!("n42_parallel_import_batch_ms").record(batch_ms);
                for (stage, ms) in [
                    ("recover", recover_ms),
                    ("parent_wait", parent_wait_ms),
                    ("partition", stats.phases.partition_ms as f64),
                    ("execute", stats.phases.groups_ms as f64),
                    ("collect", stats.phases.collect_ms as f64),
                    ("finish", stats.finish_us as f64 / 1_000.0),
                ] {
                    metrics::histogram!("n42_parallel_import_batch_stage_ms", "stage" => stage)
                        .record(ms);
                }
                tracing::debug!(
                    target: "n42::el",
                    %hash,
                    txs = stats.txs,
                    tail = stats.tail,
                    batches = stats.phases.batches,
                    accounts = stats.accounts,
                    cached = stats.cached,
                    direct_hits = stats.phases.direct_hits,
                    recover_ms,
                    parent_wait_ms,
                    batch_ms,
                    "parallel import executed; the engine validates the output"
                );
                true
            }
            Err(refused) => {
                tracing::debug!(target: "n42::el", %hash, %refused, "parallel import declined; the engine executes the block");
                declined(refused.reason())
            }
        }
    }

    /// Drops the output filed for `hash` when the engine did not take it,
    /// and the transactions' root filed with it.
    pub fn discard(hash: &B256) {
        drop(reth_evm::payload_cache::take_payload_execution::<CachedExecution>(hash));
        reth_evm::payload_cache::remove_payload_transactions_root(hash);
    }
}

/// How long [`FollowerImport::prepare`] waits for the parent's state
/// (`N42_PARALLEL_IMPORT_PARENT_WAIT_MS`, 60 by default; `0` asks once).
///
/// The provider answers for a block once it is canonical. The engine holds
/// a block from the moment its `newPayload` returns, but the forkchoice
/// update that makes it canonical is sent by the consensus loop some 20 to
/// 35 ms later when blocks follow each other closely, and the next block's
/// import has started by then: without the wait every such block was
/// declined and executed serially by the engine, which also kept the next
/// forkchoice update queued behind it.
fn parent_wait() -> std::time::Duration {
    static WAIT: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *WAIT.get_or_init(|| {
        let ms = std::env::var("N42_PARALLEL_IMPORT_PARENT_WAIT_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(60u64)
            .min(1_000);
        std::time::Duration::from_millis(ms)
    })
}

/// Asks `open` until it answers or `limit` has passed, a millisecond apart,
/// and returns the answer with the time spent. The state is whatever the
/// provider holds under the parent's hash: nothing else is ever read.
fn wait_for<T>(
    mut open: impl FnMut() -> Option<T>,
    limit: std::time::Duration,
) -> (Option<T>, std::time::Duration) {
    let started = std::time::Instant::now();
    loop {
        if let Some(state) = open() {
            return (Some(state), started.elapsed());
        }
        if started.elapsed() >= limit {
            return (None, started.elapsed());
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Submits a payload whose output [`FollowerImport::prepare`] filed. An
/// `INVALID` answer may be a verdict on that output rather than on the block
/// (the engine does not remember the hash for a refused output), so the
/// output is dropped and the payload submitted once more, which the engine
/// then executes itself: that second answer is the block's.
pub(super) async fn submit_prepared<F, E>(
    hash: B256,
    submit: impl Fn() -> F,
) -> Result<PayloadStatus, E>
where
    F: Future<Output = Result<PayloadStatus, E>>,
{
    let first = submit().await;
    // Whatever the answer, nothing of this block stays filed.
    FollowerImport::discard(&hash);
    match first {
        Ok(status) if status.is_invalid() => {
            metrics::counter!("n42_parallel_import_resubmitted_total").increment(1);
            tracing::warn!(
                target: "n42::el",
                %hash,
                %status,
                "payload refused with the parallel import's output; submitting it for the engine to execute"
            );
            submit().await
        }
        answer => answer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_rpc_types_engine::PayloadStatusEnum;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn invalid() -> PayloadStatus {
        PayloadStatus::from_status(PayloadStatusEnum::Invalid {
            validation_error: "cached execution output refused: receipts root".to_owned(),
        })
    }

    fn valid(hash: B256) -> PayloadStatus {
        PayloadStatus::new(PayloadStatusEnum::Valid, Some(hash))
    }

    /// The parent's state is taken as soon as the provider has it, and the
    /// wait gives up at its limit; no wait asks once.
    #[test]
    fn the_parent_wait_takes_the_state_when_it_appears_and_gives_up_at_the_limit() {
        use std::time::Duration;
        let asked = AtomicUsize::new(0);
        let appears = || (asked.fetch_add(1, Ordering::SeqCst) >= 3).then_some(7u8);
        let (state, _) = wait_for(appears, Duration::from_secs(5));
        assert_eq!(state, Some(7));
        assert_eq!(asked.load(Ordering::SeqCst), 4);

        let asked = AtomicUsize::new(0);
        let never = || {
            asked.fetch_add(1, Ordering::SeqCst);
            None::<u8>
        };
        let (state, waited) = wait_for(never, Duration::from_millis(20));
        assert_eq!(state, None);
        assert!(waited >= Duration::from_millis(20));
        assert!(asked.load(Ordering::SeqCst) >= 2);

        let asked = AtomicUsize::new(0);
        let never = || {
            asked.fetch_add(1, Ordering::SeqCst);
            None::<u8>
        };
        assert_eq!(wait_for(never, Duration::ZERO).0, None);
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    /// A refused output ends in the engine's own answer, after exactly one
    /// more submission, with nothing left filed.
    #[tokio::test]
    async fn a_refused_output_is_dropped_and_the_payload_submitted_once_more() {
        let hash = B256::repeat_byte(0x71);
        reth_evm::payload_cache::store_payload_execution(
            hash,
            (BlockExecutionOutput::<Receipt>::default(), Vec::<Address>::new()),
        );
        let calls = AtomicUsize::new(0);
        let answer = submit_prepared(hash, || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move { Ok::<_, ()>(if n == 0 { invalid() } else { valid(hash) }) }
        })
        .await;
        assert_eq!(answer, Ok(valid(hash)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            reth_evm::payload_cache::take_payload_execution::<CachedExecution>(&hash).is_none()
        );
    }

    /// A block that is invalid stays invalid: one more submission, no loop.
    #[tokio::test]
    async fn an_invalid_block_is_submitted_twice_and_no_more() {
        let hash = B256::repeat_byte(0x72);
        let calls = AtomicUsize::new(0);
        let answer = submit_prepared(hash, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok::<_, ()>(invalid()) }
        })
        .await;
        assert_eq!(answer, Ok(invalid()));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// A valid answer, a syncing one and an engine error are the answer.
    #[tokio::test]
    async fn any_other_answer_is_returned_after_one_submission() {
        let hash = B256::repeat_byte(0x73);
        for first in [
            Ok(valid(hash)),
            Ok(PayloadStatus::from_status(PayloadStatusEnum::Syncing)),
            Err(()),
        ] {
            let calls = AtomicUsize::new(0);
            let answer = submit_prepared(hash, || {
                calls.fetch_add(1, Ordering::SeqCst);
                let first = first.clone();
                async move { first }
            })
            .await;
            assert_eq!(answer, first);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
}
