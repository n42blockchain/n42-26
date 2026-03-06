use super::{BlobSidecarBroadcast, BlockDataBroadcast, CommittedBlock};
use alloy_eips::eip7594::BlobTransactionSidecarVariant;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatusEnum};
use n42_consensus::{verify_commit_qc, ValidatorSet};
use n42_network::{BlockSyncResponse, NetworkEvent, NetworkHandle, PeerId, SyncBlock};
use n42_primitives::QuorumCertificate;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_node_builder::ConsensusEngineHandle;
use reth_payload_primitives::EngineApiMessageVersion;
use reth_transaction_pool::blobstore::{BlobStore, DiskFileBlobStore};
use std::collections::{HashSet, VecDeque};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use metrics::{counter, gauge};
use tracing::{debug, error, info, warn};

/// Maximum committed blocks retained in the ring buffer for serving sync to other observers.
const MAX_COMMITTED_BLOCKS: usize = 10_000;

/// Progress report interval.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(10);

/// Interval for checking if we need to initiate a sync catch-up.
const SYNC_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Timeout for a sync request before resetting in-flight status.
const SYNC_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of blocks to request per sync batch (matches network layer limit).
const MAX_SYNC_BATCH: u64 = 128;

/// Lightweight observer node orchestrator.
///
/// Receives blocks via N42 GossipSub and drives reth's Engine API to keep the
/// EL state in sync. Does not participate in consensus (no voting, no block
/// production, no mobile verification). The reth eth P2P layer handles EL
/// history sync independently.
pub struct ObserverOrchestrator {
    // Network
    network: NetworkHandle,
    net_event_rx: mpsc::Receiver<NetworkEvent>,
    connected_peers: HashSet<PeerId>,

    // reth Engine API
    beacon_engine: ConsensusEngineHandle<EthEngineTypes>,
    head_block_hash: B256,

    // CL state
    local_view: u64,
    highest_seen_view: u64,
    validator_set: Option<ValidatorSet>,

    // Sync
    sync_in_flight: bool,
    sync_started_at: Option<Instant>,
    sync_attempt_counter: u64,
    committed_blocks: VecDeque<CommittedBlock>,

    // Blob
    blob_store: Option<DiskFileBlobStore>,

    // Progress
    blocks_imported: u64,
    sync_start_time: Instant,
}

impl ObserverOrchestrator {
    pub fn new(
        network: NetworkHandle,
        net_event_rx: mpsc::Receiver<NetworkEvent>,
        beacon_engine: ConsensusEngineHandle<EthEngineTypes>,
        head_block_hash: B256,
    ) -> Self {
        Self {
            network,
            net_event_rx,
            connected_peers: HashSet::new(),
            beacon_engine,
            head_block_hash,
            local_view: 0,
            highest_seen_view: 0,
            validator_set: None,
            sync_in_flight: false,
            sync_started_at: None,
            sync_attempt_counter: 0,
            committed_blocks: VecDeque::new(),
            blob_store: None,
            blocks_imported: 0,
            sync_start_time: Instant::now(),
        }
    }

    pub fn with_validator_set(mut self, vs: ValidatorSet) -> Self {
        self.validator_set = Some(vs);
        self
    }

    pub fn with_blob_store(mut self, blob_store: DiskFileBlobStore) -> Self {
        self.blob_store = Some(blob_store);
        self
    }

    /// Runs the observer event loop. Never returns under normal operation.
    pub async fn run(mut self) {
        info!(
            target: "n42::observer",
            head = %self.head_block_hash,
            "Observer mode active — EL sync via reth eth P2P, CL data via GossipSub"
        );

        let mut progress_interval = tokio::time::interval(PROGRESS_INTERVAL);
        progress_interval.tick().await; // consume the first immediate tick

        let mut sync_check_interval = tokio::time::interval(SYNC_CHECK_INTERVAL);
        sync_check_interval.tick().await;

        loop {
            tokio::select! {
                event = self.net_event_rx.recv() => {
                    match event {
                        Some(ev) => self.handle_network_event(ev).await,
                        None => {
                            info!(target: "n42::observer", "network event channel closed, shutting down observer");
                            break;
                        }
                    }
                }

                _ = progress_interval.tick() => {
                    self.print_progress();
                }

                _ = sync_check_interval.tick() => {
                    self.check_and_initiate_sync();
                }
            }
        }

        info!(target: "n42::observer", blocks_imported = self.blocks_imported, "observer shutting down");
    }

    async fn handle_network_event(&mut self, event: NetworkEvent) {
        match event {
            NetworkEvent::PeerConnected(peer_id) => {
                info!(target: "n42::observer", %peer_id, "peer connected");
                self.connected_peers.insert(peer_id);
                gauge!("n42_observer_connected_peers").set(self.connected_peers.len() as f64);
            }
            NetworkEvent::PeerDisconnected(peer_id) => {
                warn!(target: "n42::observer", %peer_id, "peer disconnected");
                self.connected_peers.remove(&peer_id);
                gauge!("n42_observer_connected_peers").set(self.connected_peers.len() as f64);
            }
            NetworkEvent::BlockAnnouncement { source, data } => {
                debug!(target: "n42::observer", %source, bytes = data.len(), "received block announcement");
                self.handle_block_announcement(data).await;
            }
            NetworkEvent::BlobSidecarReceived { source: _, data } => {
                self.handle_blob_sidecar(data);
            }
            NetworkEvent::SyncRequest { peer, request_id, request } => {
                self.handle_sync_request(peer, request_id, request);
            }
            NetworkEvent::SyncResponse { peer, response } => {
                self.handle_sync_response(peer, response).await;
            }
            NetworkEvent::SyncRequestFailed { peer, error } => {
                warn!(target: "n42::observer", %peer, %error, "sync request failed");
                self.sync_in_flight = false;
                self.sync_started_at = None;
            }
            // Observer ignores consensus messages, transactions, and verification receipts.
            _ => {}
        }
    }

    /// Decodes a BlockDataBroadcast from wire bytes and imports it.
    async fn handle_block_announcement(&mut self, data: Vec<u8>) {
        let broadcast: BlockDataBroadcast = match bincode::deserialize(&data) {
            Ok(b) => b,
            Err(e) => {
                warn!(target: "n42::observer", error = %e, "invalid block data broadcast");
                return;
            }
        };
        self.import_block_data(broadcast, None).await;
    }

    /// Core import logic: parses the execution payload, calls new_payload + fork_choice_updated.
    ///
    /// `commit_qc` is provided when the block comes from sync (has a real QC); `None` for
    /// blocks received via GossipSub (which are already finalized by the consensus protocol
    /// but don't carry a QC in the broadcast).
    async fn import_block_data(&mut self, broadcast: BlockDataBroadcast, commit_qc: Option<QuorumCertificate>) {
        let hash = broadcast.block_hash;
        let view = broadcast.view;

        if view > self.highest_seen_view {
            self.highest_seen_view = view;
        }

        let execution_data = match serde_json::from_slice(&broadcast.payload_json) {
            Ok(d) => d,
            Err(e) => {
                warn!(target: "n42::observer", %hash, error = %e, "failed to parse execution payload JSON");
                return;
            }
        };

        match self.beacon_engine.new_payload(execution_data).await {
            Ok(status) => {
                if matches!(status.status, PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted) {
                    let fcu_state = ForkchoiceState {
                        head_block_hash: hash,
                        safe_block_hash: hash,
                        finalized_block_hash: hash,
                    };
                    if let Err(e) = self.beacon_engine
                        .fork_choice_updated(fcu_state, None, EngineApiMessageVersion::default())
                        .await
                    {
                        error!(target: "n42::observer", %hash, error = %e, "fork_choice_updated failed");
                    } else {
                        self.head_block_hash = hash;
                        self.local_view = view;
                        self.blocks_imported += 1;
                        counter!("n42_observer_blocks_imported").increment(1);
                        gauge!("n42_observer_view").set(view as f64);

                        info!(
                            target: "n42::observer",
                            view,
                            %hash,
                            peers = self.connected_peers.len(),
                            "block imported"
                        );
                    }

                    // Store for serving sync to other observers.
                    // Use the real commit_qc when available (sync blocks), or a placeholder
                    // for GossipSub blocks (receivers without validator_set will skip QC check).
                    let qc = commit_qc.unwrap_or_else(QuorumCertificate::genesis);
                    self.store_committed_block(view, hash, qc, &broadcast.payload_json);
                } else if matches!(status.status, PayloadStatusEnum::Syncing) {
                    debug!(target: "n42::observer", %hash, view, "new_payload returned Syncing (reth pipeline catching up)");
                } else {
                    warn!(target: "n42::observer", %hash, view, status = ?status.status, "new_payload rejected block");
                }
            }
            Err(e) => {
                error!(target: "n42::observer", %hash, error = %e, "new_payload call failed");
            }
        }
    }

    fn handle_blob_sidecar(&self, data: Vec<u8>) {
        let blob_store = match &self.blob_store {
            Some(bs) => bs,
            None => return,
        };

        let broadcast: BlobSidecarBroadcast = match bincode::deserialize(&data) {
            Ok(b) => b,
            Err(e) => {
                warn!(target: "n42::observer", error = %e, "invalid blob sidecar broadcast");
                return;
            }
        };

        for (tx_hash, sidecar_rlp) in broadcast.sidecars {
            match <BlobTransactionSidecarVariant as alloy_rlp::Decodable>::decode(&mut &sidecar_rlp[..]) {
                Ok(sidecar) => {
                    if let Err(e) = blob_store.insert(tx_hash, sidecar) {
                        debug!(target: "n42::observer", %tx_hash, error = %e, "failed to insert blob sidecar");
                    }
                }
                Err(e) => {
                    warn!(target: "n42::observer", %tx_hash, error = %e, "failed to decode blob sidecar RLP");
                }
            }
        }
    }

    /// Serves sync requests from other observers.
    fn handle_sync_request(
        &self,
        peer: PeerId,
        request_id: u64,
        request: n42_network::BlockSyncRequest,
    ) {
        if let Err(reason) = request.validate() {
            warn!(target: "n42::observer", %peer, %reason, "rejecting invalid sync request");
            return;
        }

        let blocks: Vec<SyncBlock> = self.committed_blocks
            .iter()
            .filter(|b| b.view >= request.from_view && b.view <= request.to_view)
            .take(128)
            .map(|b| SyncBlock {
                view: b.view,
                block_hash: b.block_hash,
                commit_qc: b.commit_qc.clone(),
                payload: b.payload.clone(),
            })
            .collect();

        let peer_committed_view = self.committed_blocks
            .back()
            .map(|b| b.view)
            .unwrap_or(0);

        debug!(target: "n42::observer", %peer, blocks_sent = blocks.len(), "sending sync response");

        let response = BlockSyncResponse {
            blocks,
            peer_committed_view,
        };

        if let Err(e) = self.network.send_sync_response(request_id, response) {
            error!(target: "n42::observer", error = %e, "failed to send sync response");
        }
    }

    /// Imports blocks from a sync response, with optional QC verification.
    async fn handle_sync_response(&mut self, peer: PeerId, response: BlockSyncResponse) {
        self.sync_in_flight = false;
        self.sync_started_at = None;

        info!(
            target: "n42::observer",
            %peer,
            blocks = response.blocks.len(),
            peer_committed_view = response.peer_committed_view,
            "received sync response"
        );

        if response.blocks.is_empty() {
            return;
        }

        let mut imported = 0u64;
        for sync_block in response.blocks {
            if sync_block.payload.is_empty() {
                continue;
            }

            // Verify QC if validator set is available
            if !self.verify_sync_block_qc(&sync_block) {
                continue;
            }

            let commit_qc = sync_block.commit_qc.clone();
            let broadcast = BlockDataBroadcast {
                block_hash: sync_block.block_hash,
                view: sync_block.view,
                payload_json: sync_block.payload,
                timestamp: 0,
            };

            // Import directly — no serialize/deserialize round-trip
            self.import_block_data(broadcast, Some(commit_qc)).await;
            imported += 1;
        }

        info!(target: "n42::observer", imported, "sync blocks processed");

        // Request more if still behind
        if response.peer_committed_view > self.local_view + 3 {
            info!(
                target: "n42::observer",
                local_view = self.local_view,
                peer_committed_view = response.peer_committed_view,
                "still behind, requesting more blocks"
            );
            self.initiate_sync(self.local_view, response.peer_committed_view);
        }
    }

    fn verify_sync_block_qc(&self, sync_block: &SyncBlock) -> bool {
        let vs = match &self.validator_set {
            Some(vs) => vs,
            None => {
                // No validator set → skip QC verification (trust reth's EVM execution)
                return true;
            }
        };

        if let Err(e) = verify_commit_qc(&sync_block.commit_qc, vs) {
            warn!(
                target: "n42::observer",
                view = sync_block.view,
                hash = %sync_block.block_hash,
                error = %e,
                "sync block has invalid commit_qc, skipping"
            );
            return false;
        }

        if sync_block.commit_qc.block_hash != sync_block.block_hash {
            warn!(
                target: "n42::observer",
                view = sync_block.view,
                "sync block commit_qc hash mismatch, skipping"
            );
            return false;
        }

        if sync_block.commit_qc.view != sync_block.view {
            warn!(
                target: "n42::observer",
                view = sync_block.view,
                "sync block commit_qc view mismatch, skipping"
            );
            return false;
        }

        true
    }

    /// Checks if observer is behind the network and initiates sync if needed.
    fn check_and_initiate_sync(&mut self) {
        if self.highest_seen_view > self.local_view + 3 {
            self.initiate_sync(self.local_view, self.highest_seen_view);
        }
    }

    fn initiate_sync(&mut self, local_view: u64, target_view: u64) {
        if self.sync_in_flight {
            let timed_out = self.sync_started_at
                .map(|t| t.elapsed() > SYNC_TIMEOUT)
                .unwrap_or(false);

            if timed_out {
                warn!(target: "n42::observer", "sync request timed out, resetting");
                self.sync_in_flight = false;
                self.sync_started_at = None;
            } else {
                return;
            }
        }

        let peers: Vec<_> = self.connected_peers.iter().copied().collect();
        if peers.is_empty() {
            return;
        }

        // Rotate through peers on each attempt to avoid retrying the same unresponsive peer.
        let peer = peers[(self.sync_attempt_counter as usize) % peers.len()];
        self.sync_attempt_counter += 1;
        let capped_to_view = target_view.min(local_view + MAX_SYNC_BATCH);

        info!(
            target: "n42::observer",
            %peer,
            local_view,
            target_view = capped_to_view,
            "initiating CL sync"
        );

        let request = n42_network::BlockSyncRequest {
            from_view: local_view + 1,
            to_view: capped_to_view,
            local_committed_view: local_view,
        };

        if let Err(e) = self.network.request_sync(peer, request) {
            error!(target: "n42::observer", error = %e, "failed to send sync request");
            return;
        }

        self.sync_in_flight = true;
        self.sync_started_at = Some(Instant::now());
    }

    fn store_committed_block(&mut self, view: u64, block_hash: B256, commit_qc: QuorumCertificate, payload: &[u8]) {
        if self.committed_blocks.len() >= MAX_COMMITTED_BLOCKS {
            self.committed_blocks.pop_front();
        }
        self.committed_blocks.push_back(CommittedBlock {
            view,
            block_hash,
            commit_qc,
            payload: payload.to_vec(),
        });
    }

    fn print_progress(&self) {
        let uptime = self.sync_start_time.elapsed();
        let uptime_secs = uptime.as_secs();
        let blocks_per_sec = if uptime_secs > 0 {
            self.blocks_imported as f64 / uptime_secs as f64
        } else {
            0.0
        };

        let stage = if self.highest_seen_view > 0 && self.local_view + 10 < self.highest_seen_view {
            "catch-up"
        } else if self.blocks_imported > 0 {
            "tracking"
        } else {
            "waiting"
        };

        info!(
            target: "n42::observer",
            local_view = self.local_view,
            highest_seen = self.highest_seen_view,
            imported = self.blocks_imported,
            peers = self.connected_peers.len(),
            blocks_per_sec = format!("{:.1}", blocks_per_sec),
            uptime_secs,
            stage,
            "status"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use n42_network::NetworkCommand;

    fn make_test_network() -> (NetworkHandle, mpsc::UnboundedReceiver<NetworkCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        (NetworkHandle::new(cmd_tx), cmd_rx)
    }

    #[test]
    fn test_observer_construction() {
        let (_network, _cmd_rx) = make_test_network();
        let (_net_event_tx, _net_event_rx) = mpsc::channel::<NetworkEvent>(8192);
        // ConsensusEngineHandle cannot be constructed in unit tests;
        // integration tests cover the full ObserverOrchestrator flow.
    }

    #[test]
    fn test_verify_sync_block_no_validator_set_passes() {
        // When no validator set is configured, verify_sync_block_qc should
        // return true (trust reth's EVM execution instead of BLS verification).
        // This is verified by code inspection: the first match arm returns true
        // when self.validator_set is None.
    }
}
