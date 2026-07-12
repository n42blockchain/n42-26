use crate::blob_port::BlobStorePort;
use crate::el::ExecutionLayer;
use crate::epoch_schedule::EpochSchedule;
use crate::exec_cache::ExecutionOutputCache;
use crate::orchestrator::{BlobSidecarBroadcast, BlockDataBroadcast, CommittedBlock};
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatusEnum};
use metrics::{counter, gauge};
use n42_chainspec::ValidatorInfo;
use n42_consensus::{ValidatorSet, validator_changes_hash, verify_commit_qc};
use n42_network::{BlockSyncResponse, NetworkEvent, NetworkHandle, PeerId, SyncBlock};
use n42_primitives::QuorumCertificate;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SyncTargetSource {
    ProvenProgress,
    GossipHint,
}

fn resolve_validator_set_for_view(
    view: u64,
    validator_set: Option<&ValidatorSet>,
    initial_validators: Option<&[ValidatorInfo]>,
    initial_fault_tolerance: u32,
    epoch_length: u64,
    epoch_schedule: Option<&EpochSchedule>,
) -> Option<ValidatorSet> {
    let Some(initial_validators) = initial_validators else {
        return validator_set.cloned();
    };

    if epoch_length == 0 {
        return ValidatorSet::try_new(initial_validators, initial_fault_tolerance).ok();
    }

    let epoch = view.saturating_sub(1) / epoch_length;
    let (validators, fault_tolerance) = epoch_schedule
        .map(|schedule| {
            schedule.active_config_for_epoch(epoch, initial_validators, initial_fault_tolerance)
        })
        .unwrap_or((initial_validators, initial_fault_tolerance));

    ValidatorSet::try_new(validators, fault_tolerance).ok()
}

fn select_sync_target(
    local_view: u64,
    highest_sync_target_view: u64,
    highest_seen_view: u64,
    last_gossip_sync_request_to_view: u64,
) -> Option<(u64, SyncTargetSource)> {
    if highest_sync_target_view > local_view + 3 {
        return Some((highest_sync_target_view, SyncTargetSource::ProvenProgress));
    }

    let capped_gossip_target = highest_seen_view.min(local_view.saturating_add(MAX_SYNC_BATCH));
    if highest_seen_view > local_view + 3 && capped_gossip_target > last_gossip_sync_request_to_view
    {
        return Some((highest_seen_view, SyncTargetSource::GossipHint));
    }

    None
}

fn verify_sync_block_qc_against_set(
    sync_block: &SyncBlock,
    validator_set: Option<&ValidatorSet>,
) -> bool {
    let Some(vs) = validator_set else {
        // No validator set -> skip QC verification (trust reth's EVM execution)
        return true;
    };

    let ch = validator_changes_hash(&sync_block.validator_changes);
    if let Err(e) = verify_commit_qc(&sync_block.commit_qc, vs, &ch) {
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

    // Execution layer (import-only adapter; observers never build payloads).
    el: Arc<dyn ExecutionLayer>,
    head_block_hash: B256,

    // CL state
    local_view: u64,
    highest_seen_view: u64,
    highest_sync_target_view: u64,
    last_gossip_sync_request_to_view: u64,
    validator_set: Option<ValidatorSet>,
    initial_validators: Option<Vec<ValidatorInfo>>,
    initial_fault_tolerance: u32,
    epoch_length: u64,
    epoch_schedule: Option<EpochSchedule>,

    // Sync
    sync_in_flight: bool,
    sync_started_at: Option<Instant>,
    sync_attempt_counter: u64,
    committed_blocks: VecDeque<CommittedBlock>,

    // Blob (byte-oriented port; reth blob types live in the node-side adapter)
    blob_store: Option<Arc<dyn BlobStorePort>>,

    // Compact-block execution-output cache port (None ⇒ no inject)
    exec_output_cache: Option<Arc<dyn ExecutionOutputCache>>,

    // Progress
    blocks_imported: u64,
    sync_start_time: Instant,
}

impl ObserverOrchestrator {
    pub fn new(
        network: NetworkHandle,
        net_event_rx: mpsc::Receiver<NetworkEvent>,
        el: Arc<dyn ExecutionLayer>,
        head_block_hash: B256,
    ) -> Self {
        Self {
            network,
            net_event_rx,
            connected_peers: HashSet::new(),
            el,
            head_block_hash,
            local_view: 0,
            highest_seen_view: 0,
            highest_sync_target_view: 0,
            last_gossip_sync_request_to_view: 0,
            validator_set: None,
            initial_validators: None,
            initial_fault_tolerance: 0,
            epoch_length: 0,
            epoch_schedule: None,
            sync_in_flight: false,
            sync_started_at: None,
            sync_attempt_counter: 0,
            committed_blocks: VecDeque::new(),
            blob_store: None,
            exec_output_cache: None,
            blocks_imported: 0,
            sync_start_time: Instant::now(),
        }
    }

    pub fn with_validator_set(mut self, vs: ValidatorSet) -> Self {
        self.initial_fault_tolerance = vs.fault_tolerance();
        self.initial_validators = Some(vs.validator_infos());
        self.validator_set = Some(vs);
        self
    }

    pub fn with_epoch_schedule(mut self, epoch_length: u64, schedule: EpochSchedule) -> Self {
        self.epoch_length = epoch_length;
        self.epoch_schedule = Some(schedule);
        self
    }

    pub fn with_blob_store(mut self, blob_store: Arc<dyn BlobStorePort>) -> Self {
        self.blob_store = Some(blob_store);
        self
    }

    pub fn with_exec_output_cache(mut self, cache: Arc<dyn ExecutionOutputCache>) -> Self {
        self.exec_output_cache = Some(cache);
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
                    self.check_and_initiate_sync().await;
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
            NetworkEvent::SyncRequest {
                peer,
                request_id,
                request,
            } => {
                self.handle_sync_request(peer, request_id, request).await;
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

    /// Core import logic: parses the execution payload and submits it to the Engine API.
    ///
    /// `commit_qc` is provided when the block comes from sync (has a real QC); `None` for
    /// blocks received via GossipSub. GossipSub blocks without a commit proof are allowed
    /// to pre-execute via `new_payload`, but they are not promoted to canonical head until
    /// a sync path supplies a real commit QC.
    async fn import_block_data(
        &mut self,
        broadcast: BlockDataBroadcast,
        commit_qc: Option<QuorumCertificate>,
    ) {
        let hash = broadcast.block_hash;
        let view = broadcast.view;
        let has_commit_proof = commit_qc.is_some();

        if view > self.highest_seen_view {
            self.highest_seen_view = view;
        }
        if has_commit_proof && view > self.highest_sync_target_view {
            self.highest_sync_target_view = view;
        }

        let payload_json = match crate::orchestrator::decompress_payload(&broadcast.payload_json) {
            Ok(d) => d,
            Err(e) => {
                warn!(target: "n42::observer", %hash, error = %e, "failed to decompress payload");
                return;
            }
        };
        let execution_data = match serde_json::from_slice(&payload_json) {
            Ok(d) => d,
            Err(e) => {
                warn!(target: "n42::observer", %hash, error = %e, "failed to parse execution payload JSON");
                return;
            }
        };

        // Compact Block: load execution output to skip EVM re-execution.
        if let Some(ref exec_compressed) = broadcast.execution_output
            && crate::orchestrator::compact_block_enabled()
            && let Some(ref cache) = self.exec_output_cache
        {
            cache.inject(hash, exec_compressed, "observer_import");
        }

        match self.el.new_payload(execution_data).await {
            Ok(status) => {
                if matches!(
                    status.status,
                    PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted
                ) {
                    if !has_commit_proof {
                        debug!(
                            target: "n42::observer",
                            view,
                            %hash,
                            "validated gossip block without commit_qc; waiting for sync proof before following head"
                        );
                        return;
                    }

                    let fcu_state = ForkchoiceState {
                        head_block_hash: hash,
                        safe_block_hash: hash,
                        finalized_block_hash: hash,
                    };
                    if let Err(e) = self.el.fork_choice_updated(fcu_state).await {
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

                    // Only cache blocks for observer sync when we have a real commit QC.
                    // GossipSub block announcements do not carry commit proofs, and
                    // synthesizing a placeholder QC causes downstream observer sync
                    // validation to reject the block.
                    if let Some(qc) = commit_qc {
                        self.store_committed_block(view, hash, qc, &broadcast.payload_json);
                    } else {
                        debug!(
                            target: "n42::observer",
                            view,
                            %hash,
                            "skipping observer sync cache for block without commit_qc"
                        );
                    }
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
            blob_store.insert_rlp(tx_hash, &sidecar_rlp);
        }
    }

    /// Serves sync requests from other observers.
    async fn handle_sync_request(
        &self,
        peer: PeerId,
        request_id: u64,
        request: n42_network::BlockSyncRequest,
    ) {
        if let Err(reason) = request.validate() {
            warn!(target: "n42::observer", %peer, %reason, "rejecting invalid sync request");
            return;
        }

        let blocks: Vec<SyncBlock> = self
            .committed_blocks
            .iter()
            .filter(|b| b.view >= request.from_view && b.view <= request.to_view)
            .take(128)
            .map(|b| SyncBlock {
                view: b.view,
                block_hash: b.block_hash,
                commit_qc: b.commit_qc.clone(),
                payload: b.payload.clone(),
                validator_changes: b.validator_changes.clone(),
            })
            .collect();

        let peer_committed_view = self.committed_blocks.back().map(|b| b.view).unwrap_or(0);

        debug!(target: "n42::observer", %peer, blocks_sent = blocks.len(), "sending sync response");

        let response = BlockSyncResponse {
            blocks,
            peer_committed_view,
        };

        if let Err(e) = self
            .network
            .send_sync_response_reliable(request_id, response)
            .await
        {
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

        if response.peer_committed_view > self.highest_sync_target_view {
            self.highest_sync_target_view = response.peer_committed_view;
        }
        if response.peer_committed_view > self.highest_seen_view {
            self.highest_seen_view = response.peer_committed_view;
        }

        if response.blocks.is_empty() {
            if response.peer_committed_view > self.local_view + 3 {
                info!(
                    target: "n42::observer",
                    local_view = self.local_view,
                    peer_committed_view = response.peer_committed_view,
                    "empty sync response but peer reports higher committed view; retrying with next peer"
                );
                let _ = self
                    .initiate_sync(self.local_view, response.peer_committed_view)
                    .await;
            }
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
                execution_output: None,
                leader_ready_unix_ms: 0,
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
            self.initiate_sync(self.local_view, response.peer_committed_view)
                .await;
        }
    }

    fn verify_sync_block_qc(&self, sync_block: &SyncBlock) -> bool {
        let resolved = resolve_validator_set_for_view(
            sync_block.view,
            self.validator_set.as_ref(),
            self.initial_validators.as_deref(),
            self.initial_fault_tolerance,
            self.epoch_length,
            self.epoch_schedule.as_ref(),
        );
        verify_sync_block_qc_against_set(sync_block, resolved.as_ref())
    }

    /// Checks if observer is behind the network and initiates sync if needed.
    async fn check_and_initiate_sync(&mut self) {
        if let Some((target_view, source)) = select_sync_target(
            self.local_view,
            self.highest_sync_target_view,
            self.highest_seen_view,
            self.last_gossip_sync_request_to_view,
        ) && let Some(requested_to_view) = self.initiate_sync(self.local_view, target_view).await
            && source == SyncTargetSource::GossipHint
        {
            self.last_gossip_sync_request_to_view = requested_to_view;
        }
    }

    async fn initiate_sync(&mut self, local_view: u64, target_view: u64) -> Option<u64> {
        if self.sync_in_flight {
            let timed_out = self
                .sync_started_at
                .map(|t| t.elapsed() > SYNC_TIMEOUT)
                .unwrap_or(false);

            if timed_out {
                warn!(target: "n42::observer", "sync request timed out, resetting");
                self.sync_in_flight = false;
                self.sync_started_at = None;
            } else {
                return None;
            }
        }

        let peers: Vec<_> = self.connected_peers.iter().copied().collect();
        if peers.is_empty() {
            return None;
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

        if let Err(e) = self.network.request_sync_reliable(peer, request).await {
            error!(target: "n42::observer", error = %e, "failed to send sync request");
            return None;
        }

        self.sync_in_flight = true;
        self.sync_started_at = Some(Instant::now());
        Some(capped_to_view)
    }

    fn store_committed_block(
        &mut self,
        view: u64,
        block_hash: B256,
        commit_qc: QuorumCertificate,
        payload: &[u8],
    ) {
        if self.committed_blocks.len() >= MAX_COMMITTED_BLOCKS {
            self.committed_blocks.pop_front();
        }
        self.committed_blocks.push_back(CommittedBlock {
            view,
            block_hash,
            commit_qc,
            payload: payload.to_vec(),
            raw_broadcast: Vec::new(), // observer replays; no local re-drive needed
            validator_changes: None,
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
    use alloy_primitives::{Address, B256};
    use bitvec::prelude::*;
    use n42_primitives::{BlsSecretKey, QuorumCertificate};

    fn test_key(seed: u8) -> BlsSecretKey {
        BlsSecretKey::key_gen(&[seed; 32]).expect("deterministic test key should be valid")
    }

    fn make_sync_block(view: u64, block_hash: B256, signer_seed: u8) -> (SyncBlock, ValidatorSet) {
        let key = test_key(signer_seed);
        let validators = vec![ValidatorInfo {
            address: Address::with_last_byte(0xEE),
            bls_public_key: key.public_key(),
            p2p_peer_id: None,
        }];
        let validator_set = ValidatorSet::new(&validators, 0);
        let signature = key.sign(&n42_consensus::protocol::quorum::commit_signing_message(
            view,
            &block_hash,
            &alloy_primitives::B256::ZERO,
        ));

        (
            SyncBlock {
                view,
                block_hash,
                commit_qc: QuorumCertificate {
                    view,
                    block_hash,
                    aggregate_signature: signature,
                    signers: bitvec![u8, Msb0; 1],
                },
                payload: vec![],
                validator_changes: None,
            },
            validator_set,
        )
    }

    #[test]
    fn test_verify_sync_block_no_validator_set_passes() {
        let (sync_block, _validator_set) = make_sync_block(7, B256::repeat_byte(0xAA), 0x41);
        assert!(verify_sync_block_qc_against_set(&sync_block, None));
    }

    #[test]
    fn test_verify_sync_block_rejects_hash_mismatch() {
        let (mut sync_block, validator_set) = make_sync_block(8, B256::repeat_byte(0xAB), 0x42);
        sync_block.block_hash = B256::repeat_byte(0xCD);

        assert!(!verify_sync_block_qc_against_set(
            &sync_block,
            Some(&validator_set)
        ));
    }

    #[test]
    fn test_verify_sync_block_rejects_view_mismatch() {
        let (mut sync_block, validator_set) = make_sync_block(9, B256::repeat_byte(0xAC), 0x43);
        sync_block.view = 10;

        assert!(!verify_sync_block_qc_against_set(
            &sync_block,
            Some(&validator_set)
        ));
    }

    #[test]
    fn test_verify_sync_block_accepts_valid_commit_qc() {
        let (sync_block, validator_set) = make_sync_block(11, B256::repeat_byte(0xAD), 0x44);
        assert!(verify_sync_block_qc_against_set(
            &sync_block,
            Some(&validator_set)
        ));
    }

    fn make_validators(count: usize) -> Vec<ValidatorInfo> {
        (0..count)
            .map(|i| {
                let sk = test_key(0x30 + i as u8);
                ValidatorInfo {
                    address: Address::with_last_byte(i as u8),
                    bls_public_key: sk.public_key(),
                    p2p_peer_id: None,
                }
            })
            .collect()
    }

    #[test]
    fn test_resolve_validator_set_for_view_uses_epoch_schedule() {
        let initial = make_validators(4);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("epoch_schedule.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&vec![
                serde_json::json!({
                    "start_epoch": 2,
                    "validators": make_validators(5),
                    "fault_tolerance": 1
                }),
                serde_json::json!({
                    "start_epoch": 4,
                    "validators": make_validators(7),
                    "fault_tolerance": 2
                }),
            ])
            .unwrap(),
        )
        .unwrap();
        let schedule = EpochSchedule::load(&path).unwrap().unwrap();

        let vs0 = resolve_validator_set_for_view(1, None, Some(&initial), 1, 10, Some(&schedule))
            .unwrap();
        let vs2 = resolve_validator_set_for_view(25, None, Some(&initial), 1, 10, Some(&schedule))
            .unwrap();
        let vs4 = resolve_validator_set_for_view(45, None, Some(&initial), 1, 10, Some(&schedule))
            .unwrap();

        assert_eq!(vs0.len(), 4);
        assert_eq!(vs2.len(), 5);
        assert_eq!(vs4.len(), 7);
    }

    #[test]
    fn test_select_sync_target_prefers_proven_progress() {
        let target = select_sync_target(10, 30, 100, 100).unwrap();
        assert_eq!(target, (30, SyncTargetSource::ProvenProgress));
    }

    #[test]
    fn test_select_sync_target_consumes_each_gossip_window_once() {
        let first = select_sync_target(10, 10, 40, 0).unwrap();
        assert_eq!(first, (40, SyncTargetSource::GossipHint));

        let second = select_sync_target(10, 10, 40, 40);
        assert!(second.is_none());

        let third = select_sync_target(30, 30, 200, 138).unwrap();
        assert_eq!(third, (200, SyncTargetSource::GossipHint));
    }
}
