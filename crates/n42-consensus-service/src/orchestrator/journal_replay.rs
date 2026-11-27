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

use super::{BlockDataBroadcast, ConsensusService, SharedBlockData};
use crate::el::ExecutionPath;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ExecutionData, PayloadStatusEnum};
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{error, info, warn};

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
    /// many blocks reached `new_payload` Valid. Runs once, before the event
    /// loop starts.
    pub(super) async fn replay_block_journal(&mut self) -> usize {
        let Some(journal) = self.block_journal.clone() else {
            return 0;
        };
        let Some(el) = self.el.clone() else {
            return 0;
        };
        let records = match journal.records() {
            Ok(records) => records,
            Err(error) => {
                error!(target: "n42::cl::block_journal", %error, path = %journal.path().display(), "cannot read the block journal; continuing without replay");
                return 0;
            }
        };
        if records.is_empty() {
            return 0;
        }
        let started = std::time::Instant::now();
        let committed_hash = self.engine.last_committed_qc().block_hash;
        let locked_hash = self.engine.locked_qc().block_hash;
        let el_head_number = self.head_block_number;
        // Blocks a journalled block may build on: the execution head, then
        // every block replayed so far.
        let mut known: HashSet<B256> = HashSet::from([self.head_block_hash]);
        let mut replayed: Vec<Replayed> = Vec::new();
        let total = records.len();
        for record in records {
            let hash = record.block_hash;
            if record.number <= el_head_number {
                // Persisted by the execution layer (or a fork below its head).
                continue;
            }
            if !known.contains(&record.parent_hash) {
                warn!(target: "n42::cl::block_journal", %hash, number = record.number, parent = %record.parent_hash, "journalled block does not extend the execution head or a replayed block; not replayed");
                continue;
            }
            let broadcast = match bincode::deserialize::<BlockDataBroadcast>(&record.block_data) {
                Ok(broadcast) if broadcast.block_hash == hash => broadcast,
                Ok(_) | Err(_) => {
                    error!(target: "n42::cl::block_journal", %hash, "journalled block data does not decode to its block");
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
                    continue;
                }
            };
            let parent_hash = execution.parent_hash();
            let number = execution.block_number();
            let timestamp = execution.payload.timestamp();
            let has_staking_target = super::execution_might_contain_staking_target(&execution);
            let serve_copy = self.h2_v4_identity.is_some().then(|| execution.clone());
            match el
                .new_payload_for(ExecutionPath::HISTORICAL_SEQUENTIAL, execution)
                .await
            {
                Ok(status) if matches!(status.status, PayloadStatusEnum::Valid) => {}
                Ok(status) => {
                    error!(target: "n42::cl::block_journal", %hash, number, status = ?status.status, "execution layer did not validate a journalled block");
                    continue;
                }
                Err(error) => {
                    error!(target: "n42::cl::block_journal", %hash, number, %error, "new_payload failed for a journalled block");
                    continue;
                }
            }
            known.insert(hash);
            crate::block_journal::note_block_lineage(hash, parent_hash, number);
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
        replayed.len()
    }
}
