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
//! - When the client stops (caught up, stalled, or the execution layer keeps
//!   refusing), the reverse hash walk resumes from the staged suffix.
//! - While caught up the client does nothing: no per-block work, no timer.

use super::ConsensusService;
use crate::h2_finality::{H2V4AnchorVerdict, verify_h2_v4_sync_anchor};
use crate::range_catchup::{AnchorMode, Deactivation, QcCheck, RangeBlock, RangeReply, extra_qc};
use alloy_primitives::B256;
use metrics::{counter, gauge};
use n42_consensus::{ConsensusEngine, ValidatorSet};
use n42_network::h2_v4::H2V4ChainIdentity;
use n42_network::h2_wire::H2QuorumCertificate;
use n42_network::{Gov5RangeOutcome, PeerId};
use tracing::{debug, info, warn};

/// Blocks imported per wake-up; the loop yields to consensus between batches.
const RANGE_IMPORT_BATCH: usize = 32;

/// Strict header-QC check: the set of the QC's own epoch from the engine
/// (current, staged next or historical) or the epoch schedule file, with no
/// fallback to the current or a same-size set (sync standard section 3.3).
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
                .and_then(|schedule| schedule.config_at_or_below(epochs.epoch_for_view(qc.view)))
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

fn decode_range_block(rlp: Vec<u8>) -> Result<RangeBlock, String> {
    let block = n42_network::decode_gov5_block_rlp(&rlp).map_err(|error| error.to_string())?;
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
                RangeReply::Blocks(rlps.into_iter().map(decode_range_block).collect())
            }
            Gov5RangeOutcome::Refused { code, message } => {
                debug!(target: "n42::interop::range", %source, start, code, %message, "range request refused");
                RangeReply::Refused
            }
            Gov5RangeOutcome::Failed { error, unsupported } => {
                debug!(target: "n42::interop::range", %source, start, %error, unsupported, "range request failed");
                RangeReply::Failed { unsupported }
            }
        };
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

        let ready = self.range_catchup.take_ready(
            self.head_block_number,
            self.head_block_hash,
            stop_at,
            RANGE_IMPORT_BATCH,
        );
        for block in ready {
            let decoded = match n42_network::decode_gov5_block_rlp(&block.rlp) {
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
            let imported = self
                .import_h2_v4_catchup_block(block.source, decoded, view)
                .await;
            if !imported || self.head_block_hash != block.hash {
                warn!(
                    target: "n42::interop::range",
                    number = block.number,
                    hash = %block.hash,
                    view,
                    imported,
                    "range catch-up block did not become the execution head"
                );
                if let Some(reason) = self.range_catchup.import_failed(block.number, now) {
                    self.on_range_catchup_stopped(reason).await;
                }
                return;
            }
            counter!("n42_range_catchup_imported_total").increment(1);
            if block.loose_only {
                counter!("n42_range_catchup_loose_only_total").increment(1);
            }
        }
        gauge!("n42_range_catchup_buffered_blocks")
            .set(self.range_catchup.buffered_blocks() as f64);
        if self.h2_v4_catchup_active {
            self.release_ready_h2_v4_catchup_blocks().await;
        }
    }

    async fn on_range_catchup_stopped(&mut self, reason: Deactivation) {
        let stats = self.range_catchup.stats().clone();
        info!(
            target: "n42::interop::range",
            ?reason,
            head = self.head_block_number,
            released = stats.released,
            released_loose_only = stats.released_loose_only,
            faults = stats.faults,
            tail_evicted = stats.tail_evicted,
            qc_verifications = stats.qc_verifications,
            "range catch-up stopped"
        );
        counter!("n42_range_catchup_stops_total", "reason" => match reason {
            Deactivation::CaughtUp => "caught_up",
            Deactivation::Stalled => "stalled",
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
            if let Ok(block) = n42_network::decode_gov5_block_rlp(rlp) {
                missing.push((*source, block.header.parent_hash));
            }
        }
        for (source, parent) in missing {
            self.request_h2_v4_gov5_block(source, parent);
        }
    }
}
