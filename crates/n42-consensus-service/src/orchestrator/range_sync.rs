//! Orchestrator side of the range catch-up client (`crate::range_catchup`).
//!
//! Handover with the existing paths (one importer at a time, no height
//! imported twice):
//! - The client activates only from the missing-parent branch of
//!   `handle_h2_v4_gov5_block`, when an authenticated block sits more than
//!   `activate_gap` heights above the execution head. While it is active that
//!   branch no longer walks the parent hash backwards; the bound block is
//!   staged as before and becomes the client's link and loose anchor.
//! - The client imports only `head + 1` whose parent is the head, on the
//!   orchestrator task, and never at or above the lowest height staged by the
//!   live path (`stop_at`). The staged suffix is released by the existing
//!   `release_ready_h2_v4_catchup_blocks` once the head reaches it. Both
//!   check the head before every import, and the task runs one at a time.
//! - A block whose header view does not exceed the execution-validated view
//!   (a leader tenure repeats views) imports without a forkchoice update and
//!   leaves the execution head where it was. The client then continues from
//!   its own position, and the staged suffix is released on top of it.
//! - When the client stops (caught up, stalled, diverged, or the execution
//!   layer keeps refusing), the reverse hash walk resumes from the staged
//!   suffix. That walk is not governed by `N42_SYNC_ANCHOR`, so `enforce` is
//!   not a security boundary yet.
//! - It never starts while blocks wait in the background import queue (a
//!   follower lagging at peak load holds those blocks already) or when the
//!   execution layer is delegated to an owner node (`N42_EL_OWNER_RPC`).
//! - While caught up the client does nothing: no per-block work, no timer,
//!   and the decode lane is not polled.

use super::ConsensusService;
use crate::h2_finality::{H2V4AnchorVerdict, verify_h2_v4_sync_anchor};
use crate::range_catchup::{
    AnchorMode, CommitQcRejection, Deactivation, QcCheck, RangeBlock, RangeReply, extra_qc,
    verify_commit_qc_reply,
};
use alloy_primitives::B256;
use metrics::{counter, gauge};
use n42_consensus::{ConsensusEngine, ValidatorSet};
use n42_network::h2_v4::H2V4ChainIdentity;
use n42_network::h2_wire::H2QuorumCertificate;
use n42_network::{CommitQcRequest, Gov5CommitQcOutcome, Gov5RangeOutcome, PeerId};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Blocks imported per wake-up; the loop yields to consensus between batches.
const RANGE_IMPORT_BATCH: usize = 32;

/// The anchor mode the client starts with: off when this key's execution
/// layer is delegated to an owner node (it does not execute, so it has
/// nothing to catch up), else `N42_SYNC_ANCHOR`.
pub(super) fn range_anchor_mode() -> AnchorMode {
    if std::env::var_os("N42_EL_OWNER_RPC").is_some_and(|owner| !owner.is_empty()) {
        AnchorMode::Off
    } else {
        AnchorMode::from_env()
    }
}

/// A range reply decoded off the orchestrator loop.
pub(super) type RangeDecoded = (PeerId, u64, RangeReply);

/// Decoded replies on their way back to the loop. A decode task sends only
/// while the round it was started in is current, under the same lock that
/// [`Self::retire`] takes, so once a round is retired nothing of it is held:
/// queued results are drained and late ones are dropped by their task.
pub(super) struct RangeDecodeLane {
    pub(super) tx: mpsc::UnboundedSender<RangeDecoded>,
    pub(super) rx: mpsc::UnboundedReceiver<RangeDecoded>,
    round: std::sync::Arc<std::sync::Mutex<u64>>,
}

impl RangeDecodeLane {
    pub(super) fn new() -> Self {
        // Bounded by the client's requests in flight.
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx,
            round: Default::default(),
        }
    }

    /// Decodes `rlps` on the blocking pool and queues the result, unless the
    /// round has been retired by then.
    pub(super) fn decode(&self, source: PeerId, start: u64, rlps: Vec<Vec<u8>>, profile1: bool) {
        let lane = self.tx.clone();
        let round = self.round.clone();
        let started = *round
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tokio::task::spawn_blocking(move || {
            let reply = RangeReply::Blocks(rlps.into_iter().map(|rlp| decode_range_block(rlp, profile1)).collect());
            let current = round
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *current == started {
                let _ = lane.send((source, start, reply));
            }
        });
    }

    /// Ends the current round: drops every queued result and makes decode
    /// tasks still running drop theirs.
    pub(super) fn retire(&mut self) {
        let mut round = self
            .round
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *round = round.wrapping_add(1);
        while self.rx.try_recv().is_ok() {}
    }
}

/// Strict header-QC check: the set of the QC's own epoch from the engine
/// (current, staged next or historical) or an entry of the epoch schedule
/// file for exactly that epoch, with no fallback to the current, the last
/// scheduled or a same-size set (sync standard section 3.3). The schedule
/// file does not record validator changes voted on chain, so an earlier
/// entry says nothing about a later epoch.
pub(super) fn range_qc_check(
    engine: &ConsensusEngine,
    schedule: Option<&crate::epoch_schedule::EpochSchedule>,
    identity: Option<H2V4ChainIdentity>,
    qc: &H2QuorumCertificate,
) -> QcCheck {
    let Some(identity) = identity else {
        return QcCheck::UnknownEpoch;
    };
    let epochs = engine.epoch_manager();
    let set = epochs
        .known_validator_set_for_view(qc.view)
        .cloned()
        .or_else(|| {
            schedule
                .and_then(|schedule| schedule.get_for_epoch(epochs.epoch_for_view(qc.view)))
                .and_then(|(validators, fault_tolerance)| {
                    ValidatorSet::try_new(validators, fault_tolerance).ok()
                })
        });
    let Some(set) = set else {
        return QcCheck::UnknownEpoch;
    };
    match verify_h2_v4_sync_anchor(qc, identity, &set) {
        H2V4AnchorVerdict::Verified => QcCheck::Verified,
        H2V4AnchorVerdict::PrepareDomain => QcCheck::PrepareDomain,
        H2V4AnchorVerdict::Invalid => QcCheck::Invalid,
    }
}

fn decode_range_block(rlp: Vec<u8>, profile1: bool) -> Result<RangeBlock, String> {
    let block = n42_network::decode_block_rlp_for(&rlp, profile1).map_err(|error| error.to_string())?;
    let view = n42_network::gov5_header_view(&block.header).map_err(|error| error.to_string())?;
    Ok(RangeBlock {
        number: block.header.number,
        hash: block.block_hash,
        parent: block.header.parent_hash,
        view,
        qc: extra_qc(block.header.extra_data.as_ref()),
        rlp,
    })
}

impl ConsensusService {
    /// Called only when an authenticated block's parent is missing. Returns
    /// true when the range client owns the gap (the caller then skips the
    /// reverse hash walk).
    pub(super) fn consider_range_catchup(&mut self, block: &n42_network::Gov5GossipBlock) -> bool {
        if self.h2_v4_identity.is_none() || self.range_catchup.mode() == AnchorMode::Off {
            return false;
        }
        let was_active = self.range_catchup.is_active();
        // A follower lagging behind its own background imports holds those
        // blocks already; the range client would download them again.
        if !was_active && (self.bg_import_in_flight || !self.bg_import_queue.is_empty()) {
            return false;
        }
        if !self.range_catchup.activate(
            self.head_block_number,
            block.header.number,
            std::time::Instant::now(),
        ) {
            return false;
        }
        // The bound block authenticates its own hash (any consensus binding,
        // hence loose) and links the fetched chain to anchors above it.
        self.range_catchup.note_link(
            block.block_hash,
            block.header.parent_hash,
            block.header.number,
        );
        self.range_catchup.note_loose_anchor(block.block_hash);
        if !was_active {
            // Replies of an earlier round still in the lane are stale.
            self.range_decoded.retire();
            self.range_catchup
                .note_strict_anchor(self.engine.last_committed_qc().block_hash);
            self.range_catchup
                .note_loose_anchor(self.engine.locked_qc().block_hash);
            let bound: Vec<B256> = self
                .h2_v4_block_views
                .iter()
                .filter(|(_, view)| **view > 0)
                .map(|(hash, _)| *hash)
                .collect();
            for hash in bound {
                self.range_catchup.note_loose_anchor(hash);
            }
            counter!("n42_range_catchup_activations_total").increment(1);
            info!(
                target: "n42::interop::range",
                mode = self.range_catchup.mode().as_str(),
                head = self.head_block_number,
                target = block.header.number,
                "range catch-up started"
            );
        }
        true
    }

    /// Highest height the range client may import: below the lowest block
    /// the live path has staged.
    fn range_stop_at(&self) -> Option<u64> {
        self.h2_v4_catchup_blocks
            .first_key_value()
            .map(|(height, _)| height.saturating_sub(1))
    }

    fn status_quorum(&self) -> usize {
        let validators = self.engine.epoch_manager().current_validator_set().len() as usize;
        validators.saturating_sub(1) / 3 + 1
    }

    pub(super) fn note_range_peer_status(&mut self, peer: PeerId, height: u64) {
        if self.h2_v4_identity.is_some() {
            self.range_catchup.note_peer_status(peer, height);
        }
    }

    /// A network outcome. Blocks are decoded on the blocking pool (a reply
    /// may hold 256 MiB); the result comes back through the decode lane.
    pub(super) async fn handle_range_reply(
        &mut self,
        source: PeerId,
        start: u64,
        outcome: Gov5RangeOutcome,
    ) {
        if !self.range_catchup.is_active() {
            return;
        }
        let reply = match outcome {
            Gov5RangeOutcome::Blocks(rlps) => {
                let profile1 = self.profile1.is_some();
                self.range_decoded.decode(source, start, rlps, profile1);
                return;
            }
            Gov5RangeOutcome::Refused { code, message } => {
                debug!(target: "n42::interop::range", %source, start, code, %message, "range request refused");
                RangeReply::Refused
            }
            Gov5RangeOutcome::Failed { error, unsupported } => {
                debug!(target: "n42::interop::range", %source, start, %error, unsupported, "range request failed");
                RangeReply::Failed { unsupported }
            }
            Gov5RangeOutcome::Faulty { reason } => RangeReply::Fault(reason),
            Gov5RangeOutcome::Unavailable {
                canonical_end,
                retry_after,
            } => {
                debug!(target: "n42::interop::range", %source, start, canonical_end, ?retry_after, "range peer served nothing");
                RangeReply::Unavailable {
                    canonical_end,
                    retry_after,
                }
            }
        };
        self.on_range_reply_decoded(source, start, reply).await;
    }

    /// A `commit_qc/1` answer (sync standard 5.2): checked down to the
    /// requested height and verified strictly; a verified anchor feeds the
    /// range client while it runs.
    pub(super) fn handle_commit_qc_reply(
        &mut self,
        source: PeerId,
        request: CommitQcRequest,
        outcome: Gov5CommitQcOutcome,
    ) {
        let payload = match outcome {
            Gov5CommitQcOutcome::Found(payload) => payload,
            Gov5CommitQcOutcome::Refused { code, message } => {
                debug!(target: "n42::interop::range", %source, number = request.number, code, %message, "commit_qc refused");
                return;
            }
            Gov5CommitQcOutcome::Failed { error, unsupported } => {
                debug!(target: "n42::interop::range", %source, number = request.number, %error, unsupported, "commit_qc failed");
                if unsupported {
                    self.range_catchup.note_commit_qc_unsupported(source);
                }
                return;
            }
        };
        let engine = &self.engine;
        let schedule = self.epoch_schedule.as_ref();
        let identity = self.h2_v4_identity;
        match verify_commit_qc_reply(&request, &payload, |qc| {
            range_qc_check(engine, schedule, identity, qc)
        }) {
            Ok(anchor) => {
                counter!("n42_commit_qc_anchors_verified_total").increment(1);
                info!(
                    target: "n42::interop::range",
                    %source,
                    qc_view = anchor.qc.view,
                    qc_block = %anchor.qc.block_hash,
                    headers = anchor.chain.len(),
                    "commit_qc anchor verified"
                );
                if self.range_catchup.is_active() {
                    self.range_catchup.note_commit_qc_anchor(&anchor);
                }
            }
            Err(CommitQcRejection::Fault(reason)) => {
                counter!("n42_commit_qc_faults_total", "reason" => reason).increment(1);
                warn!(target: "n42::interop::range", %source, number = request.number, reason, "commit_qc reply failed verification");
            }
            Err(CommitQcRejection::NotAnchor(check)) => {
                debug!(target: "n42::interop::range", %source, number = request.number, ?check, "commit_qc reply is not an anchor here");
            }
        }
    }

    pub(super) async fn on_range_reply_decoded(
        &mut self,
        source: PeerId,
        start: u64,
        reply: RangeReply,
    ) {
        if !self.range_catchup.is_active() {
            return;
        }
        let engine = &self.engine;
        let schedule = self.epoch_schedule.as_ref();
        let identity = self.h2_v4_identity;
        let mut verify = |qc: &H2QuorumCertificate| range_qc_check(engine, schedule, identity, qc);
        let report = self.range_catchup.on_reply(
            source,
            start,
            reply,
            std::time::Instant::now(),
            &mut verify,
        );
        if let Some(fault) = report.fault {
            counter!("n42_range_catchup_faults_total", "reason" => fault).increment(1);
            warn!(
                target: "n42::interop::range",
                %source,
                start,
                fault,
                excluded = report.peer_excluded,
                mode = self.range_catchup.mode().as_str(),
                "range reply failed screening"
            );
        }
        counter!("n42_range_catchup_blocks_accepted_total").increment(report.accepted as u64);
        self.drive_range_catchup().await;
    }

    /// One step of the client: anchors, requests, then a bounded import batch.
    pub(super) async fn drive_range_catchup(&mut self) {
        if !self.range_catchup.is_active() {
            return;
        }
        let now = std::time::Instant::now();
        self.range_catchup
            .note_strict_anchor(self.engine.last_committed_qc().block_hash);
        let stop_at = self.range_stop_at();
        let quorum = self.status_quorum();
        let (requests, stopped) =
            self.range_catchup
                .poll(now, self.head_block_number, quorum, stop_at);
        for request in requests {
            if let Err(error) =
                self.network
                    .request_gov5_range(request.peer, request.start, request.count)
            {
                debug!(target: "n42::interop::range", peer = %request.peer, %error, "could not queue range request");
                self.range_catchup.on_reply(
                    request.peer,
                    request.start,
                    RangeReply::Failed { unsupported: false },
                    now,
                    &mut |_| QcCheck::UnknownEpoch,
                );
            }
        }
        if let Some(reason) = stopped {
            self.on_range_catchup_stopped(reason).await;
            return;
        }
        // Sync standard 3.4 step 6: the next block is buffered but nothing
        // anchors it yet; ask a peer for a CommitQC reaching it.
        if let Some((peer, number, hash)) = self.range_catchup.tail_anchor_request(now)
            && let Err(error) = self.network.request_gov5_commit_qc(peer, number, hash)
        {
            debug!(target: "n42::interop::range", %peer, number, %error, "could not queue commit_qc request");
        }

        let ready = self.range_catchup.take_ready(
            self.head_block_number,
            self.head_block_hash,
            stop_at,
            RANGE_IMPORT_BATCH,
        );
        for block in ready {
            let decoded = match self.decode_wire_block(&block.rlp) {
                Ok(decoded) => decoded,
                Err(error) => {
                    warn!(target: "n42::interop::range", number = block.number, %error, "buffered range block failed repeat decoding");
                    if let Some(reason) = self.range_catchup.import_failed(block.number, now) {
                        self.on_range_catchup_stopped(reason).await;
                    }
                    return;
                }
            };
            // A certificate view, when one bound this hash, orders execution
            // exactly as the reverse walk does; the header view otherwise.
            let view = self
                .h2_v4_block_views
                .get(&block.hash)
                .copied()
                .filter(|view| *view > 0)
                .unwrap_or(block.view);
            // `true` without a head move is a block at or below the
            // execution-validated view (a repeated header view): executed,
            // made canonical by the next forkchoice update. The reverse walk
            // treats it the same way.
            if !self
                .import_h2_v4_catchup_block(block.source, decoded, view)
                .await
            {
                warn!(
                    target: "n42::interop::range",
                    number = block.number,
                    hash = %block.hash,
                    view,
                    "range catch-up block was not executed"
                );
                // E12 on a deferred chain: a block below a strict (commit)
                // anchor is committed. If this node refused it for carrying
                // a result other than its own, it halts with the evidence.
                if !block.loose_only
                    && self.profile1.is_some()
                    && self.profile1_check_committed(view, block.hash, None)
                {
                    return;
                }
                if let Some(reason) = self.range_catchup.import_failed(block.number, now) {
                    self.on_range_catchup_stopped(reason).await;
                }
                return;
            }
            self.range_catchup.imported(block.number, block.hash);
            // Below a strict (commit) anchor: committed, so `/1`, `/2` and
            // commit_qc may serve it once it is canonical.
            if !block.loose_only && self.h2_v4_identity.is_some() {
                self.network.note_committed_block(block.hash);
            }
            counter!("n42_range_catchup_imported_total").increment(1);
            if block.loose_only {
                counter!("n42_range_catchup_loose_only_total").increment(1);
            }
        }
        if let Some(reason) = self.range_catchup.take_stop() {
            self.on_range_catchup_stopped(reason).await;
            return;
        }
        gauge!("n42_range_catchup_buffered_blocks")
            .set(self.range_catchup.buffered_blocks() as f64);
        if self.h2_v4_catchup_active {
            match self.range_catchup.position() {
                (number, Some(hash)) if number > self.head_block_number => {
                    self.release_ready_h2_v4_catchup_blocks_from(number, hash)
                        .await;
                }
                _ => {
                    self.release_ready_h2_v4_catchup_blocks().await;
                }
            }
        }
    }

    pub(super) async fn on_range_catchup_stopped(&mut self, reason: Deactivation) {
        // Nothing of the stopped round may stay queued while caught up.
        self.range_decoded.retire();
        let stats = self.range_catchup.stats().clone();
        if reason == Deactivation::Diverged {
            error!(
                target: "n42::interop::range",
                head = self.head_block_number,
                head_hash = %self.head_block_hash,
                "range catch-up: the authenticated chain does not extend the execution head; \
                 the head is on another branch (operator attention needed)"
            );
        }
        info!(
            target: "n42::interop::range",
            ?reason,
            head = self.head_block_number,
            released = stats.released,
            released_loose_only = stats.released_loose_only,
            faults = stats.faults,
            tail_evicted = stats.tail_evicted,
            budget_evicted = stats.budget_evicted,
            conflicts = stats.conflicts,
            received = stats.blocks_received,
            qc_verifications = stats.qc_verifications,
            "range catch-up stopped"
        );
        counter!("n42_range_catchup_stops_total", "reason" => match reason {
            Deactivation::CaughtUp => "caught_up",
            Deactivation::Stalled => "stalled",
            Deactivation::Diverged => "diverged",
        })
        .increment(1);
        if self.h2_v4_catchup_active && self.release_ready_h2_v4_catchup_blocks().await {
            return;
        }
        self.resume_reverse_walk();
    }

    /// Hands the remaining gap back to the reverse hash walk: request the
    /// parent of every staged block whose parent is neither staged nor the
    /// execution head.
    fn resume_reverse_walk(&mut self) {
        let mut missing = Vec::new();
        for (height, (source, rlp, _)) in &self.h2_v4_catchup_blocks {
            let below = height.saturating_sub(1);
            if below <= self.head_block_number || self.h2_v4_catchup_blocks.contains_key(&below) {
                continue;
            }
            if let Ok(block) = self.decode_wire_block(rlp) {
                missing.push((*source, block.header.parent_hash));
            }
        }
        for (source, parent) in missing {
            self.request_h2_v4_gov5_block(source, parent);
        }
    }
}
