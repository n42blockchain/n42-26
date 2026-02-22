use n42_primitives::consensus::{ConsensusMessage, NewView, TimeoutMessage};

use crate::error::{ConsensusError, ConsensusResult};
use crate::validator::LeaderSelector;
use super::quorum::{TimeoutCollector, timeout_signing_message};
use super::round::Phase;
use super::state_machine::{ConsensusEngine, EngineOutput, FUTURE_VIEW_WINDOW};

impl ConsensusEngine {
    /// Handles a view timeout triggered by the pacemaker.
    pub fn on_timeout(&mut self) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        if self.round_state.phase() == Phase::TimedOut {
            // Already timed out in this view: own timeout was already broadcast.
            // Only reset the pacemaker deadline so the select! loop doesn't spin.
            tracing::warn!(view, "view timed out (repeat, resetting pacemaker only)");
            self.pacemaker.reset_for_view(view, self.round_state.consecutive_timeouts());
            return Ok(());
        }

        tracing::warn!(view, "view timed out");
        self.round_state.timeout();

        // Reset pacemaker BEFORE processing to prevent the select! loop from spinning
        // on a deadline that stays in the past. If process_timeout forms a TC and calls
        // advance_to_view, the pacemaker will be reset again — a harmless double reset.
        self.pacemaker.reset_for_view(view, self.round_state.consecutive_timeouts());

        // Clear pending block data: similar to Tendermint's "prevote nil".
        self.pending_proposal = None;
        self.imported_blocks.clear();

        self.timeout_collector = Some(TimeoutCollector::new(view, self.validator_set().len()));

        let message = timeout_signing_message(view);
        let signature = self.secret_key.sign(&message);

        let timeout_msg = TimeoutMessage {
            view,
            high_qc: self.round_state.locked_qc().clone(),
            sender: self.my_index,
            signature,
        };

        self.emit(EngineOutput::BroadcastMessage(
            ConsensusMessage::Timeout(timeout_msg.clone()),
        ));

        self.process_timeout(timeout_msg)
    }

    /// Processes a timeout message (current or future view).
    pub(super) fn process_timeout(&mut self, timeout: TimeoutMessage) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        if timeout.view < view {
            return Ok(());
        }

        // Future-view timeout: advance to that view for TC formation.
        //
        // After crash/recovery, nodes may be at different views. Without view
        // synchronization, they cannot form TCs (which require quorum timeouts at the
        // SAME view). When a validator provides signed evidence of timing out at view V,
        // we join that view's timeout collection.
        if timeout.view > view {
            if timeout.view > view + FUTURE_VIEW_WINDOW {
                return Ok(());
            }

            // Verify BLS signature BEFORE advancing to prevent unauthenticated view jumps.
            let pk = self.validator_set().get_public_key(timeout.sender)?;
            let msg = timeout_signing_message(timeout.view);
            pk.verify(&msg, &timeout.signature).map_err(|_| ConsensusError::InvalidSignature {
                view: timeout.view,
                validator_index: timeout.sender,
            })?;

            tracing::info!(
                current_view = view,
                timeout_view = timeout.view,
                sender = timeout.sender,
                "advancing to higher timeout view for synchronization"
            );

            self.advance_to_view(timeout.view);
            self.round_state.timeout();

            // Second reset: advance_to_view resets with consecutive_timeouts=0, but timeout()
            // just incremented the counter. This applies the correct exponential backoff.
            self.pacemaker.reset_for_view(timeout.view, self.round_state.consecutive_timeouts());

            // Conditional creation: advance_to_view may have replayed buffered messages that
            // already created and populated a timeout_collector. Overwriting would lose those.
            let n_validators = self.validator_set().len();
            if self.timeout_collector.as_ref().is_none_or(|tc| tc.view() != timeout.view) {
                self.timeout_collector = Some(TimeoutCollector::new(timeout.view, n_validators));
            }

            if let Some(ref mut collector) = self.timeout_collector {
                collector.add_verified_timeout(
                    timeout.sender,
                    timeout.signature.clone(),
                    timeout.high_qc.clone(),
                )?;
            }

            // Broadcast our own timeout so other nodes can converge and form a TC.
            let own_msg = timeout_signing_message(timeout.view);
            let own_sig = self.secret_key.sign(&own_msg);
            let own_timeout = TimeoutMessage {
                view: timeout.view,
                high_qc: self.round_state.locked_qc().clone(),
                sender: self.my_index,
                signature: own_sig,
            };
            self.emit(EngineOutput::BroadcastMessage(ConsensusMessage::Timeout(own_timeout.clone())));

            return self.process_timeout(own_timeout);
        }

        // Current-view timeout processing.
        let pk = self.validator_set().get_public_key(timeout.sender)?;
        let msg = timeout_signing_message(view);
        pk.verify(&msg, &timeout.signature).map_err(|_| ConsensusError::InvalidSignature {
            view,
            validator_index: timeout.sender,
        })?;

        let n_validators = self.validator_set().len();
        let quorum_size = self.validator_set().quorum_size();
        let next_view = view.saturating_add(1);
        let next_leader = LeaderSelector::leader_for_view(next_view, self.validator_set());

        let collector = self
            .timeout_collector
            .get_or_insert_with(|| TimeoutCollector::new(view, n_validators));

        collector.add_verified_timeout(timeout.sender, timeout.signature, timeout.high_qc)?;

        tracing::debug!(
            view,
            sender = timeout.sender,
            count = collector.timeout_count(),
            "received timeout"
        );

        let should_form_tc = collector.has_quorum(quorum_size) && next_leader == self.my_index;

        if should_form_tc {
            self.try_form_tc_and_advance(view, next_view)?;
        }

        Ok(())
    }

    /// Processes a NewView message from the new leader.
    pub(super) fn process_new_view(&mut self, nv: NewView) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        if nv.view <= view {
            return Ok(());
        }

        let expected_leader = LeaderSelector::leader_for_view(nv.view, self.validator_set());
        if nv.leader != expected_leader {
            return Err(ConsensusError::InvalidProposer {
                view: nv.view,
                expected: expected_leader,
                actual: nv.leader,
            });
        }

        // Verify leader's signature to prevent Byzantine nodes from forging NewView messages.
        let pk = self.validator_set().get_public_key(nv.leader)?;
        let nv_msg = timeout_signing_message(nv.view);
        pk.verify(&nv_msg, &nv.signature).map_err(|_| ConsensusError::InvalidSignature {
            view: nv.view,
            validator_index: nv.leader,
        })?;

        // TC must be for the previous view (nv.view - 1).
        if nv.timeout_cert.view != nv.view.saturating_sub(1) {
            return Err(ConsensusError::InvalidTC {
                view: nv.timeout_cert.view,
                reason: format!(
                    "TC view {} does not match expected view {} (nv.view - 1)",
                    nv.timeout_cert.view,
                    nv.view.saturating_sub(1)
                ),
            });
        }
        super::quorum::verify_tc(&nv.timeout_cert, self.validator_set())?;

        // Verify TC's high_qc signature to prevent injection of forged QCs.
        // Genesis QC (view 0) is exempt — it has no real signatures.
        if nv.timeout_cert.high_qc.view > 0 {
            super::quorum::verify_qc(&nv.timeout_cert.high_qc, self.validator_set())?;
        }

        self.round_state.update_locked_qc(&nv.timeout_cert.high_qc);

        tracing::info!(old_view = view, new_view = nv.view, "received NewView, advancing");

        self.advance_to_view(nv.view);
        // Use actual view after advance: buffered-message replay may push view beyond nv.view.
        let actual_view = self.round_state.current_view();
        self.emit(EngineOutput::ViewChanged { new_view: actual_view });

        Ok(())
    }

    /// Builds a TC from the current timeout_collector and broadcasts NewView.
    ///
    /// Centralises TC-formation logic so the actual-view ViewChanged fix is applied
    /// consistently across all callsites.
    pub(super) fn try_form_tc_and_advance(
        &mut self,
        current_view: u64,
        next_view: u64,
    ) -> ConsensusResult<()> {
        let tc = match self.timeout_collector.as_ref() {
            Some(c) => c.build_tc(self.validator_set())?,
            None => {
                tracing::warn!(view = current_view, "timeout_collector disappeared during TC formation");
                return Ok(());
            }
        };

        tracing::info!(
            view = current_view,
            "TC formed, I am the new leader for view {}", next_view
        );

        self.round_state.update_locked_qc(&tc.high_qc);

        let nv_message = timeout_signing_message(next_view);
        let nv_sig = self.secret_key.sign(&nv_message);

        let new_view = NewView {
            view: next_view,
            timeout_cert: tc,
            leader: self.my_index,
            signature: nv_sig,
        };

        self.emit(EngineOutput::BroadcastMessage(ConsensusMessage::NewView(new_view)));

        self.advance_to_view(next_view);
        // Use actual view: advance_to_view may replay buffered messages pushing view further.
        let actual_view = self.round_state.current_view();
        self.emit(EngineOutput::ViewChanged { new_view: actual_view });

        Ok(())
    }
}
