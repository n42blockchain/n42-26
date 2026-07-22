use crate::blob_port::BlobStorePort;
use crate::el::ExecutionLayer;
use crate::epoch_schedule::EpochSchedule;
use crate::exec_cache::ExecutionOutputCache;
use crate::h2_finality::{verify_h2_v4_decide, verify_h2_v4_shadow_message};
use crate::orchestrator::bad_block_cache::BadBlockCache;
use crate::orchestrator::{BlobSidecarBroadcast, BlockDataBroadcast, CommittedBlock};
use crate::replay_import::build_gov5_execution_data;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ExecutionData, ForkchoiceState, PayloadStatusEnum};
use metrics::{counter, gauge};
use n42_chainspec::ValidatorInfo;
use n42_consensus::{ValidatorSet, validator_changes_hash, verify_commit_qc};
use n42_network::{
    BlockSyncResponse, NetworkEvent, NetworkHandle, PeerId, SyncBlock, decode_gov5_block_rlp,
};
use n42_primitives::QuorumCertificate;
use std::collections::{HashMap, HashSet, VecDeque};
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

/// Bounds unauthenticated live block bodies and authenticated Decide records
/// retained while their counterpart is in flight.
const MAX_GOV5_LIVE_PENDING: usize = 512;

/// Maximum authenticated parent links retained while walking backwards from a
/// verified Gov5 Decide. Unlike [`MAX_GOV5_LIVE_PENDING`], these entries contain
/// no transactions or execution payloads, so a far-away finalized tip can be
/// authenticated without widening the untrusted block-body window.
const MAX_GOV5_FINALIZED_LINEAGE: usize = 65_536;

const GOV5_FETCH_RETRY_INTERVAL: Duration = Duration::from_secs(2);

const H2_V4_SHADOW_KINDS: usize = 7;

fn h2_v4_kind_index(kind: &str) -> usize {
    match kind {
        "proposal" => 0,
        "vote" => 1,
        "commit_vote" => 2,
        "prepare_qc" => 3,
        "timeout" => 4,
        "new_view" => 5,
        "decide" => 6,
        _ => unreachable!("H2-v4 message kind is exhaustive"),
    }
}

fn h2_v4_message_view(message: &n42_network::h2_wire::H2Message) -> u64 {
    use n42_network::h2_wire::H2Message;
    match message {
        H2Message::Proposal(value) => value.view,
        H2Message::Vote(value) | H2Message::CommitVote(value) => value.view,
        H2Message::PrepareQc(value) => value.view,
        H2Message::Timeout(value) => value.view,
        H2Message::NewView(value) => value.view,
        H2Message::Decide(value) => value.view,
    }
}

#[derive(Clone)]
struct PendingGov5LiveBlock {
    execution_data: ExecutionData,
    parent_hash: B256,
    number: u64,
    executed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Gov5LineageEntry {
    hash: B256,
    parent_hash: B256,
    number: u64,
}

#[derive(Clone, Copy, Debug)]
struct Gov5FinalityRecord {
    view: u64,
    source: PeerId,
}

/// A verified Decide authenticates its block hash. Each exact block fetch then
/// authenticates the next parent hash, forming a lightweight chain back to the
/// local head. Only after that complete chain is known do we advance forkchoice
/// in forward order; this keeps intermediate promotion tied to a finalized
/// descendant instead of trusting unauthenticated block gossip.
struct Gov5FinalizedCatchup {
    finalized_hash: B256,
    view: u64,
    preferred_peer: PeerId,
    expected_hash: B256,
    reverse_path: Vec<Gov5LineageEntry>,
    seen: HashSet<B256>,
    pending: VecDeque<Gov5LineageEntry>,
    pending_hashes: HashSet<B256>,
    discovery_complete: bool,
}

impl Gov5FinalizedCatchup {
    fn new(finalized_hash: B256, view: u64, preferred_peer: PeerId) -> Self {
        Self {
            finalized_hash,
            view,
            preferred_peer,
            expected_hash: finalized_hash,
            reverse_path: Vec::new(),
            seen: HashSet::new(),
            pending: VecDeque::new(),
            pending_hashes: HashSet::new(),
            discovery_complete: false,
        }
    }

    fn record_block(
        &mut self,
        entry: Gov5LineageEntry,
        local_head: B256,
    ) -> Result<(), &'static str> {
        if self.discovery_complete {
            return Ok(());
        }
        if entry.hash != self.expected_hash {
            return Err("block does not match the authenticated parent link");
        }
        if self.reverse_path.len() >= MAX_GOV5_FINALIZED_LINEAGE {
            return Err("authenticated lineage exceeds the catch-up bound");
        }
        if !self.seen.insert(entry.hash) {
            return Err("authenticated lineage contains a cycle");
        }
        if let Some(child) = self.reverse_path.last()
            && (child.parent_hash != entry.hash
                || entry.number.checked_add(1) != Some(child.number))
        {
            return Err("authenticated lineage has non-contiguous block numbers");
        }

        self.reverse_path.push(entry);
        self.expected_hash = entry.parent_hash;
        if entry.parent_hash == local_head {
            self.pending = self.reverse_path.iter().rev().copied().collect();
            self.pending_hashes = self.pending.iter().map(|entry| entry.hash).collect();
            self.discovery_complete = true;
        }
        Ok(())
    }
}

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

    // Deterministic Engine API payload rejections shared by gossip and sync paths.
    bad_blocks: BadBlockCache,

    // Gov5 live interop. Bodies are pre-executed, but only an independently
    // verified H2-v4 Decide may promote one to canonical/safe/finalized head.
    gov5_live_blocks: HashMap<B256, PendingGov5LiveBlock>,
    gov5_live_order: VecDeque<B256>,
    gov5_finality: HashMap<B256, Gov5FinalityRecord>,
    gov5_finality_order: VecDeque<B256>,
    gov5_fetch_requested_at: HashMap<B256, Instant>,
    gov5_finalized_catchup: Option<Gov5FinalizedCatchup>,
    h2_v4_shadow_verified: [u64; H2_V4_SHADOW_KINDS],
    h2_v4_shadow_rejected: [u64; H2_V4_SHADOW_KINDS],

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
            bad_blocks: BadBlockCache::default(),
            gov5_live_blocks: HashMap::new(),
            gov5_live_order: VecDeque::new(),
            gov5_finality: HashMap::new(),
            gov5_finality_order: VecDeque::new(),
            gov5_fetch_requested_at: HashMap::new(),
            gov5_finalized_catchup: None,
            h2_v4_shadow_verified: [0; H2_V4_SHADOW_KINDS],
            h2_v4_shadow_rejected: [0; H2_V4_SHADOW_KINDS],
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
            NetworkEvent::Gov5H2Message { source, message } => {
                let kind = message.kind().as_str();
                debug!(target: "n42::observer", %source, kind, "validated legacy gov5 H2 message");
            }
            NetworkEvent::Gov5Block { source, rlp } => {
                self.handle_gov5_block(source, rlp, None).await;
            }
            NetworkEvent::Gov5BlockFetched {
                source,
                requested_hash,
                rlp,
            } => {
                self.handle_gov5_block(source, rlp, Some(requested_hash))
                    .await;
            }
            NetworkEvent::Gov5BlockFetchFailed {
                source,
                block_hash,
                error,
            } => {
                warn!(target: "n42::observer", %source, %block_hash, %error, "gov5 block fetch failed");
                self.retry_gov5_block_fetch(source, block_hash);
            }
            NetworkEvent::H2V4Message { source, envelope } => {
                let kind = envelope.message.kind().as_str();
                let view = h2_v4_message_view(&envelope.message);
                let view_set = resolve_validator_set_for_view(
                    view,
                    self.validator_set.as_ref(),
                    self.initial_validators.as_deref(),
                    self.initial_fault_tolerance,
                    self.epoch_length,
                    self.epoch_schedule.as_ref(),
                );
                match view_set.as_ref() {
                    Some(view_set) => match verify_h2_v4_shadow_message(&envelope, view_set) {
                        Ok(proof) => {
                            counter!("n42_h2_v4_shadow_verified_total", "kind" => kind)
                                .increment(1);
                            let kind_index = h2_v4_kind_index(kind);
                            self.h2_v4_shadow_verified[kind_index] += 1;
                            if self.h2_v4_shadow_verified[kind_index] == 1 {
                                info!(target: "n42::observer", %source, kind, view = proof.view, "verified first H2-v4 shadow message of kind");
                            }
                            debug!(target: "n42::observer", %source, kind, view = proof.view, changes_hash = %envelope.changes_hash, "verified H2-v4 shadow message");
                            if let n42_network::h2_wire::H2Message::Decide(decide) =
                                &envelope.message
                            {
                                match verify_h2_v4_decide(&envelope, view_set) {
                                    Ok(proof) => {
                                        self.highest_seen_view =
                                            self.highest_seen_view.max(proof.view);
                                        self.highest_sync_target_view =
                                            self.highest_sync_target_view.max(proof.view);
                                        if proof.view <= self.local_view {
                                            debug!(
                                                target: "n42::observer",
                                                %source,
                                                view = proof.view,
                                                local_view = self.local_view,
                                                block_hash = %proof.block_hash,
                                                "ignored already-observed gov5 H2-v4 finality proof"
                                            );
                                            return;
                                        }
                                        info!(
                                            target: "n42::observer",
                                            %source,
                                            view = proof.view,
                                            block_hash = %proof.block_hash,
                                            "verified gov5 H2-v4 finality proof"
                                        );
                                        self.record_gov5_finality(
                                            proof.block_hash,
                                            proof.view,
                                            source,
                                        );
                                        self.advance_gov5_catchup_from_cache();
                                        if let Some(expected_hash) =
                                            self.gov5_catchup_expected_hash()
                                            && !self.gov5_live_blocks.contains_key(&expected_hash)
                                        {
                                            self.request_gov5_block(source, expected_hash);
                                        }
                                        self.drive_gov5_live_import().await;
                                    }
                                    Err(error) => warn!(
                                        target: "n42::observer",
                                        %source,
                                        view = decide.view,
                                        %error,
                                        "rejected gov5 H2-v4 finality proof"
                                    ),
                                }
                            }
                        }
                        Err(error) => {
                            counter!("n42_h2_v4_shadow_rejected_total", "kind" => kind)
                                .increment(1);
                            self.h2_v4_shadow_rejected[h2_v4_kind_index(kind)] += 1;
                            warn!(target: "n42::observer", %source, kind, view, %error, "rejected H2-v4 shadow message");
                        }
                    },
                    None => debug!(
                        target: "n42::observer",
                        %source,
                        kind,
                        view,
                        "cannot verify H2-v4 shadow message without validator set"
                    ),
                }
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

    async fn handle_gov5_block(
        &mut self,
        source: PeerId,
        rlp: Vec<u8>,
        requested_hash: Option<B256>,
    ) {
        if let Some(requested_hash) = requested_hash {
            self.gov5_fetch_requested_at.remove(&requested_hash);
        }
        let block = match decode_gov5_block_rlp(&rlp) {
            Ok(block) => block,
            Err(error) => {
                counter!("n42_gov5_live_blocks_rejected_total", "stage" => "decode").increment(1);
                warn!(target: "n42::observer", %source, %error, "rejected invalid gov5 live block");
                if let Some(requested_hash) = requested_hash {
                    self.retry_gov5_block_fetch(source, requested_hash);
                }
                return;
            }
        };
        let hash = block.block_hash;
        if let Some(requested_hash) = requested_hash
            && requested_hash != hash
        {
            counter!("n42_gov5_live_blocks_rejected_total", "stage" => "fetch_hash").increment(1);
            warn!(target: "n42::observer", %source, %requested_hash, received_hash = %hash, "gov5 peer returned a different block than requested");
            self.retry_gov5_block_fetch(source, requested_hash);
            return;
        }
        if hash == self.head_block_hash || self.bad_blocks.should_skip(hash, "gov5_live_gossip") {
            return;
        }
        let execution_data = match build_gov5_execution_data(
            hash,
            &block.header,
            &block.transactions,
        ) {
            Ok(payload) => payload,
            Err(error) => {
                counter!("n42_gov5_live_blocks_rejected_total", "stage" => "payload").increment(1);
                warn!(target: "n42::observer", %source, %hash, number = block.header.number, %error, "rejected non-reconstructible gov5 live block");
                if let Some(requested_hash) = requested_hash {
                    self.retry_gov5_block_fetch(source, requested_hash);
                }
                return;
            }
        };

        if !self.gov5_live_blocks.contains_key(&hash) {
            self.gov5_live_order.push_back(hash);
            self.gov5_live_blocks.insert(
                hash,
                PendingGov5LiveBlock {
                    execution_data,
                    parent_hash: block.header.parent_hash,
                    number: block.header.number,
                    executed: false,
                },
            );
            self.trim_gov5_live_cache();
            debug!(target: "n42::observer", %source, %hash, number = block.header.number, "accepted authenticated-shape gov5 live block body");
        }
        self.advance_gov5_catchup_from_cache();
        let parent_hash = block.header.parent_hash;
        if let Some(expected_hash) = self.gov5_catchup_expected_hash() {
            if !self.gov5_live_blocks.contains_key(&expected_hash) {
                self.request_gov5_block(source, expected_hash);
            }
        } else if parent_hash != self.head_block_hash
            && !self.gov5_live_blocks.contains_key(&parent_hash)
        {
            self.request_gov5_block(source, parent_hash);
        }
        self.drive_gov5_live_import().await;
    }

    fn request_gov5_block(&mut self, peer: PeerId, block_hash: B256) {
        if block_hash == self.head_block_hash || self.gov5_live_blocks.contains_key(&block_hash) {
            return;
        }
        if self.gov5_fetch_requested_at.len() >= MAX_GOV5_LIVE_PENDING
            && !self.gov5_fetch_requested_at.contains_key(&block_hash)
        {
            warn!(target: "n42::observer", %peer, %block_hash, "gov5 block fetch window is full");
            return;
        }
        let now = Instant::now();
        if self
            .gov5_fetch_requested_at
            .get(&block_hash)
            .is_some_and(|requested| now.duration_since(*requested) < GOV5_FETCH_RETRY_INTERVAL)
        {
            return;
        }
        match self.network.request_gov5_block_by_hash(peer, block_hash) {
            Ok(()) => {
                self.gov5_fetch_requested_at.insert(block_hash, now);
                debug!(target: "n42::observer", %peer, %block_hash, "requesting missing gov5 block by hash");
            }
            Err(error) => {
                warn!(target: "n42::observer", %peer, %block_hash, %error, "could not queue gov5 block fetch");
            }
        }
    }

    fn retry_gov5_block_fetch(&mut self, failed_peer: PeerId, block_hash: B256) {
        self.gov5_fetch_requested_at.remove(&block_hash);
        if let Some(peer) = self
            .connected_peers
            .iter()
            .copied()
            .find(|peer| *peer != failed_peer)
        {
            self.request_gov5_block(peer, block_hash);
        }
    }

    fn record_gov5_finality(&mut self, hash: B256, view: u64, source: PeerId) {
        if self
            .gov5_finality
            .insert(hash, Gov5FinalityRecord { view, source })
            .is_none()
        {
            self.gov5_finality_order.push_back(hash);
        }
        while self.gov5_finality_order.len() > MAX_GOV5_LIVE_PENDING {
            if let Some(evicted) = self.gov5_finality_order.pop_front() {
                self.gov5_finality.remove(&evicted);
            }
        }
        self.start_next_gov5_finalized_catchup();
    }

    fn start_next_gov5_finalized_catchup(&mut self) {
        if self.gov5_finalized_catchup.is_some() {
            return;
        }
        self.gov5_finality
            .retain(|_, record| record.view > self.local_view);
        self.gov5_finality_order
            .retain(|hash| self.gov5_finality.contains_key(hash));
        let Some((hash, record)) = self
            .gov5_finality
            .iter()
            .min_by_key(|(_, record)| record.view)
            .map(|(hash, record)| (*hash, *record))
        else {
            return;
        };
        self.gov5_finalized_catchup =
            Some(Gov5FinalizedCatchup::new(hash, record.view, record.source));
    }

    fn gov5_catchup_expected_hash(&self) -> Option<B256> {
        self.gov5_finalized_catchup
            .as_ref()
            .filter(|catchup| !catchup.discovery_complete)
            .map(|catchup| catchup.expected_hash)
    }

    fn advance_gov5_catchup_from_cache(&mut self) {
        loop {
            let Some(expected_hash) = self.gov5_catchup_expected_hash() else {
                return;
            };
            if expected_hash == self.head_block_hash {
                if let Some(catchup) = self.gov5_finalized_catchup.as_mut() {
                    catchup.discovery_complete = true;
                }
                return;
            }
            let Some(block) = self.gov5_live_blocks.get(&expected_hash) else {
                return;
            };
            let entry = Gov5LineageEntry {
                hash: expected_hash,
                parent_hash: block.parent_hash,
                number: block.number,
            };
            let result = self
                .gov5_finalized_catchup
                .as_mut()
                .expect("expected hash requires an active catch-up")
                .record_block(entry, self.head_block_hash);
            if let Err(error) = result {
                let catchup = self
                    .gov5_finalized_catchup
                    .take()
                    .expect("failed active catch-up exists");
                warn!(
                    target: "n42::observer",
                    finalized_hash = %catchup.finalized_hash,
                    view = catchup.view,
                    %error,
                    "abandoned inconsistent gov5 finalized lineage"
                );
                self.gov5_finality.remove(&catchup.finalized_hash);
                self.gov5_finality_order
                    .retain(|hash| *hash != catchup.finalized_hash);
                self.start_next_gov5_finalized_catchup();
                return;
            }
        }
    }

    fn trim_gov5_live_cache(&mut self) {
        while self.gov5_live_order.len() > MAX_GOV5_LIVE_PENDING {
            let protected = self
                .gov5_finalized_catchup
                .as_ref()
                .filter(|catchup| catchup.discovery_complete)
                .map(|catchup| &catchup.pending_hashes);
            let eviction_index = protected
                .and_then(|protected| {
                    self.gov5_live_order
                        .iter()
                        .position(|hash| !protected.contains(hash))
                })
                .unwrap_or(0);
            if let Some(evicted) = self.gov5_live_order.remove(eviction_index) {
                self.gov5_live_blocks.remove(&evicted);
            }
        }
    }

    fn gov5_finalized_path_to_head(&self, finalized_hash: B256) -> Option<Vec<(u64, B256)>> {
        let mut path = Vec::new();
        let mut cursor = finalized_hash;
        let mut child_number = None;
        loop {
            let block = self.gov5_live_blocks.get(&cursor)?;
            if !block.executed {
                return None;
            }
            if child_number.is_some_and(|child| block.number.checked_add(1) != Some(child)) {
                return None;
            }
            path.push((block.number, cursor));
            if block.parent_hash == self.head_block_hash {
                path.reverse();
                return Some(path);
            }
            if path.len() >= MAX_GOV5_LIVE_PENDING {
                return None;
            }
            child_number = Some(block.number);
            cursor = block.parent_hash;
        }
    }

    /// Imports a lineage only after every parent link from its verified Decide
    /// tip to the current head has been discovered. Forkchoice advances one
    /// authenticated ancestor at a time, allowing already-used block bodies to
    /// be discarded before the next forward fetch.
    async fn drive_gov5_finalized_catchup(&mut self) {
        loop {
            let Some(catchup) = self.gov5_finalized_catchup.as_ref() else {
                return;
            };
            if !catchup.discovery_complete {
                let expected_hash = catchup.expected_hash;
                let preferred_peer = catchup.preferred_peer;
                let peer = self
                    .connected_peers
                    .contains(&preferred_peer)
                    .then_some(preferred_peer)
                    .or_else(|| self.connected_peers.iter().next().copied())
                    .unwrap_or(preferred_peer);
                self.request_gov5_block(peer, expected_hash);
                return;
            }

            let Some(entry) = catchup.pending.front().copied() else {
                let catchup = self
                    .gov5_finalized_catchup
                    .take()
                    .expect("completed gov5 catch-up exists");
                self.local_view = catchup.view;
                self.highest_seen_view = self.highest_seen_view.max(catchup.view);
                self.highest_sync_target_view = self.highest_sync_target_view.max(catchup.view);
                gauge!("n42_observer_view").set(catchup.view as f64);
                counter!("n42_gov5_live_blocks_finalized_total").increment(1);
                info!(
                    target: "n42::observer",
                    finalized_hash = %catchup.finalized_hash,
                    view = catchup.view,
                    imported = catchup.reverse_path.len(),
                    "authenticated gov5 finalized catch-up completed"
                );
                self.gov5_finality
                    .retain(|_, record| record.view > catchup.view);
                self.gov5_finality_order
                    .retain(|hash| self.gov5_finality.contains_key(hash));
                self.start_next_gov5_finalized_catchup();
                self.advance_gov5_catchup_from_cache();
                continue;
            };

            let Some(block) = self.gov5_live_blocks.get(&entry.hash).cloned() else {
                let preferred_peer = catchup.preferred_peer;
                let peer = self
                    .connected_peers
                    .contains(&preferred_peer)
                    .then_some(preferred_peer)
                    .or_else(|| self.connected_peers.iter().next().copied())
                    .unwrap_or(preferred_peer);
                self.request_gov5_block(peer, entry.hash);
                return;
            };
            if block.number != entry.number
                || block.parent_hash != entry.parent_hash
                || block.parent_hash != self.head_block_hash
            {
                warn!(
                    target: "n42::observer",
                    hash = %entry.hash,
                    expected_parent = %self.head_block_hash,
                    received_parent = %block.parent_hash,
                    "refused non-contiguous gov5 finalized catch-up block"
                );
                return;
            }

            if !block.executed {
                if self
                    .bad_blocks
                    .should_skip(entry.hash, "gov5_finalized_catchup")
                {
                    return;
                }
                match self.el.new_payload(block.execution_data).await {
                    Ok(status) if matches!(status.status, PayloadStatusEnum::Valid) => {
                        counter!("n42_gov5_live_blocks_executed_total").increment(1);
                    }
                    Ok(status)
                        if matches!(
                            status.status,
                            PayloadStatusEnum::Syncing | PayloadStatusEnum::Accepted
                        ) =>
                    {
                        debug!(target: "n42::observer", hash = %entry.hash, status = ?status.status, "gov5 finalized catch-up awaits EL execution");
                        return;
                    }
                    Ok(status) => {
                        self.bad_blocks.insert_if_invalid(
                            entry.hash,
                            &status.status,
                            "gov5_finalized_catchup",
                        );
                        warn!(target: "n42::observer", hash = %entry.hash, status = ?status.status, "new_payload rejected authenticated gov5 catch-up block");
                        return;
                    }
                    Err(error) => {
                        error!(target: "n42::observer", hash = %entry.hash, %error, "new_payload failed for authenticated gov5 catch-up block");
                        return;
                    }
                }
            }

            let state = ForkchoiceState {
                head_block_hash: entry.hash,
                safe_block_hash: entry.hash,
                finalized_block_hash: entry.hash,
            };
            match self.el.fork_choice_updated(state).await {
                Ok(result) if matches!(result.payload_status.status, PayloadStatusEnum::Valid) => {}
                Ok(result) => {
                    warn!(target: "n42::observer", hash = %entry.hash, status = ?result.payload_status.status, "EL did not promote authenticated gov5 catch-up block");
                    return;
                }
                Err(error) => {
                    error!(target: "n42::observer", hash = %entry.hash, %error, "fork_choice_updated failed for authenticated gov5 catch-up block");
                    return;
                }
            }

            self.head_block_hash = entry.hash;
            self.blocks_imported += 1;
            counter!("n42_observer_blocks_imported").increment(1);
            self.gov5_live_blocks.remove(&entry.hash);
            self.gov5_live_order.retain(|hash| *hash != entry.hash);
            self.gov5_fetch_requested_at.remove(&entry.hash);
            let catchup = self
                .gov5_finalized_catchup
                .as_mut()
                .expect("active catch-up still exists");
            catchup.pending.pop_front();
            catchup.pending_hashes.remove(&entry.hash);
        }
    }

    /// Drives the two independent halves of live following to a fixed point:
    /// bodies may execute before finality, but FCU promotion requires both a
    /// Valid EL result and a verified H2-v4 Decide for the same hash.
    async fn drive_gov5_live_import(&mut self) {
        // While a distant finalized lineage is still being discovered, none of
        // the cached descendants can execute: their parent chain is incomplete.
        // Keep this phase metadata-only so every fetched ancestor does O(1)
        // work instead of resubmitting the entire bounded body cache to the EL.
        if self
            .gov5_finalized_catchup
            .as_ref()
            .is_some_and(|catchup| !catchup.discovery_complete)
        {
            self.drive_gov5_finalized_catchup().await;
            return;
        }

        let mut candidates: Vec<_> = self
            .gov5_live_blocks
            .iter()
            .filter_map(|(hash, block)| (!block.executed).then_some((block.number, *hash)))
            .collect();
        candidates.sort_unstable();
        let mut rejected = Vec::new();
        for (_, hash) in candidates {
            if self.bad_blocks.should_skip(hash, "gov5_live_pre_submit") {
                rejected.push(hash);
                continue;
            }
            let Some(payload) = self
                .gov5_live_blocks
                .get(&hash)
                .map(|block| block.execution_data.clone())
            else {
                continue;
            };
            match self.el.new_payload(payload).await {
                Ok(status) if matches!(status.status, PayloadStatusEnum::Valid) => {
                    if let Some(block) = self.gov5_live_blocks.get_mut(&hash) {
                        block.executed = true;
                    }
                    counter!("n42_gov5_live_blocks_executed_total").increment(1);
                }
                Ok(status)
                    if matches!(
                        status.status,
                        PayloadStatusEnum::Syncing | PayloadStatusEnum::Accepted
                    ) =>
                {
                    debug!(target: "n42::observer", %hash, status = ?status.status, "gov5 live block awaits parent execution");
                }
                Ok(status) => {
                    self.bad_blocks
                        .insert_if_invalid(hash, &status.status, "gov5_live_import");
                    warn!(target: "n42::observer", %hash, status = ?status.status, "new_payload rejected gov5 live block");
                    rejected.push(hash);
                }
                Err(error) => {
                    error!(target: "n42::observer", %hash, %error, "new_payload failed for gov5 live block");
                }
            }
        }
        for hash in rejected {
            self.gov5_live_blocks.remove(&hash);
            self.gov5_live_order.retain(|queued| *queued != hash);
            self.gov5_finality.remove(&hash);
            self.gov5_finality_order.retain(|queued| *queued != hash);
        }

        self.drive_gov5_finalized_catchup().await;
        if self.gov5_finalized_catchup.is_some() {
            return;
        }

        loop {
            let next = self
                .gov5_finality
                .iter()
                .filter_map(|(hash, record)| {
                    self.gov5_finalized_path_to_head(*hash)
                        .map(|path| (record.view, *hash, path))
                })
                .min_by_key(|(view, _, _)| *view);
            let Some((view, hash, path)) = next else {
                break;
            };
            let first_number = path.first().map(|(number, _)| *number).unwrap_or_default();
            let number = path.last().map(|(number, _)| *number).unwrap_or_default();
            let state = ForkchoiceState {
                head_block_hash: hash,
                safe_block_hash: hash,
                finalized_block_hash: hash,
            };
            let promoted = match self.el.fork_choice_updated(state).await {
                Ok(result) => matches!(result.payload_status.status, PayloadStatusEnum::Valid),
                Err(error) => {
                    error!(target: "n42::observer", %hash, %error, "fork_choice_updated failed for finalized gov5 live block");
                    false
                }
            };
            if !promoted {
                warn!(target: "n42::observer", %hash, view, "EL did not promote finalized gov5 live block");
                break;
            }

            self.head_block_hash = hash;
            self.local_view = view;
            self.highest_seen_view = self.highest_seen_view.max(view);
            self.highest_sync_target_view = self.highest_sync_target_view.max(view);
            self.blocks_imported += path.len() as u64;
            counter!("n42_observer_blocks_imported").increment(path.len() as u64);
            counter!("n42_gov5_live_blocks_finalized_total").increment(1);
            gauge!("n42_observer_view").set(view as f64);
            info!(target: "n42::observer", %hash, first_number, number, imported = path.len(), view, "finalized gov5 live lineage imported");
            self.gov5_finality.remove(&hash);
            self.gov5_finality_order.retain(|queued| *queued != hash);
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
        let _ = self.import_block_data(broadcast, None).await;
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
    ) -> bool {
        let hash = broadcast.block_hash;
        let view = broadcast.view;
        let has_commit_proof = commit_qc.is_some();

        if self.bad_blocks.should_skip(hash, "observer_import") {
            return false;
        }

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
                return false;
            }
        };
        let execution_data: alloy_rpc_types_engine::ExecutionData = match serde_json::from_slice(
            &payload_json,
        ) {
            Ok(d) => d,
            Err(e) => {
                warn!(target: "n42::observer", %hash, error = %e, "failed to parse execution payload JSON");
                return false;
            }
        };
        if execution_data.block_hash() != hash {
            warn!(
                target: "n42::observer",
                %hash,
                payload_hash = %execution_data.block_hash(),
                "observer envelope hash does not match execution payload; dropping"
            );
            counter!("n42_block_data_payload_hash_mismatch_total").increment(1);
            return false;
        }
        if self
            .bad_blocks
            .should_skip(hash, "observer_import_pre_submit")
        {
            return false;
        }

        // Compact Block: load execution output to skip EVM re-execution.
        let compact_injected = if let Some(ref exec_compressed) = broadcast.execution_output
            && crate::orchestrator::compact_block_enabled()
            && let Some(ref cache) = self.exec_output_cache
        {
            cache.inject(hash, exec_compressed, "observer_import")
        } else {
            false
        };

        match self.el.new_payload(execution_data).await {
            Ok(status) => {
                if matches!(status.status, PayloadStatusEnum::Valid) {
                    if !has_commit_proof {
                        debug!(
                            target: "n42::observer",
                            view,
                            %hash,
                            "validated gossip block without commit_qc; waiting for sync proof before following head"
                        );
                        return true;
                    }

                    let fcu_state = ForkchoiceState {
                        head_block_hash: hash,
                        safe_block_hash: hash,
                        finalized_block_hash: hash,
                    };
                    let fcu_valid = match self.el.fork_choice_updated(fcu_state).await {
                        Ok(result) => {
                            let valid =
                                matches!(result.payload_status.status, PayloadStatusEnum::Valid);
                            if !valid {
                                warn!(target: "n42::observer", %hash, status = ?result.payload_status.status, "fork_choice_updated did not validate observer head; waiting for retry");
                            }
                            valid
                        }
                        Err(e) => {
                            error!(target: "n42::observer", %hash, error = %e, "fork_choice_updated failed");
                            false
                        }
                    };
                    if fcu_valid {
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
                    true
                } else if matches!(
                    status.status,
                    PayloadStatusEnum::Syncing | PayloadStatusEnum::Accepted
                ) {
                    if compact_injected && let Some(ref cache) = self.exec_output_cache {
                        cache.evict(hash);
                    }
                    debug!(target: "n42::observer", %hash, view, status = ?status.status, "new_payload has not executed observer block yet");
                    true
                } else {
                    if compact_injected && let Some(ref cache) = self.exec_output_cache {
                        cache.evict(hash);
                    }
                    self.bad_blocks
                        .insert_if_invalid(hash, &status.status, "observer_import");
                    warn!(target: "n42::observer", %hash, view, status = ?status.status, "new_payload rejected block");
                    false
                }
            }
            Err(e) => {
                if compact_injected && let Some(ref cache) = self.exec_output_cache {
                    cache.evict(hash);
                }
                error!(target: "n42::observer", %hash, error = %e, "new_payload call failed");
                false
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
            execution_lineage: Vec::new(),
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

            if self
                .bad_blocks
                .should_skip(sync_block.block_hash, "observer_sync_response")
            {
                counter!("n42_sync_bad_blocks_filtered_total").increment(1);
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
            if self.import_block_data(broadcast, Some(commit_qc)).await {
                imported += 1;
            }
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
            validator_changes: None,
            execution_lineage: Vec::new(),
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
            h2_shadow_verified = ?self.h2_v4_shadow_verified,
            h2_shadow_rejected = ?self.h2_v4_shadow_rejected,
            "status"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::el::ElError;
    use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
    use alloy_rpc_types_engine::{
        ExecutionPayload, ExecutionPayloadSidecar, ExecutionPayloadV1, ForkchoiceUpdated,
        PayloadAttributes, PayloadId, PayloadStatus,
    };
    use bitvec::prelude::*;
    use n42_network::NetworkCommand;
    use n42_primitives::{BlsSecretKey, QuorumCertificate};
    use std::sync::Mutex;

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

    fn numbered_hash(number: u64) -> B256 {
        let mut bytes = [0u8; 32];
        bytes[24..].copy_from_slice(&number.to_be_bytes());
        B256::from(bytes)
    }

    fn test_execution_data(
        parent_hash: B256,
        block_hash: B256,
        block_number: u64,
    ) -> ExecutionData {
        ExecutionData::new(
            ExecutionPayload::V1(ExecutionPayloadV1 {
                parent_hash,
                fee_recipient: Address::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::ZERO,
                prev_randao: B256::ZERO,
                block_number,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: block_number,
                extra_data: Bytes::new(),
                base_fee_per_gas: U256::from(1),
                block_hash,
                transactions: Vec::new(),
            }),
            ExecutionPayloadSidecar::none(),
        )
    }

    #[derive(Default)]
    struct CatchupExecutionLayer {
        fcu_heads: Mutex<Vec<B256>>,
    }

    #[async_trait::async_trait]
    impl ExecutionLayer for CatchupExecutionLayer {
        async fn new_payload(&self, _payload: ExecutionData) -> Result<PayloadStatus, ElError> {
            Ok(PayloadStatus::from_status(PayloadStatusEnum::Valid))
        }

        async fn fork_choice_updated(
            &self,
            state: ForkchoiceState,
        ) -> Result<ForkchoiceUpdated, ElError> {
            self.fcu_heads.lock().unwrap().push(state.head_block_hash);
            Ok(ForkchoiceUpdated::from_status(PayloadStatusEnum::Valid))
        }

        async fn fork_choice_updated_with_attrs(
            &self,
            _state: ForkchoiceState,
            _attrs: PayloadAttributes,
        ) -> Result<ForkchoiceUpdated, ElError> {
            Ok(ForkchoiceUpdated::from_status(PayloadStatusEnum::Valid))
        }

        async fn resolve_payload(
            &self,
            _id: PayloadId,
            _kind: crate::el::ResolveKind,
        ) -> Option<Result<crate::el::BuiltBlock, ElError>> {
            None
        }
    }

    #[test]
    fn test_gov5_finalized_catchup_authenticates_more_than_body_window() {
        let block_count = MAX_GOV5_LIVE_PENDING + 513;
        let head = numbered_hash(0);
        let mut catchup = Gov5FinalizedCatchup::new(
            numbered_hash(block_count as u64),
            block_count as u64,
            PeerId::random(),
        );

        for number in (1..=block_count as u64).rev() {
            catchup
                .record_block(
                    Gov5LineageEntry {
                        hash: numbered_hash(number),
                        parent_hash: numbered_hash(number - 1),
                        number,
                    },
                    head,
                )
                .unwrap();
        }

        assert!(catchup.discovery_complete);
        assert_eq!(catchup.pending.len(), block_count);
        assert_eq!(catchup.pending.front().unwrap().number, 1);
        assert_eq!(catchup.pending.back().unwrap().number, block_count as u64);
    }

    #[tokio::test]
    async fn test_gov5_finalized_catchup_imports_one_body_window_then_refetches_forward() {
        let block_count = MAX_GOV5_LIVE_PENDING + 513;
        let head = numbered_hash(0);
        let peer = PeerId::random();
        let mut catchup =
            Gov5FinalizedCatchup::new(numbered_hash(block_count as u64), block_count as u64, peer);
        for number in (1..=block_count as u64).rev() {
            catchup
                .record_block(
                    Gov5LineageEntry {
                        hash: numbered_hash(number),
                        parent_hash: numbered_hash(number - 1),
                        number,
                    },
                    head,
                )
                .unwrap();
        }

        let (command_tx, _command_rx) = mpsc::channel(1024);
        let (priority_tx, mut priority_rx) = mpsc::channel(1);
        let network = NetworkHandle::new(command_tx, priority_tx);
        let (_event_tx, event_rx) = mpsc::channel(1);
        let execution = Arc::new(CatchupExecutionLayer::default());
        let mut observer = ObserverOrchestrator::new(network, event_rx, execution.clone(), head);
        observer.gov5_finalized_catchup = Some(catchup);
        for number in 1..=MAX_GOV5_LIVE_PENDING as u64 {
            let hash = numbered_hash(number);
            observer.gov5_live_order.push_back(hash);
            observer.gov5_live_blocks.insert(
                hash,
                PendingGov5LiveBlock {
                    execution_data: test_execution_data(numbered_hash(number - 1), hash, number),
                    parent_hash: numbered_hash(number - 1),
                    number,
                    executed: true,
                },
            );
        }

        observer.drive_gov5_finalized_catchup().await;

        assert_eq!(observer.head_block_hash, numbered_hash(512));
        assert!(observer.gov5_live_blocks.is_empty());
        assert_eq!(
            observer
                .gov5_finalized_catchup
                .as_ref()
                .unwrap()
                .pending
                .len(),
            513
        );
        {
            let fcu_heads = execution.fcu_heads.lock().unwrap();
            assert_eq!(fcu_heads.len(), MAX_GOV5_LIVE_PENDING);
            assert_eq!(fcu_heads.first().copied(), Some(numbered_hash(1)));
            assert_eq!(fcu_heads.last().copied(), Some(numbered_hash(512)));
        }
        assert!(matches!(
            priority_rx.recv().await,
            Some(NetworkCommand::RequestGov5BlockByHash { peer: requested_peer, block_hash })
                if requested_peer == peer && block_hash == numbered_hash(513)
        ));
    }

    #[test]
    fn test_gov5_finalized_catchup_cache_protects_pending_lineage() {
        let block_count = MAX_GOV5_LIVE_PENDING + 1;
        let head = numbered_hash(0);
        let mut catchup = Gov5FinalizedCatchup::new(
            numbered_hash(block_count as u64),
            block_count as u64,
            PeerId::random(),
        );
        for number in (1..=block_count as u64).rev() {
            catchup
                .record_block(
                    Gov5LineageEntry {
                        hash: numbered_hash(number),
                        parent_hash: numbered_hash(number - 1),
                        number,
                    },
                    head,
                )
                .unwrap();
        }

        let (command_tx, _command_rx) = mpsc::channel(1);
        let (priority_tx, _priority_rx) = mpsc::channel(1);
        let network = NetworkHandle::new(command_tx, priority_tx);
        let (_event_tx, event_rx) = mpsc::channel(1);
        let execution = Arc::new(CatchupExecutionLayer::default());
        let mut observer = ObserverOrchestrator::new(network, event_rx, execution, head);
        observer.gov5_finalized_catchup = Some(catchup);
        for number in 1..=MAX_GOV5_LIVE_PENDING as u64 {
            let hash = numbered_hash(number);
            observer.gov5_live_order.push_back(hash);
            observer.gov5_live_blocks.insert(
                hash,
                PendingGov5LiveBlock {
                    execution_data: test_execution_data(numbered_hash(number - 1), hash, number),
                    parent_hash: numbered_hash(number - 1),
                    number,
                    executed: false,
                },
            );
        }
        let unrelated_tip = numbered_hash(10_000);
        observer.gov5_live_order.push_back(unrelated_tip);
        observer.gov5_live_blocks.insert(
            unrelated_tip,
            PendingGov5LiveBlock {
                execution_data: test_execution_data(B256::ZERO, unrelated_tip, 10_000),
                parent_hash: B256::ZERO,
                number: 10_000,
                executed: false,
            },
        );

        observer.trim_gov5_live_cache();

        assert_eq!(observer.gov5_live_blocks.len(), MAX_GOV5_LIVE_PENDING);
        assert!(!observer.gov5_live_blocks.contains_key(&unrelated_tip));
        for number in 1..=MAX_GOV5_LIVE_PENDING as u64 {
            assert!(
                observer
                    .gov5_live_blocks
                    .contains_key(&numbered_hash(number))
            );
        }
    }

    #[test]
    fn test_gov5_finalized_catchup_binds_each_requested_hash() {
        let mut catchup = Gov5FinalizedCatchup::new(numbered_hash(3), 3, PeerId::random());
        let error = catchup
            .record_block(
                Gov5LineageEntry {
                    hash: numbered_hash(2),
                    parent_hash: numbered_hash(1),
                    number: 2,
                },
                numbered_hash(0),
            )
            .unwrap_err();

        assert_eq!(error, "block does not match the authenticated parent link");
        assert!(catchup.reverse_path.is_empty());
    }

    #[test]
    fn test_gov5_finalized_catchup_rejects_non_contiguous_numbers() {
        let mut catchup = Gov5FinalizedCatchup::new(numbered_hash(3), 3, PeerId::random());
        catchup
            .record_block(
                Gov5LineageEntry {
                    hash: numbered_hash(3),
                    parent_hash: numbered_hash(2),
                    number: 3,
                },
                numbered_hash(0),
            )
            .unwrap();
        let error = catchup
            .record_block(
                Gov5LineageEntry {
                    hash: numbered_hash(2),
                    parent_hash: numbered_hash(1),
                    number: 1,
                },
                numbered_hash(0),
            )
            .unwrap_err();

        assert_eq!(
            error,
            "authenticated lineage has non-contiguous block numbers"
        );
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
