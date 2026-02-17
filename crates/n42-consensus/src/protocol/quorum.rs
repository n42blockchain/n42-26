use alloy_primitives::B256;
use bitvec::prelude::*;
use n42_primitives::{
    bls::{AggregateSignature, BlsPublicKey, BlsSignature},
    consensus::{QuorumCertificate, TimeoutCertificate, ViewNumber},
};
use std::collections::{HashMap, HashSet};

use crate::error::{ConsensusError, ConsensusResult};
use crate::validator::ValidatorSet;

/// Collects votes for a specific view and produces a QuorumCertificate
/// once 2f+1 votes are received.
#[derive(Debug)]
pub struct VoteCollector {
    /// View this collector is gathering votes for.
    view: ViewNumber,
    /// Block hash being voted on.
    block_hash: B256,
    /// Collected signatures indexed by validator index.
    votes: HashMap<u32, BlsSignature>,
    /// Total validators in the set.
    set_size: u32,
    /// Validator indices whose signatures have already been verified
    /// (in process_vote/process_commit_vote). Skipped during build_qc
    /// to avoid redundant pairing operations (~50% savings).
    verified: HashSet<u32>,
}

impl VoteCollector {
    /// Creates a new vote collector for the given view and block.
    pub fn new(view: ViewNumber, block_hash: B256, set_size: u32) -> Self {
        Self {
            view,
            block_hash,
            votes: HashMap::new(),
            set_size,
            verified: HashSet::new(),
        }
    }

    /// Adds a vote. Returns `Err` if duplicate.
    pub fn add_vote(
        &mut self,
        validator_index: u32,
        signature: BlsSignature,
    ) -> ConsensusResult<()> {
        if self.votes.contains_key(&validator_index) {
            return Err(ConsensusError::DuplicateVote {
                view: self.view,
                validator_index,
            });
        }
        self.votes.insert(validator_index, signature);
        Ok(())
    }

    /// Adds a vote that has already been signature-verified by the caller.
    /// The `verified` set is checked in `build_qc_with_message()` to skip
    /// redundant re-verification, saving ~50% of pairing operations.
    pub fn add_verified_vote(
        &mut self,
        validator_index: u32,
        signature: BlsSignature,
    ) -> ConsensusResult<()> {
        self.add_vote(validator_index, signature)?;
        self.verified.insert(validator_index);
        Ok(())
    }

    /// Returns the block hash this collector is gathering votes for.
    pub fn block_hash(&self) -> B256 {
        self.block_hash
    }

    /// Returns the current number of collected votes.
    pub fn vote_count(&self) -> usize {
        self.votes.len()
    }

    /// Checks if we have enough votes for a quorum.
    pub fn has_quorum(&self, quorum_size: usize) -> bool {
        self.votes.len() >= quorum_size
    }

    /// Builds a QuorumCertificate by aggregating collected signatures.
    ///
    /// Verifies each vote against the validator's public key before aggregating.
    /// Uses the standard signing message (view || block_hash).
    /// Returns an error if there aren't enough valid votes for a quorum.
    pub fn build_qc(
        &self,
        validator_set: &ValidatorSet,
    ) -> ConsensusResult<QuorumCertificate> {
        let message = signing_message(self.view, &self.block_hash);
        self.build_qc_with_message(validator_set, &message)
    }

    /// Builds a QuorumCertificate using a custom signing message for verification.
    ///
    /// This is needed for CommitVote (Round 2) which uses a different message
    /// format ("commit" || view || block_hash) than the standard Round 1 vote.
    ///
    /// Votes with invalid signatures are skipped (defense-in-depth). The QC is
    /// formed from the remaining valid votes, failing only if fewer than
    /// `quorum_size` valid votes remain.
    pub fn build_qc_with_message(
        &self,
        validator_set: &ValidatorSet,
        message: &[u8],
    ) -> ConsensusResult<QuorumCertificate> {
        let quorum_size = validator_set.quorum_size();
        if self.votes.len() < quorum_size {
            return Err(ConsensusError::InsufficientVotes {
                view: self.view,
                have: self.votes.len(),
                need: quorum_size,
            });
        }

        // Verify each vote and collect valid signatures, skipping invalid ones.
        // Votes already verified in process_vote() (tracked in `self.verified`)
        // are not re-verified, saving ~50% of pairing operations.
        let mut valid_sigs: Vec<&BlsSignature> = Vec::new();
        let mut signers = bitvec![u8, Msb0; 0; self.set_size as usize];

        for (&idx, sig) in &self.votes {
            // Bounds check first
            if idx >= self.set_size {
                tracing::warn!(view = self.view, idx, "skipping out-of-range validator in QC build");
                continue;
            }

            // Skip re-verification for votes already verified in process_vote().
            if self.verified.contains(&idx) {
                valid_sigs.push(sig);
                signers.set(idx as usize, true);
                continue;
            }

            let pk = match validator_set.get_public_key(idx) {
                Ok(pk) => pk,
                Err(_) => {
                    tracing::warn!(view = self.view, idx, "skipping unknown validator in QC build");
                    continue;
                }
            };
            if pk.verify(message, sig).is_err() {
                tracing::warn!(view = self.view, idx, "skipping invalid signature in QC build");
                continue;
            }
            valid_sigs.push(sig);
            signers.set(idx as usize, true);
        }

        // Check we still have enough valid votes after filtering
        if valid_sigs.len() < quorum_size {
            return Err(ConsensusError::InsufficientVotes {
                view: self.view,
                have: valid_sigs.len(),
                need: quorum_size,
            });
        }

        let aggregate_signature = AggregateSignature::aggregate(&valid_sigs)?;

        Ok(QuorumCertificate {
            view: self.view,
            block_hash: self.block_hash,
            aggregate_signature,
            signers,
        })
    }
}

/// Collects timeout messages for a specific view and produces a TimeoutCertificate.
#[derive(Debug)]
pub struct TimeoutCollector {
    /// View this collector is gathering timeouts for.
    view: ViewNumber,
    /// Collected timeout signatures indexed by validator index.
    timeouts: HashMap<u32, (BlsSignature, QuorumCertificate)>,
    /// Total validators in the set.
    set_size: u32,
    /// Validator indices whose timeout signatures have already been verified.
    verified: HashSet<u32>,
}

impl TimeoutCollector {
    /// Creates a new timeout collector for the given view.
    pub fn new(view: ViewNumber, set_size: u32) -> Self {
        Self {
            view,
            timeouts: HashMap::new(),
            set_size,
            verified: HashSet::new(),
        }
    }

    /// Adds a timeout message. Returns `Err` if duplicate.
    pub fn add_timeout(
        &mut self,
        validator_index: u32,
        signature: BlsSignature,
        high_qc: QuorumCertificate,
    ) -> ConsensusResult<()> {
        if self.timeouts.contains_key(&validator_index) {
            return Err(ConsensusError::DuplicateVote {
                view: self.view,
                validator_index,
            });
        }
        self.timeouts.insert(validator_index, (signature, high_qc));
        Ok(())
    }

    /// Adds a timeout message that has already been signature-verified.
    pub fn add_verified_timeout(
        &mut self,
        validator_index: u32,
        signature: BlsSignature,
        high_qc: QuorumCertificate,
    ) -> ConsensusResult<()> {
        self.add_timeout(validator_index, signature, high_qc)?;
        self.verified.insert(validator_index);
        Ok(())
    }

    /// Returns the current number of collected timeouts.
    pub fn timeout_count(&self) -> usize {
        self.timeouts.len()
    }

    /// Checks if we have enough timeouts for a quorum.
    pub fn has_quorum(&self, quorum_size: usize) -> bool {
        self.timeouts.len() >= quorum_size
    }

    /// Builds a TimeoutCertificate by aggregating collected timeout signatures.
    ///
    /// Timeout messages with invalid signatures are skipped (defense-in-depth).
    /// The TC is formed from the remaining valid timeouts, failing only if fewer
    /// than `quorum_size` valid timeouts remain.
    pub fn build_tc(
        &self,
        validator_set: &ValidatorSet,
    ) -> ConsensusResult<TimeoutCertificate> {
        let quorum_size = validator_set.quorum_size();
        if self.timeouts.len() < quorum_size {
            return Err(ConsensusError::InsufficientVotes {
                view: self.view,
                have: self.timeouts.len(),
                need: quorum_size,
            });
        }

        // Build the timeout signing message: ("timeout" || view)
        let message = timeout_signing_message(self.view);

        // Find the highest QC among all timeout messages
        let mut highest_qc: Option<&QuorumCertificate> = None;
        let mut valid_sigs: Vec<&BlsSignature> = Vec::new();
        let mut signers = bitvec![u8, Msb0; 0; self.set_size as usize];

        for (&idx, (sig, high_qc)) in &self.timeouts {
            // Bounds check first
            if idx >= self.set_size {
                tracing::warn!(view = self.view, idx, "skipping out-of-range validator in TC build");
                continue;
            }

            // Skip re-verification for already-verified timeouts.
            if self.verified.contains(&idx) {
                valid_sigs.push(sig);
                signers.set(idx as usize, true);
                if highest_qc.as_ref().is_none_or(|hq| high_qc.view > hq.view) {
                    highest_qc = Some(high_qc);
                }
                continue;
            }

            let pk = match validator_set.get_public_key(idx) {
                Ok(pk) => pk,
                Err(_) => {
                    tracing::warn!(view = self.view, idx, "skipping unknown validator in TC build");
                    continue;
                }
            };
            if pk.verify(&message, sig).is_err() {
                tracing::warn!(view = self.view, idx, "skipping invalid timeout signature in TC build");
                continue;
            }
            valid_sigs.push(sig);
            signers.set(idx as usize, true);

            // Track highest QC
            if highest_qc.as_ref().is_none_or(|hq| high_qc.view > hq.view) {
                highest_qc = Some(high_qc);
            }
        }

        // Check we still have enough valid timeouts after filtering
        if valid_sigs.len() < quorum_size {
            return Err(ConsensusError::InsufficientVotes {
                view: self.view,
                have: valid_sigs.len(),
                need: quorum_size,
            });
        }

        let aggregate_signature = AggregateSignature::aggregate(&valid_sigs)?;
        let high_qc = highest_qc
            .cloned()
            .ok_or_else(|| ConsensusError::InvalidTC {
                view: self.view,
                reason: "no high_qc found in timeout messages".to_string(),
            })?;

        Ok(TimeoutCertificate {
            view: self.view,
            aggregate_signature,
            signers,
            high_qc,
        })
    }
}

/// Verifies a QuorumCertificate against the validator set.
pub fn verify_qc(
    qc: &QuorumCertificate,
    validator_set: &ValidatorSet,
) -> ConsensusResult<()> {
    // Check that enough signers participated
    let signer_count = qc.signer_count();
    let quorum_size = validator_set.quorum_size();
    if signer_count < quorum_size {
        return Err(ConsensusError::InvalidQC {
            view: qc.view,
            reason: format!(
                "insufficient signers: have {signer_count}, need {quorum_size}"
            ),
        });
    }

    // Collect public keys of signers
    let signer_pks: Vec<&BlsPublicKey> = qc
        .signers
        .iter()
        .enumerate()
        .filter(|(_, bit)| *bit == true)
        .map(|(idx, _)| validator_set.get_public_key(idx as u32))
        .collect::<ConsensusResult<Vec<_>>>()?;

    // Verify the aggregated signature
    let message = signing_message(qc.view, &qc.block_hash);
    AggregateSignature::verify_aggregate(&message, &qc.aggregate_signature, &signer_pks)
        .map_err(|_| ConsensusError::InvalidQC {
            view: qc.view,
            reason: "aggregated signature verification failed".to_string(),
        })
}

/// Constructs the signing message for votes: view (8 bytes LE) || block_hash (32 bytes).
pub fn signing_message(view: ViewNumber, block_hash: &B256) -> Vec<u8> {
    let mut msg = Vec::with_capacity(40);
    msg.extend_from_slice(&view.to_le_bytes());
    msg.extend_from_slice(block_hash.as_slice());
    msg
}

/// Constructs the signing message for commit votes: "commit" || view || block_hash.
pub fn commit_signing_message(view: ViewNumber, block_hash: &B256) -> Vec<u8> {
    let mut msg = Vec::with_capacity(46);
    msg.extend_from_slice(b"commit");
    msg.extend_from_slice(&view.to_le_bytes());
    msg.extend_from_slice(block_hash.as_slice());
    msg
}

/// Verifies a TimeoutCertificate against the validator set.
///
/// Checks that:
/// 1. Enough signers participated (quorum)
/// 2. The aggregated signature is valid over `timeout_signing_message(tc.view)`
pub fn verify_tc(
    tc: &TimeoutCertificate,
    validator_set: &ValidatorSet,
) -> ConsensusResult<()> {
    // Check that enough signers participated
    let signer_count = tc.signers.iter().filter(|b| **b).count();
    let quorum_size = validator_set.quorum_size();
    if signer_count < quorum_size {
        return Err(ConsensusError::InvalidTC {
            view: tc.view,
            reason: format!(
                "insufficient signers: have {signer_count}, need {quorum_size}"
            ),
        });
    }

    // Collect public keys of signers
    let signer_pks: Vec<&BlsPublicKey> = tc
        .signers
        .iter()
        .enumerate()
        .filter(|(_, bit)| *bit == true)
        .map(|(idx, _)| validator_set.get_public_key(idx as u32))
        .collect::<ConsensusResult<Vec<_>>>()?;

    // Verify the aggregated signature over timeout message
    let message = timeout_signing_message(tc.view);
    AggregateSignature::verify_aggregate(&message, &tc.aggregate_signature, &signer_pks)
        .map_err(|_| ConsensusError::InvalidTC {
            view: tc.view,
            reason: "aggregated signature verification failed".to_string(),
        })
}

/// Constructs the signing message for timeout: "timeout" || view (8 bytes LE).
pub fn timeout_signing_message(view: ViewNumber) -> Vec<u8> {
    let mut msg = Vec::with_capacity(15);
    msg.extend_from_slice(b"timeout");
    msg.extend_from_slice(&view.to_le_bytes());
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use n42_chainspec::ValidatorInfo;
    use n42_primitives::BlsSecretKey;

    /// Helper: create a test validator set of size `n` along with the secret keys.
    fn test_validator_set(n: usize) -> (Vec<BlsSecretKey>, ValidatorSet) {
        let sks: Vec<_> = (0..n).map(|_| BlsSecretKey::random().unwrap()).collect();
        let infos: Vec<_> = sks
            .iter()
            .enumerate()
            .map(|(i, sk)| ValidatorInfo {
                address: Address::with_last_byte(i as u8),
                bls_public_key: sk.public_key(),
            })
            .collect();
        let f = ((n as u32).saturating_sub(1)) / 3;
        let vs = ValidatorSet::new(&infos, f);
        (sks, vs)
    }

    #[test]
    fn test_vote_collector_basic() {
        let (sks, vs) = test_validator_set(4);
        let view = 1u64;
        let block_hash = B256::repeat_byte(0xBB);
        let mut collector = VoteCollector::new(view, block_hash, vs.len());

        assert_eq!(collector.vote_count(), 0, "initially no votes");
        assert!(!collector.has_quorum(vs.quorum_size()), "no quorum with 0 votes");

        // Add 3 votes (quorum = 2*1+1 = 3 for n=4)
        let msg = signing_message(view, &block_hash);
        for i in 0..3u32 {
            let sig = sks[i as usize].sign(&msg);
            collector.add_vote(i, sig).expect("adding vote should succeed");
        }

        assert_eq!(collector.vote_count(), 3, "should have 3 votes");
        assert!(collector.has_quorum(vs.quorum_size()), "should have quorum with 3 votes");
    }

    #[test]
    fn test_vote_collector_duplicate() {
        let (sks, vs) = test_validator_set(4);
        let view = 1u64;
        let block_hash = B256::repeat_byte(0xCC);
        let mut collector = VoteCollector::new(view, block_hash, vs.len());

        let msg = signing_message(view, &block_hash);
        let sig = sks[0].sign(&msg);

        // First vote succeeds
        collector.add_vote(0, sig.clone()).expect("first vote should succeed");

        // Duplicate vote should fail
        let sig2 = sks[0].sign(&msg);
        let result = collector.add_vote(0, sig2);
        assert!(result.is_err(), "duplicate vote should return error");

        // Verify it's specifically a DuplicateVote error
        match result.unwrap_err() {
            ConsensusError::DuplicateVote {
                view: v,
                validator_index: idx,
            } => {
                assert_eq!(v, view);
                assert_eq!(idx, 0);
            }
            other => panic!("expected DuplicateVote error, got: {:?}", other),
        }
    }

    #[test]
    fn test_build_qc_and_verify() {
        let (sks, vs) = test_validator_set(4);
        let view = 5u64;
        let block_hash = B256::repeat_byte(0xDD);
        let mut collector = VoteCollector::new(view, block_hash, vs.len());

        // Sign with 3 validators (indices 0, 1, 2) to meet quorum of 3
        let msg = signing_message(view, &block_hash);
        for i in 0..3u32 {
            let sig = sks[i as usize].sign(&msg);
            collector.add_vote(i, sig).unwrap();
        }

        // Build QC should succeed
        let qc = collector.build_qc(&vs).expect("build_qc should succeed");
        assert_eq!(qc.view, view, "QC view should match");
        assert_eq!(qc.block_hash, block_hash, "QC block_hash should match");
        assert_eq!(qc.signer_count(), 3, "QC should have 3 signers");

        // Verify signers bitmap: bits 0, 1, 2 set; bit 3 unset
        assert!(qc.signers[0], "signer 0 should be set");
        assert!(qc.signers[1], "signer 1 should be set");
        assert!(qc.signers[2], "signer 2 should be set");
        assert!(!qc.signers[3], "signer 3 should not be set");

        // verify_qc should succeed
        verify_qc(&qc, &vs).expect("verify_qc should succeed for valid QC");
    }

    #[test]
    fn test_verify_qc_insufficient_signers() {
        let (sks, vs) = test_validator_set(4);
        let view = 10u64;
        let block_hash = B256::repeat_byte(0xEE);

        // Build a QC with only 1 signer (quorum requires 3)
        let msg = signing_message(view, &block_hash);
        let sig = sks[0].sign(&msg);
        let agg_sig =
            n42_primitives::bls::AggregateSignature::aggregate(&[&sig]).unwrap();

        let qc = QuorumCertificate {
            view,
            block_hash,
            aggregate_signature: agg_sig,
            signers: {
                let mut bv = bitvec![u8, Msb0; 0; 4];
                bv.set(0, true);
                bv
            },
        };

        let result = verify_qc(&qc, &vs);
        assert!(result.is_err(), "verify_qc should fail with insufficient signers");

        match result.unwrap_err() {
            ConsensusError::InvalidQC { view: v, reason } => {
                assert_eq!(v, view);
                assert!(
                    reason.contains("insufficient signers"),
                    "error reason should mention insufficient signers, got: {}",
                    reason
                );
            }
            other => panic!("expected InvalidQC error, got: {:?}", other),
        }
    }

    #[test]
    fn test_signing_message() {
        let view: ViewNumber = 42;
        let block_hash = B256::repeat_byte(0xFF);

        let msg = signing_message(view, &block_hash);

        // Should be 8 bytes (view LE) + 32 bytes (block hash) = 40 bytes
        assert_eq!(msg.len(), 40, "signing message should be 40 bytes");

        // First 8 bytes: view 42 in little-endian
        assert_eq!(&msg[..8], &42u64.to_le_bytes(), "first 8 bytes should be view in LE");

        // Last 32 bytes: block hash
        assert_eq!(
            &msg[8..],
            block_hash.as_slice(),
            "last 32 bytes should be block hash"
        );
    }

    #[test]
    fn test_timeout_signing_message() {
        let view: ViewNumber = 99;
        let msg = timeout_signing_message(view);

        // Should be 7 bytes ("timeout") + 8 bytes (view LE) = 15 bytes
        assert_eq!(msg.len(), 15, "timeout signing message should be 15 bytes");

        // First 7 bytes: "timeout"
        assert_eq!(&msg[..7], b"timeout", "should start with 'timeout'");

        // Last 8 bytes: view in LE
        assert_eq!(
            &msg[7..],
            &99u64.to_le_bytes(),
            "last 8 bytes should be view in LE"
        );
    }

    #[test]
    fn test_commit_signing_message_format() {
        let view: ViewNumber = 7;
        let block_hash = B256::repeat_byte(0x11);
        let msg = commit_signing_message(view, &block_hash);

        // Should be 6 bytes ("commit") + 8 bytes (view LE) + 32 bytes (block hash) = 46 bytes
        assert_eq!(msg.len(), 46, "commit signing message should be 46 bytes");
        assert_eq!(&msg[..6], b"commit", "should start with 'commit'");
        assert_eq!(&msg[6..14], &7u64.to_le_bytes(), "view bytes should match");
        assert_eq!(&msg[14..], block_hash.as_slice(), "block hash should match");
    }

    #[test]
    fn test_build_qc_insufficient_votes() {
        let (sks, vs) = test_validator_set(4);
        let view = 1u64;
        let block_hash = B256::repeat_byte(0xAA);
        let mut collector = VoteCollector::new(view, block_hash, vs.len());

        // Add only 1 vote (quorum is 3)
        let msg = signing_message(view, &block_hash);
        let sig = sks[0].sign(&msg);
        collector.add_vote(0, sig).unwrap();

        let result = collector.build_qc(&vs);
        assert!(result.is_err(), "build_qc should fail with insufficient votes");

        match result.unwrap_err() {
            ConsensusError::InsufficientVotes { have, need, .. } => {
                assert_eq!(have, 1);
                assert_eq!(need, 3);
            }
            other => panic!("expected InsufficientVotes, got: {:?}", other),
        }
    }

    // ── Level 1: Defense-in-depth tests for invalid signature skipping ──

    /// build_qc should skip votes with invalid signatures and still form a QC
    /// if enough valid votes remain.
    #[test]
    fn test_build_qc_skips_invalid_signature() {
        let (sks, vs) = test_validator_set(4);
        let view = 3u64;
        let block_hash = B256::repeat_byte(0xF1);
        let mut collector = VoteCollector::new(view, block_hash, vs.len());

        // 3 valid votes from validators 0, 1, 2
        let msg = signing_message(view, &block_hash);
        for i in 0..3u32 {
            let sig = sks[i as usize].sign(&msg);
            collector.add_vote(i, sig).unwrap();
        }

        // 1 invalid vote from validator 3 (signed with wrong message)
        let wrong_msg = signing_message(view, &B256::repeat_byte(0xFF));
        let bad_sig = sks[3].sign(&wrong_msg);
        collector.add_vote(3, bad_sig).unwrap();

        // build_qc should succeed with 3 valid signatures, skipping the invalid one
        let qc = collector.build_qc(&vs).expect("QC should form from valid votes");
        assert_eq!(qc.signer_count(), 3, "QC should have 3 signers (invalid skipped)");
        assert!(qc.signers[0]);
        assert!(qc.signers[1]);
        assert!(qc.signers[2]);
        assert!(!qc.signers[3], "invalid signer should be excluded");

        // QC should verify successfully
        verify_qc(&qc, &vs).expect("QC should verify");
    }

    /// build_qc should fail if too many invalid signatures reduce valid count below quorum.
    #[test]
    fn test_build_qc_fails_with_too_many_invalid() {
        let (sks, vs) = test_validator_set(4);
        let view = 4u64;
        let block_hash = B256::repeat_byte(0xF2);
        let mut collector = VoteCollector::new(view, block_hash, vs.len());

        // 2 valid votes (quorum is 3)
        let msg = signing_message(view, &block_hash);
        for i in 0..2u32 {
            let sig = sks[i as usize].sign(&msg);
            collector.add_vote(i, sig).unwrap();
        }

        // 2 invalid votes
        let wrong_msg = signing_message(99, &block_hash);
        for i in 2..4u32 {
            let bad_sig = sks[i as usize].sign(&wrong_msg);
            collector.add_vote(i, bad_sig).unwrap();
        }

        // 4 raw votes but only 2 valid → below quorum
        let result = collector.build_qc(&vs);
        assert!(result.is_err(), "should fail with insufficient valid votes");
        match result.unwrap_err() {
            ConsensusError::InsufficientVotes { have, need, .. } => {
                assert_eq!(have, 2);
                assert_eq!(need, 3);
            }
            other => panic!("expected InsufficientVotes, got: {:?}", other),
        }
    }

    /// build_tc should skip timeout messages with invalid signatures.
    #[test]
    fn test_build_tc_skips_invalid_signature() {
        let (sks, vs) = test_validator_set(4);
        let view = 5u64;
        let genesis_qc = QuorumCertificate::genesis();

        let mut collector = TimeoutCollector::new(view, vs.len());
        let msg = timeout_signing_message(view);

        // 3 valid timeouts
        for i in 0..3u32 {
            let sig = sks[i as usize].sign(&msg);
            collector.add_timeout(i, sig, genesis_qc.clone()).unwrap();
        }

        // 1 invalid timeout (wrong message)
        let wrong_msg = timeout_signing_message(999);
        let bad_sig = sks[3].sign(&wrong_msg);
        collector.add_timeout(3, bad_sig, genesis_qc.clone()).unwrap();

        // TC should form with 3 valid, skipping the invalid one
        let tc = collector.build_tc(&vs).expect("TC should form from valid timeouts");
        assert_eq!(tc.signers.iter().filter(|b| **b).count(), 3);
        assert!(tc.signers[0]);
        assert!(tc.signers[1]);
        assert!(tc.signers[2]);
        assert!(!tc.signers[3], "invalid signer should be excluded");
    }

    /// build_tc should fail if too many invalid signatures reduce count below quorum.
    #[test]
    fn test_build_tc_fails_with_too_many_invalid() {
        let (sks, vs) = test_validator_set(4);
        let view = 6u64;
        let genesis_qc = QuorumCertificate::genesis();

        let mut collector = TimeoutCollector::new(view, vs.len());
        let msg = timeout_signing_message(view);

        // 2 valid timeouts
        for i in 0..2u32 {
            let sig = sks[i as usize].sign(&msg);
            collector.add_timeout(i, sig, genesis_qc.clone()).unwrap();
        }

        // 2 invalid timeouts
        let wrong_msg = timeout_signing_message(999);
        for i in 2..4u32 {
            let bad_sig = sks[i as usize].sign(&wrong_msg);
            collector.add_timeout(i, bad_sig, genesis_qc.clone()).unwrap();
        }

        let result = collector.build_tc(&vs);
        assert!(result.is_err());
        match result.unwrap_err() {
            ConsensusError::InsufficientVotes { have, need, .. } => {
                assert_eq!(have, 2);
                assert_eq!(need, 3);
            }
            other => panic!("expected InsufficientVotes, got: {:?}", other),
        }
    }

    /// build_tc should track the highest QC among valid timeouts only.
    #[test]
    fn test_build_tc_highest_qc_from_valid_only() {
        let (sks, vs) = test_validator_set(4);
        let view = 7u64;
        let genesis_qc = QuorumCertificate::genesis(); // view 0

        // Build a "higher" QC (view 5) for the invalid timeout
        let higher_qc = QuorumCertificate {
            view: 5,
            block_hash: B256::repeat_byte(0xAB),
            aggregate_signature: genesis_qc.aggregate_signature.clone(),
            signers: genesis_qc.signers.clone(),
        };

        let mut collector = TimeoutCollector::new(view, vs.len());
        let msg = timeout_signing_message(view);

        // 3 valid timeouts with genesis QC (view 0)
        for i in 0..3u32 {
            let sig = sks[i as usize].sign(&msg);
            collector.add_timeout(i, sig, genesis_qc.clone()).unwrap();
        }

        // 1 invalid timeout with higher QC (view 5) — should be skipped
        let wrong_msg = timeout_signing_message(999);
        let bad_sig = sks[3].sign(&wrong_msg);
        collector.add_timeout(3, bad_sig, higher_qc).unwrap();

        let tc = collector.build_tc(&vs).expect("TC should form");
        // high_qc should be genesis (view 0), not the invalid timeout's view 5
        assert_eq!(tc.high_qc.view, 0, "high_qc should come from valid timeouts only");
    }
}
