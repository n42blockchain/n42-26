//! AM35 restart replay of the block journal.
//!
//! A whole-set power loss between the R2 votes for a block B and the first
//! member's reth save of B leaves every R2 voter locked on B while no
//! execution layer holds it. Each voter journalled B (and its unpersisted
//! ancestors) before signing its R2 vote. On restart, before the node serves,
//! builds or votes, this replay hands the journalled blocks the execution
//! layer lacks back to it with `new_payload`, in height order from the
//! persisted head, and moves forkchoice only up to the block the consensus
//! state says is committed. A locked but uncommitted B stays a validated
//! side block: the leader's build on `locked_qc.block_hash` (an FCU with
//! attributes) then finds it, and peers are served it from here.
//!
//! Records are matched to the execution layer by hash: a record is skipped
//! only when reth persisted that very block, and a record whose parent is a
//! persisted block (by hash) replays even below the execution head (a fork
//! sibling). A journalled block the consensus state rests on (the locked or
//! the committed block, or an ancestor of either) that cannot be replayed
//! after retries stops the node: it must never vote or build without it.

use super::{BlockDataBroadcast, ConsensusService, SharedBlockData};
use crate::el::ExecutionPath;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ExecutionData, PayloadStatusEnum};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

/// `new_payload` attempts per journalled block before a transient failure
/// (an EL error, or a non-final status such as SYNCING) is final.
const REPLAY_ATTEMPTS: u32 = 5;

/// Backoff before the first `new_payload` retry; doubles per attempt.
#[cfg(not(test))]
const REPLAY_RETRY_BACKOFF: Duration = Duration::from_millis(250);
#[cfg(test)]
const REPLAY_RETRY_BACKOFF: Duration = Duration::from_millis(1);

/// Why a journalled block the consensus state rests on was not replayed.
#[derive(Debug)]
pub(crate) struct ReplayFailure {
    pub(crate) hash: B256,
    pub(crate) number: u64,
    pub(crate) role: &'static str,
    pub(crate) reason: String,
}

impl std::fmt::Display for ReplayFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AM35: journalled {} block {} (height {}) could not be replayed into the execution layer: {}",
            self.role, self.hash, self.number, self.reason
        )
    }
}

/// One block the replay handed back to the execution layer.
struct Replayed {
    hash: B256,
    view: u64,
    number: u64,
    data: SharedBlockData,
    execution: Option<ExecutionData>,
}

impl ConsensusService {
    /// Re-imports journalled blocks the execution layer lacks. Returns how
    /// many blocks reached `new_payload` Valid, or the failure of a block the
    /// lock or the committed QC rests on (the caller stops the node). Runs
    /// once, before the event loop starts.
    pub(super) async fn replay_block_journal(&mut self) -> Result<usize, ReplayFailure> {
        let Some(journal) = self.block_journal.clone() else {
            return Ok(0);
        };
        let Some(el) = self.el.clone() else {
            return Ok(0);
        };
        let records = match journal.records() {
            Ok(records) => records,
            Err(error) => {
                // The open already dropped a torn tail; a read error here is
                // an I/O fault on frames the open accepted.
                return Err(ReplayFailure {
                    hash: B256::ZERO,
                    number: 0,
                    role: "any",
                    reason: format!("cannot read {}: {error}", journal.path().display()),
                });
            }
        };
        if records.is_empty() {
            return Ok(0);
        }
        let started = std::time::Instant::now();
        let committed_hash = self.engine.last_committed_qc().block_hash;
        let locked_hash = self.engine.locked_qc().block_hash;
        let el_head_number = self.head_block_number;
        // The blocks consensus rests on, and (below) their journalled
        // ancestors: each must be persisted or replayed.
        let mut required: HashMap<B256, &'static str> = HashMap::new();
        for (hash, role) in [(locked_hash, "locked"), (committed_hash, "committed")] {
            if hash != B256::ZERO {
                required.entry(hash).or_insert(role);
            }
        }
        let parents: HashMap<B256, B256> = records
            .iter()
            .map(|record| (record.block_hash, record.parent_hash))
            .collect();
        for (hash, role) in required.clone() {
            let mut current = parents.get(&hash).copied();
            while let Some(ancestor) = current {
                if !parents.contains_key(&ancestor) || required.contains_key(&ancestor) {
                    break;
                }
                required.insert(ancestor, role);
                current = parents.get(&ancestor).copied();
            }
        }
        let numbers: HashMap<B256, u64> = records
            .iter()
            .map(|record| (record.block_hash, record.number))
            .collect();
        // Blocks a journalled block may build on: the execution head, every
        // block replayed so far, and any block reth persisted (by hash).
        let mut known: HashSet<B256> = HashSet::from([self.head_block_hash]);
        let mut failures: HashMap<B256, String> = HashMap::new();
        let mut replayed: Vec<Replayed> = Vec::new();
        let total = records.len();
        for record in records {
            let hash = record.block_hash;
            if journal.is_persisted(hash, record.number) {
                // reth persisted this very block.
                known.insert(hash);
                continue;
            }
            let parent_persisted = record
                .number
                .checked_sub(1)
                .is_some_and(|number| journal.is_persisted(record.parent_hash, number));
            if !known.contains(&record.parent_hash) && !parent_persisted {
                let reason = failures.get(&record.parent_hash).map_or_else(
                    || {
                        format!(
                            "parent {} is neither in the execution layer nor replayed",
                            record.parent_hash
                        )
                    },
                    |parent_failure| {
                        format!(
                            "parent {} was not replayed: {parent_failure}",
                            record.parent_hash
                        )
                    },
                );
                warn!(target: "n42::cl::block_journal", %hash, number = record.number, parent = %record.parent_hash, %reason, "journalled block not replayed");
                failures.insert(hash, reason);
                continue;
            }
            let broadcast = match bincode::deserialize::<BlockDataBroadcast>(&record.block_data) {
                Ok(broadcast) if broadcast.block_hash == hash => broadcast,
                Ok(_) | Err(_) => {
                    error!(target: "n42::cl::block_journal", %hash, "journalled block data does not decode to its block");
                    failures.insert(hash, "block data does not decode to its block".to_string());
                    continue;
                }
            };
            let native_header = broadcast
                .native_header_rlp
                .clone()
                .or_else(|| record.native_header.clone());
            if self.profile1.is_some() {
                if !self.profile1_admit_hash(hash, native_header.as_deref(), "block_journal_replay")
                {
                    error!(target: "n42::cl::block_journal", %hash, number = record.number, "Profile 1 refused the header of a journalled block; not replayed");
                    failures.insert(hash, "Profile 1 refused its header".to_string());
                    continue;
                }
            } else if let Some(raw) = native_header.as_deref()
                && !super::remember_direct_native_header(hash, raw)
            {
                warn!(target: "n42::cl::block_journal", %hash, "journalled block carries an invalid native header");
            }
            let execution = match super::decompress_payload(&broadcast.payload_json)
                .and_then(super::decode_execution_payload_owned)
            {
                Ok(execution) if execution.block_hash() == hash => execution,
                Ok(_) | Err(_) => {
                    error!(target: "n42::cl::block_journal", %hash, "journalled payload does not decode to its block");
                    failures.insert(hash, "payload does not decode to its block".to_string());
                    continue;
                }
            };
            let parent_hash = execution.parent_hash();
            let number = execution.block_number();
            let timestamp = execution.payload.timestamp();
            let has_staking_target = super::execution_might_contain_staking_target(&execution);
            let serve_copy = self.h2_v4_identity.is_some().then(|| execution.clone());
            // Transient outcomes (an EL error, SYNCING/ACCEPTED) are retried
            // with backoff; INVALID is final.
            let mut outcome = Err(String::new());
            for attempt in 0..REPLAY_ATTEMPTS {
                if attempt > 0 {
                    tokio::time::sleep(REPLAY_RETRY_BACKOFF * 2u32.pow(attempt - 1)).await;
                }
                match el
                    .new_payload_for(ExecutionPath::HISTORICAL_SEQUENTIAL, execution.clone())
                    .await
                {
                    Ok(status) if matches!(status.status, PayloadStatusEnum::Valid) => {
                        outcome = Ok(());
                        break;
                    }
                    Ok(status) if matches!(status.status, PayloadStatusEnum::Invalid { .. }) => {
                        outcome = Err(format!("execution layer returned {:?}", status.status));
                        break;
                    }
                    Ok(status) => {
                        warn!(target: "n42::cl::block_journal", %hash, number, attempt, status = ?status.status, "journalled block not validated yet; retrying");
                        outcome = Err(format!("execution layer returned {:?}", status.status));
                    }
                    Err(error) => {
                        warn!(target: "n42::cl::block_journal", %hash, number, attempt, %error, "new_payload failed for a journalled block; retrying");
                        outcome = Err(format!("new_payload failed: {error}"));
                    }
                }
            }
            if let Err(reason) = outcome {
                error!(target: "n42::cl::block_journal", %hash, number, %reason, attempts = REPLAY_ATTEMPTS, "journalled block not replayed");
                failures.insert(hash, reason);
                continue;
            }
            known.insert(hash);
            journal.note_lineage(hash, parent_hash, number);
            let data: SharedBlockData = Arc::new(record.block_data);
            self.cache_pending_block_data_with_metadata(
                hash,
                Arc::clone(&data),
                &[hash],
                record.view,
                broadcast.payload_json.len(),
            );
            self.record_eager_import_completion(
                hash,
                timestamp,
                parent_hash,
                number,
                has_staking_target,
                None,
            );
            self.finish_eager_import_completion(hash, parent_hash, number, has_staking_target)
                .await;
            replayed.push(Replayed {
                hash,
                view: record.view,
                number,
                data,
                execution: serve_copy,
            });
        }

        // Fail closed: a journalled block the lock or the commit rests on
        // that is still missing would leave this node voting and building
        // without it.
        let mut missing: Vec<_> = failures
            .iter()
            .filter_map(|(hash, reason)| {
                required.get(hash).map(|role| ReplayFailure {
                    hash: *hash,
                    number: numbers.get(hash).copied().unwrap_or_default(),
                    role,
                    reason: reason.clone(),
                })
            })
            .collect();
        missing.sort_by_key(|failure| failure.number);
        if let Some(first) = missing.into_iter().next() {
            error!(target: "n42::cl::block_journal", hash = %first.hash, number = first.number, role = first.role, reason = %first.reason, "AM35: a journalled block the consensus state rests on could not be replayed; stopping");
            return Err(first);
        }

        // Forkchoice moves only to what consensus committed. A locked block
        // that never got its CommitQC stays a validated side block.
        let committed = replayed
            .iter()
            .find(|block| block.hash == committed_hash)
            .map(|block| (block.view, block.number));
        if let Some((view, number)) = committed {
            let forkchoice = self.fcu_state(committed_hash, committed_hash);
            match el
                .fork_choice_updated_for(ExecutionPath::HISTORICAL_SEQUENTIAL, forkchoice)
                .await
            {
                Ok(result) if matches!(result.payload_status.status, PayloadStatusEnum::Valid) => {
                    self.head_block_number = self.head_block_number.max(number);
                    self.advance_execution_validated_head(
                        view,
                        committed_hash,
                        "block journal replay",
                    );
                    self.mark_catchup_commits_finalized(committed_hash);
                }
                Ok(result) => {
                    warn!(target: "n42::cl::block_journal", %committed_hash, status = ?result.payload_status.status, "forkchoice to the replayed committed block was not Valid");
                }
                Err(error) => {
                    warn!(target: "n42::cl::block_journal", %committed_hash, %error, "forkchoice to the replayed committed block failed");
                }
            }
        }

        // Serve the uncommitted blocks: a peer that did not vote for them
        // fetches them by hash (Profile 1) or takes the block data (native).
        let committed_view = committed.map(|(view, _)| view).unwrap_or(0);
        for block in replayed.iter().filter(|block| block.view > committed_view) {
            if let Some(execution) = block.execution.as_ref() {
                match n42_network::encode_gov5_block_rlp(execution) {
                    Ok(rlp) => {
                        if let Err(error) = self.network.broadcast_gov5_block_reliable(rlp).await {
                            warn!(target: "n42::cl::block_journal", hash = %block.hash, %error, "could not republish a replayed block");
                        }
                    }
                    Err(error) => {
                        warn!(target: "n42::cl::block_journal", hash = %block.hash, %error, "could not encode a replayed block for peers");
                    }
                }
            } else if let Err(error) = self
                .network
                .announce_block_reliable(block.data.as_ref().clone())
                .await
            {
                warn!(target: "n42::cl::block_journal", hash = %block.hash, %error, "could not republish a replayed block");
            }
        }

        metrics::counter!("n42_block_journal_replayed_blocks_total")
            .increment(replayed.len() as u64);
        info!(
            target: "n42::cl::block_journal",
            journalled = total,
            replayed = replayed.len(),
            el_head_number,
            %committed_hash,
            %locked_hash,
            locked_replayed = replayed.iter().any(|block| block.hash == locked_hash),
            committed_replayed = committed.is_some(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "AM35: block journal replayed into the execution layer"
        );
        // A commit vote held before the restart cannot exist (the engine is
        // fresh), but the replayed data may complete one raised by events
        // queued meanwhile.
        self.retry_held_commit_vote();
        Ok(replayed.len())
    }
}
