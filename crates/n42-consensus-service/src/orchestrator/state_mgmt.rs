use super::{BlockDataBroadcast, CommittedBlock, ConsensusService};
use crate::persistence::{self, ConsensusSnapshot};
use alloy_primitives::B256;
use metrics::counter;
use n42_consensus::{validator_changes_hash, verify_commit_qc};
use n42_network::{BlockSyncResponse, MAX_BLOCKS_PER_SYNC_REQUEST, PeerId, SyncBlock};
use n42_primitives::QuorumCertificate;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

// ── Configuration constants ──
//
// These are read once at first access and cached for the process lifetime.
// Reading env vars on every call (which happens each view change / slot boundary)
// would issue a syscall on every hot-path invocation; LazyLock eliminates that.

static CFG_SYNC_BUFFER_SIZE: LazyLock<usize> = LazyLock::new(|| {
    std::env::var("N42_SYNC_BUFFER_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000)
});

static CFG_SYNC_TIMEOUT_SECS: LazyLock<u64> = LazyLock::new(|| {
    std::env::var("N42_SYNC_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30)
});

static CFG_MAX_EMPTY_SKIPS: LazyLock<u32> = LazyLock::new(|| {
    std::env::var("N42_MAX_EMPTY_SKIPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3)
});

/// Maximum committed blocks retained in the ring buffer for sync serving.
/// At 8-second slots, 10,000 blocks ≈ ~22 hours of history.
/// Configurable via `N42_SYNC_BUFFER_SIZE` (read once at startup).
pub(super) fn max_committed_blocks() -> usize {
    *CFG_SYNC_BUFFER_SIZE
}

/// Timeout for a state sync request; resets the in-flight flag on expiry.
/// Configurable via `N42_SYNC_TIMEOUT_SECS` (read once at startup).
pub(super) fn sync_request_timeout() -> Duration {
    Duration::from_secs(*CFG_SYNC_TIMEOUT_SECS)
}

/// Maximum consecutive empty-block skips before producing a block for liveness.
/// At 8s slots, 3 skips = 24 seconds max gap.
/// Configurable via `N42_MAX_EMPTY_SKIPS` (read once at startup).
pub(super) fn max_consecutive_empty_skips() -> u32 {
    *CFG_MAX_EMPTY_SKIPS
}

// ── State persistence ──

impl ConsensusService {
    /// Collects the staged epoch transition info from the epoch manager for persistence.
    fn collect_scheduled_epoch(&self) -> Option<(u64, Vec<n42_chainspec::ValidatorInfo>, u32)> {
        self.engine
            .epoch_manager()
            .staged_epoch_info()
            .map(|(epoch, validators, f)| (epoch, validators.to_vec(), f))
    }

    /// Builds a snapshot of the current consensus state for persistence.
    fn build_snapshot(&self) -> ConsensusSnapshot {
        let em = self.engine.epoch_manager();
        let current_epoch_validators = if em.epochs_enabled() {
            let vs = em.current_validator_set();
            Some((
                em.current_epoch(),
                vs.validator_infos(),
                vs.fault_tolerance(),
            ))
        } else {
            None
        };
        ConsensusSnapshot {
            version: 4,
            current_view: self.engine.current_view(),
            locked_qc: self.engine.locked_qc().clone(),
            last_committed_qc: self.engine.last_committed_qc().clone(),
            consecutive_timeouts: self.engine.consecutive_timeouts(),
            scheduled_epoch_transition: self.collect_scheduled_epoch(),
            authorized_verifiers: Vec::new(),
            committed_block_count: self.committed_block_count,
            last_voted_view: self.engine.last_voted_view(),
            current_epoch_validators,
            execution_validated_head_view: self.execution_validated_head_view,
            execution_validated_head_hash: self.head_block_hash,
        }
    }

    /// Persists the current consensus state to disk.
    /// Called after each BlockCommitted event and on graceful shutdown.
    pub(super) fn save_consensus_state(&self) {
        let path = match &self.state_file {
            Some(p) => p,
            None => return,
        };

        let snapshot = self.build_snapshot();
        if let Err(e) = persistence::save_consensus_state(path, &snapshot) {
            error!(target: "n42::cl::sync", error = %e, "failed to save consensus state");
        } else {
            debug!(target: "n42::cl::sync", view = snapshot.current_view, "consensus state persisted");
        }

        // Also persist staking state
        if let Some(ref staking_sink) = self.staking_sink {
            staking_sink.save();
        }
    }

    /// Persists consensus state on shutdown, including consecutive_timeouts.
    pub(super) fn save_shutdown_state(&self) {
        let path = match &self.state_file {
            Some(p) => p,
            None => return,
        };

        let snapshot = self.build_snapshot();
        if let Err(e) = persistence::save_consensus_state(path, &snapshot) {
            error!(target: "n42::cl::sync", error = %e, "failed to persist final consensus state on shutdown");
        } else {
            info!(target: "n42::cl::sync", view = snapshot.current_view, "final consensus state persisted");
        }
    }

    /// Stores a committed block in the ring buffer for serving sync requests.
    pub(super) fn store_committed_block(
        &mut self,
        view: u64,
        block_hash: B256,
        commit_qc: QuorumCertificate,
        validator_changes: Option<Vec<n42_primitives::consensus::ValidatorChange>>,
    ) {
        let payload = self
            .pending_block_data
            .get(&block_hash)
            .and_then(|data| bincode::deserialize::<BlockDataBroadcast>(data).ok())
            .map(|b| b.payload_json)
            .unwrap_or_default();

        if self.committed_blocks.len() >= max_committed_blocks() {
            self.committed_blocks.pop_front();
        }
        self.committed_blocks.push_back(CommittedBlock {
            view,
            block_hash,
            commit_qc,
            payload,
            validator_changes,
        });
    }

    /// Backfills validator changes for an already-cached committed block.
    ///
    /// This is used when a follower commits a block from Decide first and only
    /// later recovers the matching Proposal carrying validator changes. The
    /// sync ring buffer must be patched so joining nodes can verify the CommitQC
    /// against the correct `validator_changes_hash`.
    pub(super) fn recover_committed_block_validator_changes(
        &mut self,
        view: u64,
        block_hash: B256,
        validator_changes: Vec<n42_primitives::consensus::ValidatorChange>,
    ) {
        if validator_changes.is_empty() {
            return;
        }

        if let Some(block) = self
            .committed_blocks
            .iter_mut()
            .find(|b| b.view == view && b.block_hash == block_hash)
        {
            block.validator_changes = Some(validator_changes);
            debug!(
                target: "n42::cl::sync",
                view,
                %block_hash,
                "patched committed block validator_changes in sync cache"
            );
            return;
        }

        warn!(
            target: "n42::cl::sync",
            view,
            %block_hash,
            "late validator_changes recovery could not find committed block in sync cache"
        );
    }
}

// ── State sync ──

impl ConsensusService {
    /// Requests an execution lineage from every connected peer.
    ///
    /// This is narrower than ordinary consensus catch-up: a committed payload
    /// returned `Syncing`, so the local reth tree is known to be missing an
    /// ancestor. A single peer may have an incomplete retained payload ring or
    /// an unready request-response stream; fan-out avoids a full sync timeout
    /// while every response remains QC-verified and idempotent.
    pub(super) fn initiate_execution_catchup_sync(&mut self, local_view: u64, target_view: u64) {
        if self.sync_in_flight {
            debug!(
                target: "n42::cl::sync",
                local_view,
                target_view,
                "execution catch-up sync already in flight"
            );
            return;
        }

        let peers: Vec<_> = self.connected_peers.iter().copied().collect();
        if peers.is_empty() {
            warn!(target: "n42::cl::sync", "no connected peers for execution catch-up sync");
            return;
        }

        let capped_to_view = target_view.min(local_view + MAX_BLOCKS_PER_SYNC_REQUEST);
        let request = n42_network::BlockSyncRequest {
            from_view: local_view + 1,
            to_view: capped_to_view,
            local_committed_view: local_view,
        };
        let mut sent = 0usize;
        let mut requested_peers = std::collections::HashSet::new();
        for peer in peers {
            match self.network.request_sync(peer, request.clone()) {
                Ok(()) => {
                    sent += 1;
                    requested_peers.insert(peer);
                }
                Err(error) => {
                    warn!(target: "n42::cl::sync", %peer, %error, "failed to send execution catch-up request");
                }
            }
        }
        if sent == 0 {
            return;
        }

        info!(
            target: "n42::cl::sync",
            local_view,
            target_view = capped_to_view,
            peers = sent,
            "initiating execution catch-up sync"
        );
        self.sync_in_flight = true;
        self.sync_started_at = Some(Instant::now());
        self.sync_request_range = Some((request.from_view, capped_to_view));
        self.sync_requested_peers = requested_peers;
    }

    /// Initiates a state sync request to a connected peer.
    /// Uses deterministic peer rotation by view number to avoid always hitting the same peer.
    pub(super) fn initiate_sync(&mut self, local_view: u64, target_view: u64) {
        if self.sync_in_flight {
            let timed_out = self
                .sync_started_at
                .map(|t| t.elapsed() > sync_request_timeout())
                .unwrap_or(false);

            if timed_out {
                warn!(
                    target: "n42::cl::sync",
                    elapsed_secs = self.sync_started_at.map_or(0, |t| t.elapsed().as_secs()),
                    "sync request timed out, resetting"
                );
                self.sync_in_flight = false;
                self.sync_started_at = None;
                self.sync_request_range = None;
                self.sync_requested_peers.clear();
            } else {
                debug!(target: "n42::cl::sync", local_view, target_view, "sync already in flight, skipping");
                return;
            }
        }

        let peers: Vec<_> = self.connected_peers.iter().copied().collect();
        if peers.is_empty() {
            warn!(target: "n42::cl::sync", "no connected peers for sync");
            return;
        }
        let peer = peers[self.sync_peer_cursor % peers.len()];
        self.sync_peer_cursor = self.sync_peer_cursor.wrapping_add(1);

        info!(target: "n42::cl::sync", %peer, local_view, target_view, "initiating state sync");

        // Cap to_view so the request doesn't exceed MAX_BLOCKS_PER_SYNC_REQUEST.
        // The peer will reject oversized ranges; capping here avoids a wasted round-trip.
        let capped_to_view = target_view.min(local_view + MAX_BLOCKS_PER_SYNC_REQUEST);
        let request = n42_network::BlockSyncRequest {
            from_view: local_view + 1,
            to_view: capped_to_view,
            local_committed_view: local_view,
        };

        let request_range = (request.from_view, request.to_view);
        if let Err(e) = self.network.request_sync(peer, request) {
            error!(target: "n42::cl::sync", error = %e, "failed to send sync request");
            return;
        }

        self.sync_in_flight = true;
        self.sync_started_at = Some(Instant::now());
        self.sync_request_range = Some(request_range);
        self.sync_requested_peers.clear();
        self.sync_requested_peers.insert(peer);
    }

    /// Handles an incoming sync request from a peer.
    /// Looks up committed blocks in the ring buffer and responds with up to 128 blocks.
    pub(super) async fn handle_sync_request(
        &self,
        peer: PeerId,
        request_id: u64,
        request: n42_network::BlockSyncRequest,
    ) {
        counter!("n42_sync_requests_served_total").increment(1);

        // Validate the request range to prevent malicious peers from requesting
        // unbounded ranges.
        if let Err(reason) = request.validate() {
            warn!(target: "n42::cl::sync", %peer, %reason, "rejecting invalid sync request");
            return;
        }

        debug!(
            target: "n42::cl::sync",
            %peer,
            from_view = request.from_view,
            to_view = request.to_view,
            "handling sync request"
        );

        const MAX_SYNC_BLOCKS: usize = 128;

        let blocks: Vec<SyncBlock> = self
            .committed_blocks
            .iter()
            .filter(|b| b.view >= request.from_view && b.view <= request.to_view)
            .take(MAX_SYNC_BLOCKS)
            .map(|b| SyncBlock {
                view: b.view,
                block_hash: b.block_hash,
                commit_qc: b.commit_qc.clone(),
                payload: b.payload.clone(),
                validator_changes: b.validator_changes.clone(),
            })
            .collect();

        let peer_committed_view = self.committed_blocks.back().map(|b| b.view).unwrap_or(0);

        debug!(target: "n42::cl::sync", %peer, blocks_sent = blocks.len(), peer_committed_view, "sending sync response");

        let response = BlockSyncResponse {
            blocks,
            peer_committed_view,
        };

        if let Err(e) = self
            .network
            .send_sync_response_reliable(request_id, response)
            .await
        {
            error!(target: "n42::cl::sync", error = %e, "failed to send sync response");
        }
    }

    /// Handles a sync response containing blocks from a peer.
    /// Verifies QC validity, imports each block into reth, and requests more if still behind.
    pub(super) async fn handle_sync_response(&mut self, peer: PeerId, response: BlockSyncResponse) {
        let Some((requested_from, requested_to)) = self.sync_request_range else {
            warn!(
                target: "n42::cl::sync",
                %peer,
                "dropping unsolicited sync response with no active request"
            );
            counter!("n42_sync_responses_unsolicited_total").increment(1);
            return;
        };
        if !self.sync_requested_peers.remove(&peer) {
            warn!(
                target: "n42::cl::sync",
                %peer,
                requested_from,
                requested_to,
                "dropping sync response from a peer outside the active request group"
            );
            counter!("n42_sync_responses_unsolicited_total").increment(1);
            return;
        }

        info!(
            target: "n42::cl::sync",
            %peer,
            blocks = response.blocks.len(),
            peer_committed_view = response.peer_committed_view,
            "received sync response"
        );

        if response.blocks.is_empty() {
            debug!(target: "n42::cl::sync", "sync response contains no blocks");
            self.finish_sync_response_group(false);
            return;
        }

        let mut imported = 0u64;
        let mut highest_qc_verified_view = 0u64;
        for sync_block in &response.blocks {
            if sync_block.payload.is_empty() {
                debug!(target: "n42::cl::sync", view = sync_block.view, "skipping sync block with empty payload");
                continue;
            }

            // (T2a) Validate the block falls within the range we actually
            // requested. A malicious peer — or a stale fan-out response for a
            // since-superseded request — must not be able to inject blocks
            // outside `[from_view, to_view]`.
            if sync_block.view < requested_from || sync_block.view > requested_to {
                warn!(
                    target: "n42::cl::sync",
                    view = sync_block.view,
                    from_view = requested_from,
                    to_view = requested_to,
                    "dropping sync block outside the requested range"
                );
                counter!("n42_sync_blocks_out_of_range_total").increment(1);
                continue;
            }

            if !self.verify_sync_block_qc(sync_block) {
                continue;
            }
            highest_qc_verified_view = highest_qc_verified_view.max(sync_block.view);

            if self.committed_blocks.iter().any(|block| {
                block.view == sync_block.view && block.block_hash != sync_block.block_hash
            }) {
                error!(
                    target: "n42::cl::sync",
                    view = sync_block.view,
                    hash = %sync_block.block_hash,
                    "refusing conflicting QC-verified block at an already committed view"
                );
                continue;
            }

            let already_committed = self.committed_blocks.iter().any(|block| {
                block.view == sync_block.view && block.block_hash == sync_block.block_hash
            });

            if sync_block.view == self.execution_validated_head_view
                && self.execution_validated_head_view != 0
                && sync_block.block_hash != self.head_block_hash
            {
                error!(
                    target: "n42::cl::sync",
                    view = sync_block.view,
                    hash = %sync_block.block_hash,
                    execution_validated_head = %self.head_block_hash,
                    "refusing conflicting sync block at the execution-validated view"
                );
                counter!("n42_sync_blocks_conflict_validated_head_total").increment(1);
                continue;
            }

            // (T2b) Hoisted execution-validated-head guard. Never re-import or
            // re-FCU a block at or below the head reth has already executed —
            // whether or not it still sits in the committed ring (which is
            // EMPTY right after a restart). `import_and_notify` →
            // `handle_valid_import` issues the backward FCU *before*
            // `advance_execution_validated_head` can refuse it (F1), so the skip
            // has to happen here, ahead of the import. A second fan-out response
            // for a lineage an earlier peer already rebuilt lands here too.
            if sync_block.view <= self.execution_validated_head_view {
                if already_committed && let Some(ref changes) = sync_block.validator_changes {
                    self.recover_committed_block_validator_changes(
                        sync_block.view,
                        sync_block.block_hash,
                        changes.clone(),
                    );
                }
                imported += 1;
                continue;
            }

            let broadcast = BlockDataBroadcast {
                block_hash: sync_block.block_hash,
                view: sync_block.view,
                payload_json: sync_block.payload.clone(),
                timestamp: 0,
                execution_output: None,
                leader_ready_unix_ms: 0,
            };

            self.import_and_notify(broadcast).await;

            // (T2c) Never double-count consensus metadata. A block is
            // already-counted when it is in the ring, or — after a crash between
            // recording the CommitQC and executing the payload — when its view
            // sits at or below the restored committed floor while reth's
            // executed head is still behind. Import it (above) so reth rebuilds
            // the lineage, then skip the metadata bookkeeping.
            let committed_floor = self
                .execution_validated_head_view
                .max(self.last_commit_qc.as_ref().map(|qc| qc.view).unwrap_or(0));
            if already_committed || sync_block.view <= committed_floor {
                if let Some(ref changes) = sync_block.validator_changes {
                    self.recover_committed_block_validator_changes(
                        sync_block.view,
                        sync_block.block_hash,
                        changes.clone(),
                    );
                }
                imported += 1;
                continue;
            }

            // Fix #3: Scan sync-imported blocks for staking transactions.
            if let Some(ref staking_sink) = self.staking_sink
                && !sync_block.payload.is_empty()
            {
                match super::decompress_payload(&sync_block.payload) {
                    Ok(decompressed) => {
                        staking_sink.scan_committed_block(sync_block.view, &decompressed);
                    }
                    Err(e) => {
                        warn!(target: "n42::cl::sync", view = sync_block.view, error = %e, "failed to decompress sync payload for staking scan");
                    }
                }
            }

            // Update consensus metadata that payload building depends on.
            self.committed_block_count += 1;
            self.prev_randao_cache =
                alloy_primitives::keccak256(sync_block.commit_qc.aggregate_signature.to_bytes());

            // Write evidence to MDBX (same as normal commit path).
            if let Some(ref evidence_store) = self.evidence_store {
                let evidence = n42_jmt::ConsensusEvidence {
                    view: sync_block.view,
                    block_hash: sync_block.block_hash.0,
                    aggregate_signature: sync_block.commit_qc.aggregate_signature.to_bytes(),
                    signer_count: u16::try_from(sync_block.commit_qc.signers.len())
                        .unwrap_or(u16::MAX),
                    packed_signers: sync_block.commit_qc.signers.as_raw_slice().to_vec(),
                    // Filled later by the node-side mobile evidence write-back task;
                    // sync recovery must not gate committee evidence on phones.
                    mobile: None,
                };
                let encoded = evidence.encode();
                let root_bytes = *blake3::hash(&encoded).as_bytes();
                let store = std::sync::Arc::clone(evidence_store);
                let bn = self.committed_block_count;
                // Keep sync recovery crash-safe: do not persist the updated
                // snapshot until the matching evidence row is on disk.
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) = store.put_raw(bn, &encoded, &root_bytes) {
                        tracing::warn!(target: "n42::cl::sync", block = bn, error = %e, "evidence write failed");
                    }
                }).await;
            }

            // Apply validator changes from the synced block to the epoch manager.
            if let Some(ref changes) = sync_block.validator_changes {
                match self
                    .engine
                    .epoch_manager_mut()
                    .apply_committed_changes_from_sync(changes)
                {
                    Ok(()) => {
                        // Record the view at which the staging occurred so the epoch
                        // boundary check below can tell live-staged state from sync-staged state.
                        self.epoch_sync_staged_view = Some(sync_block.view);
                    }
                    Err(e) => {
                        tracing::warn!(target: "n42::cl::sync", view = sync_block.view, error = %e, "sync: commit validator changes failed");
                    }
                }
            }

            // Advance epoch when we reach or pass the boundary following the staging view.
            //
            // IMPORTANT: only fire if the staging was recorded from THIS sync pass
            // (epoch_sync_staged_view is set and <= current block's view).  Without
            // this guard, a pre-existing `next_set` from live-consensus staging at a
            // LATER view would trigger a premature advance at an earlier boundary,
            // causing the epoch manager to diverge from the actual network state.
            //
            // NOTE: we cannot use `is_epoch_boundary(sync_block.view + 1)` here
            // because the epoch boundary view may be a *timeout view* — a view where
            // the pacemaker timed out and no block was committed.  The live consensus
            // path fires advance_epoch() via advance_to_view() for every view including
            // timeout views, but the sync path only sees committed blocks.  If views
            // 30–31 were both timeouts, no sync block satisfies view+1 == 31, so the
            // epoch would never advance.  Instead we compute the next boundary view
            // from the staging view and advance as soon as any committed block at or
            // past that boundary is processed.
            if self.engine.epoch_manager().has_staged_next()
                && {
                    let epoch_len = self.engine.epoch_manager().epoch_length().max(1);
                    self.epoch_sync_staged_view.is_some_and(|sv| {
                        // Boundaries are at views k*epoch_len+1 for k ≥ 1.
                        // First boundary strictly after staging view sv:
                        //   k = floor((sv - 1) / epoch_len) + 1
                        //   boundary = k * epoch_len + 1
                        let k = sv.saturating_sub(1) / epoch_len + 1;
                        let next_boundary = k.saturating_mul(epoch_len).saturating_add(1);
                        sv <= sync_block.view && sync_block.view >= next_boundary
                    })
                }
                && self.engine.epoch_manager_mut().advance_epoch()
            {
                self.epoch_sync_staged_view = None;
                // Update my_index to reflect the new validator set — without this,
                // a newly-added validator would keep voting with the stale index (e.g. 0)
                // after sync completes, causing signature verification failures on the leader.
                self.engine.sync_local_validator_index();
                let new_epoch = self.engine.epoch_manager().current_epoch();
                let validator_count = self.engine.epoch_manager().current_validator_set().len();
                info!(
                    target: "n42::cl::sync",
                    new_epoch,
                    validator_count,
                    view = sync_block.view,
                    "epoch advanced during block sync"
                );
                let updated_vs = self.engine.epoch_manager().current_validator_set().clone();
                self.validator_set_for_sync = Some(updated_vs.clone());
                if let Some(ref state) = self.consensus_state {
                    state.update_validator_set(updated_vs);
                }
            }

            self.committed_blocks.push_back(CommittedBlock {
                view: sync_block.view,
                block_hash: sync_block.block_hash,
                commit_qc: sync_block.commit_qc.clone(),
                payload: sync_block.payload.clone(),
                validator_changes: sync_block.validator_changes.clone(),
            });
            self.committed_blocks
                .make_contiguous()
                .sort_by_key(|block| block.view);
            while self.committed_blocks.len() > max_committed_blocks() {
                self.committed_blocks.pop_front();
            }

            imported += 1;
        }

        // A useful response completes the fan-out group; later duplicates are
        // rejected as unsolicited. If this peer supplied nothing useful, keep
        // waiting for the other peers in the active catch-up fan-out.
        self.finish_sync_response_group(imported > 0);

        let execution_target_view = highest_qc_verified_view
            .max(self.last_commit_qc.as_ref().map(|qc| qc.view).unwrap_or(0));
        let execution_caught_up = self.execution_validated_head_view >= execution_target_view;
        if !execution_caught_up && !self.sync_in_flight {
            warn!(
                target: "n42::cl::sync",
                execution_validated_head_view = self.execution_validated_head_view,
                execution_target_view,
                "sync response did not close the execution gap; requesting the remaining lineage"
            );
            self.initiate_execution_catchup_sync(
                self.execution_validated_head_view,
                execution_target_view,
            );
        }

        // Save snapshot after sync batch to persist updated committed_block_count.
        self.save_consensus_state();

        info!(target: "n42::cl::sync", imported, peer_committed_view = response.peer_committed_view, "state sync blocks imported");

        // A node can become leader for the next view while a committed child is
        // still waiting for a missing execution ancestor. handle_view_changed
        // correctly defers building while that import is unresolved; once sync
        // reconstructs the lineage, re-enter the normal quorum-gated,
        // slot-aligned build path instead of waiting for a pacemaker timeout.
        if imported > 0
            && execution_caught_up
            && self.syncing_blocks.is_empty()
            && self.pending_finalization.is_none()
            && self.engine.is_current_leader()
            && !self.bg_import_in_flight
            && self.bg_import_queue.is_empty()
            && self.building_on_parent.is_none()
            && self.next_build_at.is_none()
        {
            info!(
                target: "n42::cl::sync",
                view = self.engine.current_view(),
                execution_validated_head_view = self.execution_validated_head_view,
                "execution catch-up completed for current leader; resuming gated build schedule"
            );
            self.begin_leader_build_wait(super::LeaderBuildWaitMode::Scheduled, None);
            self.evaluate_leader_build_wait(None).await;
        }

        let local_view = self.engine.current_view();
        if response.peer_committed_view > local_view + 3 {
            info!(
                target: "n42::cl::sync",
                local_view,
                peer_committed_view = response.peer_committed_view,
                "still behind after sync, requesting more blocks"
            );
            self.initiate_sync(local_view, response.peer_committed_view);
        }
    }

    fn finish_sync_response_group(&mut self, useful_response: bool) {
        if useful_response {
            self.sync_requested_peers.clear();
        }
        if useful_response || self.sync_requested_peers.is_empty() {
            self.sync_in_flight = false;
            self.sync_started_at = None;
            self.sync_request_range = None;
        }
    }

    /// Verifies commit QC validity for a sync block.
    /// Returns false and logs a warning if verification fails.
    fn verify_sync_block_qc(&self, sync_block: &SyncBlock) -> bool {
        // Identify the validator set by matching the QC's bitmap length rather than
        // computing epoch_for_view().  epoch_for_view() assumes epoch advances fire
        // exactly at every boundary, but in practice staging can be delayed past a
        // boundary (e.g., validator added at view 47 → epoch advances at view 61, not
        // view 31).  Matching by size is correct because each epoch has a distinct
        // validator count in the normal add/remove workflow; BLS verification then
        // confirms we picked the right set.
        let bitmap_len = sync_block.commit_qc.signers.len();
        let em = self.engine.epoch_manager();
        let vs = match em.find_validator_set_by_len(bitmap_len) {
            Some(vs) => vs,
            None => {
                warn!(
                    target: "n42::cl::sync",
                    view = sync_block.view,
                    bitmap_len,
                    current_epoch = em.current_epoch(),
                    "no validator set with matching bitmap length; rejecting sync block"
                );
                return false;
            }
        };

        let ch = validator_changes_hash(&sync_block.validator_changes);
        if let Err(e) = verify_commit_qc(&sync_block.commit_qc, vs, &ch) {
            warn!(
                target: "n42::cl::sync",
                view = sync_block.view,
                hash = %sync_block.block_hash,
                error = %e,
                "sync block has invalid commit_qc, skipping"
            );
            return false;
        }

        if sync_block.commit_qc.block_hash != sync_block.block_hash {
            warn!(
                target: "n42::cl::sync",
                view = sync_block.view,
                block_hash = %sync_block.block_hash,
                qc_hash = %sync_block.commit_qc.block_hash,
                "sync block commit_qc hash mismatch, skipping"
            );
            return false;
        }

        if sync_block.commit_qc.view != sync_block.view {
            warn!(
                target: "n42::cl::sync",
                block_view = sync_block.view,
                qc_view = sync_block.commit_qc.view,
                "sync block commit_qc view mismatch, skipping"
            );
            return false;
        }

        true
    }
}
