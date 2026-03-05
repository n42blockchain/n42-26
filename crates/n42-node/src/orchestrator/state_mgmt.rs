use super::{BlockDataBroadcast, CommittedBlock, ConsensusOrchestrator};
use crate::persistence::{self, ConsensusSnapshot};
use alloy_primitives::B256;
use n42_consensus::verify_commit_qc;
use n42_network::{BlockSyncResponse, PeerId, SyncBlock, MAX_BLOCKS_PER_SYNC_REQUEST};
use n42_primitives::QuorumCertificate;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::time::Instant;
use metrics::counter;
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

impl ConsensusOrchestrator {
    /// Collects the staged epoch transition info from the epoch manager for persistence.
    fn collect_scheduled_epoch(&self) -> Option<(u64, Vec<n42_chainspec::ValidatorInfo>, u32)> {
        self.engine
            .epoch_manager()
            .staged_epoch_info()
            .map(|(epoch, validators, f)| (epoch, validators.to_vec(), f))
    }

    /// Collects the authorized verifier BLS pubkeys from consensus state for persistence.
    fn collect_authorized_verifiers(&self) -> Vec<[u8; 48]> {
        self.consensus_state
            .as_ref()
            .map(|s| s.snapshot_authorized_verifiers())
            .unwrap_or_default()
    }

    /// Builds a snapshot of the current consensus state for persistence.
    fn build_snapshot(&self) -> ConsensusSnapshot {
        ConsensusSnapshot {
            version: 1,
            current_view: self.engine.current_view(),
            locked_qc: self.engine.locked_qc().clone(),
            last_committed_qc: self.engine.last_committed_qc().clone(),
            consecutive_timeouts: self.engine.consecutive_timeouts(),
            scheduled_epoch_transition: self.collect_scheduled_epoch(),
            authorized_verifiers: self.collect_authorized_verifiers(),
            committed_block_count: self.committed_block_count,
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
        if let Some(ref staking_mgr) = self.staking_manager {
            let mgr = staking_mgr.lock().unwrap_or_else(|e| e.into_inner());
            mgr.save();
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
        });
    }
}

// ── State sync ──

impl ConsensusOrchestrator {
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
        let peer = peers[(local_view as usize) % peers.len()];

        info!(target: "n42::cl::sync", %peer, local_view, target_view, "initiating state sync");

        // Cap to_view so the request doesn't exceed MAX_BLOCKS_PER_SYNC_REQUEST.
        // The peer will reject oversized ranges; capping here avoids a wasted round-trip.
        let capped_to_view = target_view.min(local_view + MAX_BLOCKS_PER_SYNC_REQUEST);
        let request = n42_network::BlockSyncRequest {
            from_view: local_view + 1,
            to_view: capped_to_view,
            local_committed_view: local_view,
        };

        if let Err(e) = self.network.request_sync(peer, request) {
            error!(target: "n42::cl::sync", error = %e, "failed to send sync request");
            return;
        }

        self.sync_in_flight = true;
        self.sync_started_at = Some(Instant::now());
    }

    /// Handles an incoming sync request from a peer.
    /// Looks up committed blocks in the ring buffer and responds with up to 128 blocks.
    pub(super) fn handle_sync_request(
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
            })
            .collect();

        let peer_committed_view = self
            .committed_blocks
            .back()
            .map(|b| b.view)
            .unwrap_or(0);

        debug!(target: "n42::cl::sync", %peer, blocks_sent = blocks.len(), peer_committed_view, "sending sync response");

        let response = BlockSyncResponse {
            blocks,
            peer_committed_view,
        };

        if let Err(e) = self.network.send_sync_response(request_id, response) {
            error!(target: "n42::cl::sync", error = %e, "failed to send sync response");
        }
    }

    /// Handles a sync response containing blocks from a peer.
    /// Verifies QC validity, imports each block into reth, and requests more if still behind.
    pub(super) async fn handle_sync_response(
        &mut self,
        peer: PeerId,
        response: BlockSyncResponse,
    ) {
        self.sync_in_flight = false;
        self.sync_started_at = None;

        info!(
            target: "n42::cl::sync",
            %peer,
            blocks = response.blocks.len(),
            peer_committed_view = response.peer_committed_view,
            "received sync response"
        );

        if response.blocks.is_empty() {
            debug!(target: "n42::cl::sync", "sync response contains no blocks");
            return;
        }

        let mut imported = 0u64;
        for sync_block in &response.blocks {
            if sync_block.payload.is_empty() {
                debug!(target: "n42::cl::sync", view = sync_block.view, "skipping sync block with empty payload");
                continue;
            }

            if !self.verify_sync_block_qc(sync_block) {
                continue;
            }

            let broadcast = BlockDataBroadcast {
                block_hash: sync_block.block_hash,
                view: sync_block.view,
                payload_json: sync_block.payload.clone(),
            };

            self.import_and_notify(broadcast).await;

            // Fix #3: Scan sync-imported blocks for staking transactions.
            if let Some(ref staking_mgr) = self.staking_manager
                && !sync_block.payload.is_empty()
            {
                let mut mgr = staking_mgr.lock().unwrap_or_else(|e| e.into_inner());
                mgr.scan_committed_block(sync_block.view, &sync_block.payload);
            }

            if self.committed_blocks.len() >= max_committed_blocks() {
                self.committed_blocks.pop_front();
            }
            self.committed_blocks.push_back(CommittedBlock {
                view: sync_block.view,
                block_hash: sync_block.block_hash,
                commit_qc: sync_block.commit_qc.clone(),
                payload: sync_block.payload.clone(),
            });

            imported += 1;
        }

        info!(target: "n42::cl::sync", imported, peer_committed_view = response.peer_committed_view, "state sync blocks imported");

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

    /// Verifies commit QC validity for a sync block.
    /// Returns false and logs a warning if verification fails.
    fn verify_sync_block_qc(&self, sync_block: &SyncBlock) -> bool {
        let vs = match &self.validator_set_for_sync {
            Some(vs) => vs,
            None => {
                warn!(target: "n42::cl::sync", "cannot verify sync blocks: no validator set configured, rejecting sync response");
                return false;
            }
        };

        if let Err(e) = verify_commit_qc(&sync_block.commit_qc, vs) {
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
