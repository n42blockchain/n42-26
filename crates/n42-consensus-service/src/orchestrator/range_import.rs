//! Range catch-up import: stage timings (Stage 0) and the pipelined batch
//! import (Stage 1) of `docs/devlog-195`.
//!
//! The serial path imports one block at a time on the orchestrator task:
//! repeat decode, Profile-1 admission, payload build, `new_payload`,
//! `forkchoice_updated`, then the per-block notifications. The pipelined
//! path moves the decode and the payload build to a blocking worker that
//! runs ahead, hands admitted payloads to an ordered submitter task (the
//! engine validates them one at a time, in order), and issues one
//! forkchoice update per batch, to its last head-advancing block. Every
//! per-block notification (vote release, deferred finalization, lineage
//! proof) still runs once per block, in block order, after that update.
//!
//! `N42_RANGE_PIPELINE=<n>`: payloads prepared and admitted ahead of the
//! engine (default 4, at most 32); `0` keeps the serial path.
//!
//! Stage 2: the pipelined path also verifies the transaction senders of the
//! next `N42_RANGE_SENDER_AHEAD` blocks (default 8, at most 32; `0` off)
//! ahead of the engine, on the execution layer's own small pool
//! ([`n42_execution::SenderWarmup`]), into the caches the engine's payload
//! conversion reads. The window counts from the block the engine is on, so
//! the entries waiting for the engine stay bounded whatever the reply size.
//! Only verified senders are cached; a block with a bad signature meets the
//! engine's own verification and is refused as before.

use super::BlockDataBroadcast;
use super::ConsensusService;
use crate::el::ExecutionPath;
use crate::range_catchup::{Deactivation, ReadyBlock};
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ExecutionData, PayloadStatusEnum};
use metrics::{counter, histogram};
use n42_network::PeerId;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Default depth of the pipelined import.
const DEFAULT_RANGE_PIPELINE_DEPTH: usize = 4;

/// Default sender warm-up window, in blocks ahead of the engine.
const DEFAULT_RANGE_SENDER_AHEAD: usize = 8;

/// Largest sender warm-up window: at ~1,200 transactions a block, 32 blocks
/// are ~38k entries of the 131,072-entry secp256k1 cache.
pub(super) const MAX_RANGE_SENDER_AHEAD: usize = 32;

/// `N42_RANGE_SENDER_AHEAD`, read once.
pub(super) fn range_sender_ahead_from_env() -> usize {
    static AHEAD: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *AHEAD.get_or_init(|| {
        std::env::var("N42_RANGE_SENDER_AHEAD")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_RANGE_SENDER_AHEAD)
            .min(MAX_RANGE_SENDER_AHEAD)
    })
}

/// `N42_RANGE_PIPELINE`, read once.
pub(super) fn range_pipeline_depth_from_env() -> usize {
    static DEPTH: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *DEPTH.get_or_init(|| {
        std::env::var("N42_RANGE_PIPELINE")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_RANGE_PIPELINE_DEPTH)
            .min(32)
    })
}

/// Where the wall time of a range catch-up goes, per block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    /// Repeat decode of the buffered RLP (serial path, on the loop).
    Decode,
    /// Profile-1 header admission (on the loop).
    Admit,
    /// `ExecutionData` reconstruction (serial path, on the loop).
    Build,
    /// Awaiting `new_payload` (serial path).
    NewPayload,
    /// Pipelined path: the loop waiting for the next engine result or the
    /// next prepared payload (the engine, or the worker when it falls
    /// behind, is then the critical path).
    Wait,
    /// Awaiting `forkchoice_updated` (per block serial, per batch pipelined).
    Fcu,
    /// Notifications after the head moved: vote release, deferred
    /// finalization, lineage proof and state file, client bookkeeping.
    Post,
    /// Loop work inside an import batch not covered by a stage above.
    Other,
    /// Between import batches: the orchestrator idle or serving other
    /// events (fetch waits included).
    Gap,
    /// Pipelined path, off the loop: decode and payload build on the worker.
    Prep,
    /// Pipelined path, off the loop: `new_payload` as the submitter saw it.
    Engine,
    /// Pipelined path, off the loop: sender verification ahead of the
    /// engine (decode included), per block.
    Warm,
}

impl Stage {
    pub(super) const ALL: [Stage; 12] = [
        Stage::Decode,
        Stage::Admit,
        Stage::Build,
        Stage::NewPayload,
        Stage::Wait,
        Stage::Fcu,
        Stage::Post,
        Stage::Other,
        Stage::Gap,
        Stage::Prep,
        Stage::Engine,
        Stage::Warm,
    ];

    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Stage::Decode => "decode",
            Stage::Admit => "admit",
            Stage::Build => "build",
            Stage::NewPayload => "new_payload",
            Stage::Wait => "wait",
            Stage::Fcu => "fcu",
            Stage::Post => "post",
            Stage::Other => "other",
            Stage::Gap => "gap",
            Stage::Prep => "prep",
            Stage::Engine => "engine",
            Stage::Warm => "warm",
        }
    }

    /// Off the loop: overlapped with the loop's stages, not part of the
    /// wall-time sum.
    const fn overlapped(self) -> bool {
        matches!(self, Stage::Prep | Stage::Engine | Stage::Warm)
    }
}

/// Per-stage totals of one range catch-up run. The loop-side stages, the
/// residual `other` and the `gap` between batches add up to the run's wall
/// time from its first imported block to its last.
#[derive(Debug, Default)]
pub(super) struct CatchupTimings {
    totals_ms: [f64; Stage::ALL.len()],
    blocks: u64,
    batches: u64,
    fcus: u64,
    /// Senders the engine's payload conversion found cached, and verified
    /// itself, during the run's batches (process-wide counters, so another
    /// import running at the same time is counted too).
    engine_sender_hits: u64,
    engine_sender_misses: u64,
    /// Senders verified ahead of the engine, and those that failed there.
    warmed: u64,
    warm_failed: u64,
    started: Option<Instant>,
    last_end: Option<Instant>,
    /// Open batch: its start and the loop-side stage time recorded in it.
    section: Option<(Instant, f64)>,
}

impl CatchupTimings {
    /// Records `elapsed` under `stage`: always as a histogram sample, and
    /// in the run totals while a batch is open.
    pub(super) fn record(&mut self, stage: Stage, elapsed: Duration) {
        let ms = elapsed.as_secs_f64() * 1_000.0;
        histogram!("n42_range_catchup_stage_ms", "stage" => stage.as_str()).record(ms);
        if let Some((_, in_section)) = self.section.as_mut() {
            self.totals_ms[stage as usize] += ms;
            if !stage.overlapped() {
                *in_section += ms;
            }
        }
    }

    /// Opens a batch; the time since the previous batch is the gap.
    pub(super) fn begin_batch(&mut self) {
        let now = Instant::now();
        match self.last_end {
            Some(end) => {
                let gap = now.duration_since(end);
                self.section = Some((now, 0.0));
                self.record(Stage::Gap, gap);
                // The gap is not batch time.
                if let Some((_, in_section)) = self.section.as_mut() {
                    *in_section = 0.0;
                }
            }
            None => {
                self.started = Some(now);
                self.section = Some((now, 0.0));
            }
        }
        self.batches += 1;
    }

    /// Closes the batch: its time not covered by a stage is `other`.
    pub(super) fn end_batch(&mut self, imported: u64) {
        let Some((started, in_section)) = self.section else {
            return;
        };
        let now = Instant::now();
        let other_ms = (now.duration_since(started).as_secs_f64() * 1_000.0 - in_section).max(0.0);
        self.record(Stage::Other, Duration::from_secs_f64(other_ms / 1_000.0));
        self.blocks += imported;
        self.section = None;
        self.last_end = Some(now);
    }

    pub(super) fn note_fcu(&mut self) {
        self.fcus += 1;
    }

    /// The engine's sender lookups during one batch.
    pub(super) fn note_engine_senders(&mut self, counts: n42_execution::PayloadSenderCounts) {
        self.engine_sender_hits += counts.hits;
        self.engine_sender_misses += counts.misses;
        counter!("n42_range_catchup_engine_senders_total", "cache" => "hit").increment(counts.hits);
        counter!("n42_range_catchup_engine_senders_total", "cache" => "miss")
            .increment(counts.misses);
    }

    /// One block's sender warm-up.
    fn note_warm(&mut self, stats: n42_execution::WarmStats, elapsed: Duration) {
        self.record(Stage::Warm, elapsed);
        self.warmed += stats.recovered as u64;
        self.warm_failed += stats.failed as u64;
        counter!("n42_range_catchup_warmed_senders_total", "outcome" => "recovered")
            .increment(stats.recovered as u64);
        counter!("n42_range_catchup_warmed_senders_total", "outcome" => "cached")
            .increment(stats.cached as u64);
        counter!("n42_range_catchup_warmed_senders_total", "outcome" => "failed")
            .increment(stats.failed as u64);
    }

    /// Logs the run's breakdown and starts a new run.
    pub(super) fn log_and_reset(&mut self, head: u64) {
        let (Some(started), Some(ended)) = (self.started, self.last_end) else {
            *self = Self::default();
            return;
        };
        let wall_ms = ended.duration_since(started).as_secs_f64() * 1_000.0;
        let blocks = self.blocks.max(1) as f64;
        let attributed: f64 = Stage::ALL
            .iter()
            .filter(|stage| !stage.overlapped())
            .map(|stage| self.totals_ms[*stage as usize])
            .sum();
        let breakdown = Stage::ALL
            .iter()
            .filter(|stage| self.totals_ms[**stage as usize] > 0.0)
            .map(|stage| {
                let total = self.totals_ms[*stage as usize];
                format!("{}={:.0}ms/{:.2}", stage.as_str(), total, total / blocks)
            })
            .collect::<Vec<_>>()
            .join(" ");
        info!(
            target: "n42::interop::range",
            head,
            blocks = self.blocks,
            batches = self.batches,
            fcus = self.fcus,
            engine_sender_hits = self.engine_sender_hits,
            engine_sender_misses = self.engine_sender_misses,
            warmed = self.warmed,
            warm_failed = self.warm_failed,
            wall_ms = format!("{wall_ms:.0}"),
            ms_per_block = format!("{:.2}", wall_ms / blocks),
            attributed_ms = format!("{attributed:.0}"),
            stages = %breakdown,
            "range catch-up import timings (stage=total/mean per block)"
        );
        *self = Self::default();
    }

    #[cfg(test)]
    pub(super) fn total_ms(&self, stage: Stage) -> f64 {
        self.totals_ms[stage as usize]
    }

    #[cfg(test)]
    pub(super) fn engine_senders(&self) -> (u64, u64) {
        (self.engine_sender_hits, self.engine_sender_misses)
    }

    #[cfg(test)]
    pub(super) fn warmed(&self) -> (u64, u64) {
        (self.warmed, self.warm_failed)
    }

    #[cfg(test)]
    pub(super) fn wall_and_attributed_ms(&self) -> (f64, f64) {
        let wall = match (self.started, self.last_end) {
            (Some(started), Some(ended)) => ended.duration_since(started).as_secs_f64() * 1_000.0,
            _ => 0.0,
        };
        let attributed = Stage::ALL
            .iter()
            .filter(|stage| !stage.overlapped())
            .map(|stage| self.totals_ms[*stage as usize])
            .sum();
        (wall, attributed)
    }
}

/// How a batch ended.
pub(super) enum BatchEnd {
    /// Every ready block imported; the caller goes on.
    Done,
    /// A block failed and was handled; the caller returns.
    Return,
    /// A block failed and the client stopped.
    Stop(Deactivation),
}

/// How far the engine is through a batch: the window of the sender
/// warm-up counts from here.
#[derive(Default)]
struct EngineProgress {
    /// Blocks of the batch the engine has answered, and whether the batch
    /// is over.
    state: std::sync::Mutex<(usize, bool)>,
    changed: std::sync::Condvar,
}

impl EngineProgress {
    fn lock(&self) -> std::sync::MutexGuard<'_, (usize, bool)> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The engine has answered the first `count` blocks.
    fn answered(&self, count: usize) {
        let mut state = self.lock();
        state.0 = state.0.max(count);
        drop(state);
        self.changed.notify_all();
    }

    /// The batch is over: the warm-up stops.
    fn stop(&self) {
        self.lock().1 = true;
        self.changed.notify_all();
    }

    /// Blocks until block `index` is inside the window of `window` blocks
    /// from the one the engine is on. `Some(true)`: warm it; `Some(false)`:
    /// the engine is already past it; `None`: the batch is over.
    fn wait_for(&self, index: usize, window: usize) -> Option<bool> {
        let mut state = self.lock();
        loop {
            let (answered, stopped) = *state;
            if stopped {
                return None;
            }
            if index < answered + window {
                return Some(index >= answered);
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

/// Stops the warm-up when the batch ends, however it ends (a dropped
/// future included), so its blocking thread never waits on for good.
struct StopWarmup(std::sync::Arc<EngineProgress>);

impl Drop for StopWarmup {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// A block made ready off the loop.
struct Prepared {
    index: usize,
    outcome: Result<(n42_network::Gov5GossipBlock, ExecutionData), PrepError>,
}

enum PrepError {
    Decode(String),
    Build(String),
}

/// One block of a batch, as the loop tracks it.
struct BatchBlock {
    source: PeerId,
    number: u64,
    hash: B256,
    parent: B256,
    view: u64,
    timestamp: u64,
    loose_only: bool,
}

/// Why a block of the batch was not imported.
enum Failure {
    /// The buffered RLP did not decode again.
    Decode(String),
    /// Refused or not executed (admission, payload build, `new_payload`,
    /// or the batch's forkchoice update).
    NotExecuted,
}

impl ConsensusService {
    /// The view that orders a range block's execution: a certificate view
    /// when one bound its hash, the header view otherwise.
    pub(super) fn range_block_view(&self, block: &ReadyBlock) -> u64 {
        self.h2_v4_block_views
            .get(&block.hash)
            .copied()
            .filter(|view| *view > 0)
            .unwrap_or(block.view)
    }

    /// A block of the batch was not imported: the caller's handling of a
    /// failed block (E12 on a committed block, retry or stop).
    async fn range_block_failed(
        &mut self,
        number: u64,
        hash: B256,
        view: u64,
        loose_only: bool,
        failure: Failure,
        now: Instant,
    ) -> BatchEnd {
        match failure {
            Failure::Decode(error) => {
                warn!(target: "n42::interop::range", number, %error, "buffered range block failed repeat decoding");
            }
            Failure::NotExecuted => {
                warn!(
                    target: "n42::interop::range",
                    number,
                    %hash,
                    view,
                    "range catch-up block was not executed"
                );
                // E12 on a deferred chain: a block below a strict (commit)
                // anchor is committed. If this node refused it for carrying
                // a result other than its own, it halts with the evidence.
                if !loose_only
                    && self.profile1.is_some()
                    && self.profile1_check_committed(view, hash, None)
                {
                    return BatchEnd::Return;
                }
            }
        }
        match self.range_catchup.import_failed(number, now) {
            Some(reason) => BatchEnd::Stop(reason),
            None => BatchEnd::Return,
        }
    }

    /// Bookkeeping of an imported range block, after the head moved.
    fn range_block_imported(&mut self, number: u64, hash: B256, loose_only: bool) {
        self.range_catchup.imported(number, hash);
        // Below a strict (commit) anchor: committed, so `/1`, `/2` and
        // commit_qc may serve it once it is canonical.
        if !loose_only && self.h2_v4_identity.is_some() {
            self.network.note_committed_block(hash);
        }
        counter!("n42_range_catchup_imported_total").increment(1);
        if loose_only {
            counter!("n42_range_catchup_loose_only_total").increment(1);
        }
    }

    /// The serial import of a ready batch, block by block (the path before
    /// Stage 1, kept for `N42_RANGE_PIPELINE=0`).
    pub(super) async fn import_range_batch_serial(
        &mut self,
        ready: Vec<ReadyBlock>,
        now: Instant,
    ) -> (BatchEnd, u64) {
        let mut imported = 0;
        for block in ready {
            let started = Instant::now();
            let decoded = self.decode_wire_block(&block.rlp);
            self.catchup_timings
                .record(Stage::Decode, started.elapsed());
            let decoded = match decoded {
                Ok(decoded) => decoded,
                Err(error) => {
                    let view = block.view;
                    let end = self
                        .range_block_failed(
                            block.number,
                            block.hash,
                            view,
                            block.loose_only,
                            Failure::Decode(error.to_string()),
                            now,
                        )
                        .await;
                    return (end, imported);
                }
            };
            let view = self.range_block_view(&block);
            // `true` without a head move is a block at or below the
            // execution-validated view (a repeated header view): executed,
            // made canonical by the next forkchoice update. The reverse walk
            // treats it the same way.
            if !self
                .import_h2_v4_catchup_block(block.source, decoded, view)
                .await
            {
                let end = self
                    .range_block_failed(
                        block.number,
                        block.hash,
                        view,
                        block.loose_only,
                        Failure::NotExecuted,
                        now,
                    )
                    .await;
                return (end, imported);
            }
            let started = Instant::now();
            self.range_block_imported(block.number, block.hash, block.loose_only);
            self.catchup_timings.record(Stage::Post, started.elapsed());
            imported += 1;
        }
        (BatchEnd::Done, imported)
    }

    /// The pipelined import of a ready batch (Stage 1). Strictly in order:
    /// blocks are admitted, submitted and released in height order, and
    /// nothing after a failed block is submitted, made canonical or
    /// released.
    pub(super) async fn import_range_batch_pipelined(
        &mut self,
        mut ready: Vec<ReadyBlock>,
        now: Instant,
    ) -> (BatchEnd, u64) {
        let Some(el) = self.el.clone() else {
            return self.import_range_batch_serial(ready, now).await;
        };
        let depth = self.range_pipeline_depth.max(1);
        let profile1 = self.profile1.is_some();
        let mut blocks: Vec<BatchBlock> = ready
            .iter()
            .map(|block| BatchBlock {
                source: block.source,
                number: block.number,
                hash: block.hash,
                parent: block.parent,
                view: self.range_block_view(block),
                timestamp: 0,
                loose_only: block.loose_only,
            })
            .collect();
        let count = blocks.len();

        // Decode and payload build, ahead of the engine, on the blocking
        // pool. The bounded channel keeps it at most `depth` blocks ahead
        // of the loop; it stops at its next send once the loop is done.
        let rlps: std::sync::Arc<Vec<Vec<u8>>> = std::sync::Arc::new(
            ready
                .iter_mut()
                .map(|block| std::mem::take(&mut block.rlp))
                .collect(),
        );
        drop(ready);
        let progress = std::sync::Arc::new(EngineProgress::default());
        let stop_warmup = StopWarmup(progress.clone());
        let (warm_tx, warm_rx) = std::sync::mpsc::channel::<(n42_execution::WarmStats, Duration)>();
        if let Some(warmup) = el.sender_warmup().filter(|_| self.range_sender_ahead > 0) {
            // Sender verification for the blocks inside the window, ahead
            // of the engine, on the warm-up's own pool. Its own decode keeps
            // it independent of the payload worker's shorter lead.
            let (rlps, progress, window) =
                (rlps.clone(), progress.clone(), self.range_sender_ahead);
            tokio::task::spawn_blocking(move || {
                for (index, rlp) in rlps.iter().enumerate() {
                    match progress.wait_for(index, window) {
                        None => break,
                        Some(false) => continue,
                        Some(true) => {}
                    }
                    let started = Instant::now();
                    // A block that does not decode fails in the payload
                    // worker; there is nothing to warm.
                    let Ok(block) = n42_network::decode_block_rlp_for(rlp, profile1) else {
                        continue;
                    };
                    let stats = warmup.warm(&block.transactions);
                    if warm_tx.send((stats, started.elapsed())).is_err() {
                        break;
                    }
                }
            });
        }
        let (prep_tx, mut prep_rx) = mpsc::channel::<(Prepared, Duration)>(depth);
        let prep_rlps = rlps.clone();
        tokio::task::spawn_blocking(move || {
            for (index, rlp) in prep_rlps.iter().enumerate() {
                let started = Instant::now();
                let outcome = n42_network::decode_block_rlp_for(rlp, profile1)
                    .map_err(|error| PrepError::Decode(error.to_string()))
                    .and_then(|block| {
                        crate::replay_import::build_gov5_gossip_execution_data(&block)
                            .map(|data| (block, data))
                            .map_err(|error| PrepError::Build(error.to_string()))
                    });
                if prep_tx
                    .blocking_send((Prepared { index, outcome }, started.elapsed()))
                    .is_err()
                {
                    break;
                }
            }
        });

        // The ordered submitter: one `new_payload` at a time, in the order
        // received, and nothing more after the first non-Valid answer. The
        // adapter awaits before its engine send (import gate, parallel
        // import), so concurrent calls would not keep their order; the
        // engine validates one block at a time in any case.
        let (submit_tx, mut submit_rx) = mpsc::channel::<(usize, ExecutionData)>(depth);
        let (result_tx, mut result_rx) = mpsc::unbounded_channel();
        let submitter_progress = progress.clone();
        let submitter = tokio::spawn(async move {
            while let Some((index, data)) = submit_rx.recv().await {
                let started = Instant::now();
                let result = el
                    .new_payload_for(ExecutionPath::HISTORICAL_SEQUENTIAL, data)
                    .await;
                submitter_progress.answered(index + 1);
                let valid = matches!(&result, Ok(status) if matches!(status.status, PayloadStatusEnum::Valid));
                if result_tx.send((index, result, started.elapsed())).is_err() || !valid {
                    break;
                }
            }
        });

        let mut next_prepared = 0usize;
        let mut submitted = 0usize;
        let mut valid = 0usize;
        let mut failure: Option<(usize, Failure)> = None;
        loop {
            if valid == count || (failure.is_some() && valid == submitted) {
                break;
            }
            let feeding = failure.is_none() && next_prepared < count && submitted - valid < depth;
            let waited = Instant::now();
            tokio::select! {
                biased;
                Some((index, result, engine)) = result_rx.recv(), if submitted > valid => {
                    self.catchup_timings.record(Stage::Wait, waited.elapsed());
                    self.catchup_timings.record(Stage::Engine, engine);
                    let block = &blocks[index];
                    match result {
                        Ok(status) if matches!(status.status, PayloadStatusEnum::Valid) => {
                            info!(target: "n42::interop::h2v4", block_hash = %block.hash, block_number = block.number, view = block.view, "authenticated Gov5 catch-up parent reached new_payload(Valid)");
                            valid = index + 1;
                        }
                        Ok(status) => {
                            error!(target: "n42::interop::h2v4", source = %block.source, block_hash = %block.hash, block_number = block.number, view = block.view, status = ?status.status, "authenticated Gov5 catch-up stopped on non-Valid payload");
                            failure = Some((index, Failure::NotExecuted));
                            // Submitted blocks after it are never answered.
                            submitted = valid;
                        }
                        Err(error) => {
                            error!(target: "n42::interop::h2v4", source = %block.source, block_hash = %block.hash, block_number = block.number, view = block.view, %error, "authenticated Gov5 catch-up new_payload failed");
                            failure = Some((index, Failure::NotExecuted));
                            submitted = valid;
                        }
                    }
                }
                prepared = prep_rx.recv(), if feeding => {
                    self.catchup_timings.record(Stage::Wait, waited.elapsed());
                    let Some((Prepared { index, outcome }, prep)) = prepared else {
                        failure = Some((next_prepared, Failure::Decode("payload worker ended early".into())));
                        continue;
                    };
                    self.catchup_timings.record(Stage::Prep, prep);
                    next_prepared = index + 1;
                    let (block, data) = match outcome {
                        Ok(prepared) => prepared,
                        Err(PrepError::Decode(error)) => {
                            failure = Some((index, Failure::Decode(error)));
                            continue;
                        }
                        Err(PrepError::Build(error)) => {
                            let tracked = &blocks[index];
                            error!(target: "n42::interop::h2v4", source = %tracked.source, block_hash = %tracked.hash, block_number = tracked.number, view = tracked.view, %error, "could not reconstruct authenticated Gov5 catch-up payload");
                            failure = Some((index, Failure::NotExecuted));
                            continue;
                        }
                    };
                    let started = Instant::now();
                    let admitted = self.profile1_admit_block(&block, "catchup");
                    self.catchup_timings.record(Stage::Admit, started.elapsed());
                    if !admitted {
                        failure = Some((index, Failure::NotExecuted));
                        continue;
                    }
                    blocks[index].timestamp = block.header.timestamp;
                    drop(block);
                    if submit_tx.send((index, data)).await.is_err() {
                        failure = Some((index, Failure::NotExecuted));
                        continue;
                    }
                    submitted = index + 1;
                }
                else => {
                    // Nothing left to wait for (a closed result channel).
                    if failure.is_none() {
                        failure = Some((valid, Failure::NotExecuted));
                    }
                    break;
                }
            }
        }
        drop(submit_tx);
        drop(prep_rx);
        submitter.abort();
        drop(stop_warmup);
        // What the warm-up finished; a block it is still on is left out.
        for (stats, elapsed) in warm_rx.try_iter() {
            self.catchup_timings.note_warm(stats, elapsed);
        }

        // The head-advancing blocks of the valid prefix: a view above the
        // execution-validated view (and above every earlier block's in the
        // batch), as the serial path decides block by block.
        let mut high = self.execution_validated_head_view;
        let mut advancing = vec![false; valid];
        let mut last_advancing = None;
        for (index, block) in blocks[..valid].iter().enumerate() {
            if block.view > high {
                high = block.view;
                advancing[index] = true;
                last_advancing = Some(index);
            }
        }
        // One forkchoice update for the batch, to its last head-advancing
        // block; it makes every block below it canonical.
        let mut released = valid;
        if let Some(head) = last_advancing {
            let hash = blocks[head].hash;
            let forkchoice = self.fcu_state(hash, hash);
            let started = Instant::now();
            let result = match self.el.clone() {
                Some(el) => {
                    el.fork_choice_updated_for(ExecutionPath::HISTORICAL_SEQUENTIAL, forkchoice)
                        .await
                }
                None => Err(crate::el::ElError("no execution layer".into())),
            };
            self.catchup_timings.record(Stage::Fcu, started.elapsed());
            self.catchup_timings.note_fcu();
            let canonical = match result {
                Ok(result) if matches!(result.payload_status.status, PayloadStatusEnum::Valid) => {
                    true
                }
                Ok(result) => {
                    error!(target: "n42::interop::h2v4", block_hash = %hash, block_number = blocks[head].number, view = blocks[head].view, status = ?result.payload_status.status, "authenticated Gov5 catch-up FCU was not Valid");
                    false
                }
                Err(error) => {
                    error!(target: "n42::interop::h2v4", block_hash = %hash, block_number = blocks[head].number, view = blocks[head].view, %error, "authenticated Gov5 catch-up FCU failed");
                    false
                }
            };
            if !canonical {
                // Blocks before the first head-advancing one needed no
                // update; that block fails as the serial path's would.
                let first = advancing.iter().position(|advances| *advances).unwrap_or(0);
                released = first;
                if failure.is_none() || failure.as_ref().is_some_and(|(index, _)| *index > first) {
                    failure = Some((first, Failure::NotExecuted));
                }
            }
        }

        // Per block, in order: what the serial path does after its update.
        let started = Instant::now();
        for index in 0..released {
            let block = &blocks[index];
            let (hash, parent, view, number, timestamp) = (
                block.hash,
                block.parent,
                block.view,
                block.number,
                block.timestamp,
            );
            if advancing[index] {
                self.last_committed_timestamp = self.last_committed_timestamp.max(timestamp);
                self.advance_execution_validated_head(view, hash, "authenticated Gov5 catch-up");
                self.head_block_number = self.head_block_number.max(number);
                self.mark_catchup_commits_finalized(hash);
                self.prune_executed_h2_v4_catchup_history();
                crate::qualification_abort_at("execution_validated");
                let imported = BlockDataBroadcast {
                    block_hash: hash,
                    view,
                    payload_json: Vec::new(),
                    timestamp,
                    execution_output: None,
                    leader_ready_unix_ms: 0,
                    native_header_rlp: None,
                };
                self.complete_deferred_finalization(&imported).await;
            }
            if let Err(error) =
                self.engine
                    .process_event(n42_consensus::ConsensusEvent::BlockImportedWithParent {
                        block_hash: hash,
                        parent_hash: parent,
                    })
            {
                error!(target: "n42::interop::h2v4", block_hash = %hash, view, %error, "could not release execution-gated vote for Gov5 catch-up block");
            }
            let loose_only = blocks[index].loose_only;
            self.range_block_imported(number, hash, loose_only);
        }
        self.catchup_timings.record(Stage::Post, started.elapsed());
        histogram!("n42_range_catchup_batch_blocks").record(released as f64);

        match failure {
            Some((index, failure)) => {
                let block = &blocks[index];
                let (number, hash, view, loose_only) =
                    (block.number, block.hash, block.view, block.loose_only);
                let end = self
                    .range_block_failed(number, hash, view, loose_only, failure, now)
                    .await;
                (end, released as u64)
            }
            None => (BatchEnd::Done, released as u64),
        }
    }
}
