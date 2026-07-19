use super::{BlockDataBroadcast, CommittedBlock, ConsensusService};
use crate::persistence::{self, ConsensusSnapshot};
use alloy_primitives::B256;
use metrics::counter;
use n42_consensus::{validator_changes_hash, verify_commit_qc};
use n42_network::{
    BlockSyncResponse, MAX_BLOCKS_PER_SYNC_REQUEST, MAX_SYNC_MESSAGE_SIZE, PeerId, SyncBlock,
    SyncPayload,
};
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

const MAX_SYNC_BLOCKS: usize = 128;

fn sync_response_fits_frame(response: &BlockSyncResponse) -> bool {
    bincode::serialized_size(response)
        .ok()
        .is_some_and(|size| size <= MAX_SYNC_MESSAGE_SIZE as u64)
}

fn try_append_lineage_within_frame(response: &mut BlockSyncResponse, payload: SyncPayload) -> bool {
    response.execution_lineage.push(payload);
    if sync_response_fits_frame(response) {
        true
    } else {
        response.execution_lineage.pop();
        false
    }
}

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
            version: 5,
            current_view: self.engine.current_view(),
            locked_qc: self.engine.locked_qc().clone(),
            last_committed_qc: self.engine.last_committed_qc().clone(),
            consecutive_timeouts: self.engine.consecutive_timeouts(),
            scheduled_epoch_transition: self.collect_scheduled_epoch(),
            authorized_verifiers: Vec::new(),
            committed_block_count: self.committed_block_count,
            last_voted_view: self.engine.last_voted_view(),
            last_commit_voted_view: self.engine.last_commit_voted_view(),
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
        let execution_lineage: Vec<_> = self
            .recent_execution_lineage(block_hash)
            .into_iter()
            .map(|entry| SyncPayload {
                view: entry.view,
                block_hash: entry.block_hash,
                block_data: entry.block_data,
            })
            .collect();
        // Ordinary blocks remain recoverable through `SyncBlock`; duplicating
        // every cached execution output in the 10k-entry commit ring would be
        // unbounded at high TPS. Raw retention is reserved for the indispensable
        // case: a prepared ancestor without its own CommitQC.
        let execution_lineage = if execution_lineage.len() > 1 {
            execution_lineage
        } else {
            Vec::new()
        };
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
            execution_lineage,
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
    /// Clears an expired sync request group so another recovery strategy can
    /// take ownership. Both ordinary state sync and execution catch-up must
    /// pass through this gate: otherwise a lost single-peer response can leave
    /// `sync_in_flight` set forever and suppress every later fan-out request.
    fn expire_stale_sync_request(&mut self) -> bool {
        if !self.sync_in_flight {
            return false;
        }

        let elapsed = self.sync_started_at.map(|started| started.elapsed());
        let timed_out = elapsed
            .map(|elapsed| elapsed > sync_request_timeout())
            // An in-flight request without a start time violates the local
            // state invariant. Treat it as stale instead of wedging recovery.
            .unwrap_or(true);
        if !timed_out {
            return false;
        }

        warn!(
            target: "n42::cl::sync",
            elapsed_secs = elapsed.map_or(0, |elapsed| elapsed.as_secs()),
            requested_peers = self.sync_requested_peers.len(),
            "sync request timed out, resetting"
        );
        counter!("n42_sync_requests_timed_out_total").increment(1);
        self.sync_in_flight = false;
        self.sync_started_at = None;
        self.sync_request_range = None;
        self.sync_requested_peers.clear();
        true
    }

    /// Requests an execution lineage from every connected peer.
    ///
    /// This is narrower than ordinary consensus catch-up: a committed payload
    /// returned `Syncing`, so the local reth tree is known to be missing an
    /// ancestor. A single peer may have an incomplete retained payload ring or
    /// an unready request-response stream; fan-out avoids a full sync timeout
    /// while every response remains QC-verified and idempotent.
    pub(super) fn initiate_execution_catchup_sync(&mut self, local_view: u64, target_view: u64) {
        self.expire_stale_sync_request();
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
        self.expire_stale_sync_request();
        if self.sync_in_flight {
            debug!(target: "n42::cl::sync", local_view, target_view, "sync already in flight, skipping");
            return;
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

        let retained: Vec<&CommittedBlock> = self
            .committed_blocks
            .iter()
            .filter(|b| b.view >= request.from_view && b.view <= request.to_view)
            .take(MAX_SYNC_BLOCKS)
            .collect();
        let mut blocks: Vec<SyncBlock> = retained
            .iter()
            .map(|b| SyncBlock {
                view: b.view,
                block_hash: b.block_hash,
                commit_qc: b.commit_qc.clone(),
                payload: b.payload.clone(),
                validator_changes: b.validator_changes.clone(),
            })
            .collect();
        let peer_committed_view = self.committed_blocks.back().map(|b| b.view).unwrap_or(0);
        let mut response = BlockSyncResponse {
            blocks: std::mem::take(&mut blocks),
            peer_committed_view,
            execution_lineage: Vec::new(),
        };
        let initial_blocks = response.blocks.len();
        while !sync_response_fits_frame(&response) && !response.blocks.is_empty() {
            response.blocks.pop();
        }
        if response.blocks.len() != initial_blocks {
            warn!(
                target: "n42::cl::sync",
                retained_blocks = initial_blocks,
                blocks_sent = response.blocks.len(),
                max_frame_bytes = MAX_SYNC_MESSAGE_SIZE,
                "truncated sync blocks to the negotiated frame budget"
            );
            counter!("n42_sync_response_frame_truncations_total", "section" => "blocks")
                .increment(1);
        }

        let selected_hashes: std::collections::HashSet<_> = response
            .blocks
            .iter()
            .map(|block| block.block_hash)
            .collect();
        let mut lineage_hashes = std::collections::HashSet::new();
        let lineage_candidates = retained
            .iter()
            .filter(|block| selected_hashes.contains(&block.block_hash))
            .flat_map(|block| block.execution_lineage.iter())
            .filter(|payload| lineage_hashes.insert(payload.block_hash))
            .take(MAX_SYNC_BLOCKS)
            .cloned();
        let mut lineage_truncated = false;
        for payload in lineage_candidates {
            if !try_append_lineage_within_frame(&mut response, payload) {
                lineage_truncated = true;
                break;
            }
        }
        if lineage_truncated {
            warn!(
                target: "n42::cl::sync",
                blocks_sent = response.blocks.len(),
                lineage_sent = response.execution_lineage.len(),
                max_frame_bytes = MAX_SYNC_MESSAGE_SIZE,
                "truncated execution lineage to the negotiated frame budget"
            );
            counter!("n42_sync_response_frame_truncations_total", "section" => "lineage")
                .increment(1);
        }

        debug!(target: "n42::cl::sync", %peer, blocks_sent = response.blocks.len(), lineage_sent = response.execution_lineage.len(), peer_committed_view, "sending sync response");

        if let Err(e) = self
            .network
            .send_sync_response_reliable(request_id, response)
            .await
        {
            error!(target: "n42::cl::sync", error = %e, "failed to send sync response");
        }
    }

    /// Validates raw execution payloads against the CommitQC-bearing blocks in
    /// the response and returns the unique, parent-linked lineage in block
    /// number order. No payload is imported unless the chain reaches the
    /// caller's exact execution-validated hash.
    fn validated_sync_execution_lineage(
        &self,
        response: &BlockSyncResponse,
        requested_from: u64,
        requested_to: u64,
    ) -> Vec<(SyncPayload, BlockDataBroadcast, B256, u64)> {
        let committed_hashes: std::collections::HashSet<B256> = response
            .blocks
            .iter()
            .filter(|block| {
                block.view >= requested_from
                    && block.view <= requested_to
                    && self.verify_sync_block_qc(block)
            })
            .map(|block| block.block_hash)
            .collect();
        if committed_hashes.is_empty() || response.execution_lineage.is_empty() {
            return Vec::new();
        }

        let mut decoded: std::collections::HashMap<
            B256,
            (SyncPayload, BlockDataBroadcast, B256, u64),
        > = std::collections::HashMap::new();
        for payload in &response.execution_lineage {
            if payload.view < requested_from || payload.view > requested_to {
                continue;
            }
            let Ok(broadcast) = bincode::deserialize::<BlockDataBroadcast>(&payload.block_data)
            else {
                continue;
            };
            if broadcast.view != payload.view || broadcast.block_hash != payload.block_hash {
                continue;
            }
            let Some((parent_hash, block_number)) =
                Self::execution_parent_and_number(&payload.block_data)
            else {
                continue;
            };
            let decoded_payload = (payload.clone(), broadcast, parent_hash, block_number);
            if let Some(existing) = decoded.get(&payload.block_hash) {
                if existing.0.block_data != payload.block_data || existing.0.view != payload.view {
                    error!(
                        target: "n42::cl::sync",
                        hash = %payload.block_hash,
                        "sync response contains conflicting raw payloads for one block hash"
                    );
                    return Vec::new();
                }
                continue;
            }
            decoded.insert(payload.block_hash, decoded_payload);
        }

        let mut required = std::collections::HashSet::new();
        for committed_hash in &committed_hashes {
            let mut current = *committed_hash;
            let mut path = Vec::new();
            let mut seen = std::collections::HashSet::new();
            let mut child_number = None;
            while current != self.head_block_hash && path.len() < MAX_SYNC_BLOCKS {
                if !seen.insert(current) {
                    path.clear();
                    break;
                }
                let Some((_, _, parent_hash, block_number)) = decoded.get(&current) else {
                    path.clear();
                    break;
                };
                if child_number.is_some_and(|child| block_number.saturating_add(1) != child) {
                    path.clear();
                    break;
                }
                child_number = Some(*block_number);
                path.push(current);
                current = *parent_hash;
            }
            if current == self.head_block_hash {
                required.extend(path);
            } else {
                warn!(
                    target: "n42::cl::sync",
                    %committed_hash,
                    execution_head = %self.head_block_hash,
                    "sync response execution lineage does not reach the validated head"
                );
                counter!("n42_sync_execution_lineage_incomplete_total").increment(1);
            }
        }

        let mut lineage: Vec<_> = required
            .into_iter()
            .filter_map(|hash| decoded.remove(&hash))
            .collect();
        lineage.sort_by_key(|(_, _, _, block_number)| *block_number);
        if lineage
            .windows(2)
            .any(|pair| pair[0].3 == pair[1].3 && pair[0].0.block_hash != pair[1].0.block_hash)
        {
            error!(target: "n42::cl::sync", "sync response contains conflicting execution blocks at one height");
            return Vec::new();
        }
        lineage
    }

    /// Selects the exact raw path `(floor, target]` from a previously validated
    /// response lineage. Returning an empty path for a non-floor target means
    /// the caller must not trust a cached execution output for that target.
    fn sync_execution_path(
        lineage: &[(SyncPayload, BlockDataBroadcast, B256, u64)],
        floor: B256,
        target: B256,
    ) -> Vec<&(SyncPayload, BlockDataBroadcast, B256, u64)> {
        if target == floor {
            return Vec::new();
        }
        let by_hash: std::collections::HashMap<_, _> = lineage
            .iter()
            .map(|entry| (entry.0.block_hash, entry))
            .collect();
        let mut current = target;
        let mut reversed = Vec::new();
        let mut seen = std::collections::HashSet::new();
        while current != floor && reversed.len() < MAX_SYNC_BLOCKS {
            if !seen.insert(current) {
                return Vec::new();
            }
            let Some(entry) = by_hash.get(&current).copied() else {
                return Vec::new();
            };
            reversed.push(entry);
            current = entry.2;
        }
        if current != floor {
            return Vec::new();
        }
        reversed.reverse();
        reversed
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

        let validated_lineage =
            self.validated_sync_execution_lineage(&response, requested_from, requested_to);
        // Capture the durable metadata floor before any import advances the
        // execution head. This is the crash-recovery double-count guard; using
        // the post-import head would incorrectly classify every fresh block as
        // already accounted for.
        let restored_committed_floor = self
            .execution_validated_head_view
            .max(self.last_commit_qc.as_ref().map(|qc| qc.view).unwrap_or(0));

        // Stage every raw StateDiff before advancing reth. The prerequisite
        // imports below may immediately return Valid and drain the sidecar
        // queue, so staging after import would permanently skip an ancestor.
        if self.jmt.is_some() || self.twig.is_some() {
            for (payload, _, _, _) in &validated_lineage {
                if let Some(diff) =
                    Self::extract_state_diff_for_state_tree(payload.block_hash, &payload.block_data)
                {
                    self.missing_sidecar_diffs.remove(&payload.view);
                    self.pending_sidecar_diffs
                        .insert(payload.view, (payload.block_hash, diff));
                } else {
                    self.missing_sidecar_diffs
                        .insert(payload.view, payload.block_hash);
                    counter!("n42_sidecar_diff_gap_total").increment(1);
                }
            }
        }

        let raw_lineage_by_hash: std::collections::HashMap<_, _> = validated_lineage
            .iter()
            .map(|(payload, broadcast, _, block_number)| {
                (payload.block_hash, (broadcast.clone(), *block_number))
            })
            .collect();

        let mut imported = 0u64;
        let mut highest_qc_verified_view = 0u64;
        let mut sync_blocks: Vec<_> = response.blocks.iter().collect();
        sync_blocks.sort_by_key(|block| block.view);
        'sync_blocks: for sync_block in sync_blocks {
            if sync_block.payload.is_empty()
                && !raw_lineage_by_hash.contains_key(&sync_block.block_hash)
            {
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

            if self
                .bad_blocks
                .should_skip(sync_block.block_hash, "sync_response")
            {
                counter!("n42_sync_bad_blocks_filtered_total").increment(1);
                continue;
            }

            // Rebuild only the prepared ancestors immediately preceding this
            // CommitQC block. Processing per committed block preserves order
            // when another prepared ancestor appears between two CommitQCs in
            // the same response.
            let lineage_floor_hash = self.head_block_hash;
            let block_lineage = Self::sync_execution_path(
                &validated_lineage,
                lineage_floor_hash,
                sync_block.block_hash,
            );
            for entry in block_lineage
                .iter()
                .take(block_lineage.len().saturating_sub(1))
            {
                let broadcast = &entry.1;
                let parent_hash = entry.2;
                if parent_hash != self.head_block_hash {
                    warn!(
                        target: "n42::cl::sync",
                        view = broadcast.view,
                        hash = %broadcast.block_hash,
                        expected_parent = %self.head_block_hash,
                        actual_parent = %parent_hash,
                        "prepared sync ancestor is not next in the validated lineage"
                    );
                    continue 'sync_blocks;
                }
                if !self.import_and_notify(broadcast.clone()).await
                    || self.head_block_hash != broadcast.block_hash
                {
                    warn!(
                        target: "n42::cl::sync",
                        view = broadcast.view,
                        hash = %broadcast.block_hash,
                        "prepared sync ancestor has not reached execution-valid state"
                    );
                    continue 'sync_blocks;
                }
                counter!("n42_sync_prepared_ancestors_imported_total").increment(1);
            }

            let validated_raw_child = block_lineage
                .last()
                .filter(|entry| entry.0.block_hash == sync_block.block_hash);
            let broadcast = validated_raw_child
                .map(|entry| entry.1.clone())
                .unwrap_or_else(|| BlockDataBroadcast {
                    block_hash: sync_block.block_hash,
                    view: sync_block.view,
                    payload_json: sync_block.payload.clone(),
                    timestamp: 0,
                    execution_output: None,
                    leader_ready_unix_ms: 0,
                });

            if !self.import_and_notify(broadcast).await {
                counter!("n42_sync_blocks_import_rejected_total").increment(1);
                continue;
            }

            let retained_execution_lineage: Vec<_> =
                block_lineage.iter().map(|entry| entry.0.clone()).collect();
            let execution_block_number = validated_raw_child.map(|entry| entry.3).or_else(|| {
                Self::execution_parent_and_number_from_payload(&sync_block.payload)
                    .map(|(_, block_number)| block_number)
            });

            // Scan every newly recovered execution block in actual block-number
            // order, including prepared ancestors. A live commit with a missing
            // ancestor deliberately deferred this work, so the persisted
            // last-scanned floor makes the replay idempotent.
            if let Some(ref staking_sink) = self.staking_sink {
                if block_lineage.is_empty() {
                    if let Some(block_number) = execution_block_number
                        && block_number > staking_sink.last_scanned_block()
                        && let Ok(decompressed) = super::decompress_payload(&sync_block.payload)
                    {
                        staking_sink.scan_committed_block(block_number, &decompressed);
                    }
                } else {
                    for entry in &block_lineage {
                        let broadcast = &entry.1;
                        let block_number = entry.3;
                        if block_number <= staking_sink.last_scanned_block() {
                            continue;
                        }
                        match super::decompress_payload(&broadcast.payload_json) {
                            Ok(decompressed) => {
                                staking_sink.scan_committed_block(block_number, &decompressed)
                            }
                            Err(error) => warn!(
                                target: "n42::cl::sync",
                                block_number,
                                %error,
                                "failed to decompress raw sync payload for staking scan"
                            ),
                        }
                    }
                }
            }

            // (T2c) Never double-count consensus metadata. A block is
            // already-counted when it is in the ring, or — after a crash between
            // recording the CommitQC and executing the payload — when its view
            // sits at or below the restored committed floor while reth's
            // executed head is still behind. Import it (above) so reth rebuilds
            // the lineage, then skip the metadata bookkeeping.
            if already_committed || sync_block.view <= restored_committed_floor {
                if let Some(block_number) = execution_block_number {
                    self.committed_block_count = self.committed_block_count.max(block_number);
                }
                if let Some(block) = self.committed_blocks.iter_mut().find(|block| {
                    block.view == sync_block.view && block.block_hash == sync_block.block_hash
                }) {
                    if !retained_execution_lineage.is_empty() {
                        block.execution_lineage = retained_execution_lineage;
                    }
                    if block.payload.is_empty()
                        && let Some((broadcast, _)) =
                            raw_lineage_by_hash.get(&sync_block.block_hash)
                    {
                        block.payload = broadcast.payload_json.clone();
                    }
                }
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

            // Update consensus metadata that payload building depends on.
            if let Some(block_number) = execution_block_number {
                self.committed_block_count = self.committed_block_count.max(block_number);
            } else {
                self.committed_block_count += 1;
            }
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
                payload: validated_raw_child
                    .map(|entry| entry.1.payload_json.clone())
                    .unwrap_or_else(|| sync_block.payload.clone()),
                validator_changes: sync_block.validator_changes.clone(),
                execution_lineage: retained_execution_lineage,
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

#[cfg(test)]
mod frame_budget_tests {
    use super::*;

    #[test]
    fn execution_lineage_is_truncated_before_the_16mb_frame_limit() {
        let mut response = BlockSyncResponse {
            blocks: Vec::new(),
            peer_committed_view: 2,
            execution_lineage: Vec::new(),
        };
        let first = SyncPayload {
            view: 1,
            block_hash: B256::repeat_byte(1),
            block_data: vec![1; 9 * 1024 * 1024],
        };
        let second = SyncPayload {
            view: 2,
            block_hash: B256::repeat_byte(2),
            block_data: vec![2; 9 * 1024 * 1024],
        };

        assert!(try_append_lineage_within_frame(&mut response, first));
        assert!(!try_append_lineage_within_frame(&mut response, second));
        assert_eq!(response.execution_lineage.len(), 1);
        assert!(sync_response_fits_frame(&response));
    }
}
