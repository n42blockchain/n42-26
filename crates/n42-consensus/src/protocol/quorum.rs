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
    view: ViewNumber,
    block_hash: B256,
    /// Collected signatures indexed by validator index.
    votes: HashMap<u32, BlsSignature>,
    set_size: u32,
    /// Validators whose signatures were already verified by the caller.
    /// Skipped during `build_qc_with_message` to avoid redundant BLS pairings.
    verified: HashSet<u32>,
}

impl VoteCollector {
    pub fn new(view: ViewNumber, block_hash: B256, set_size: u32) -> Self {
        Self {
            view,
            block_hash,
            votes: HashMap::new(),
            set_size,
            verified: HashSet::new(),
        }
    }

    /// Adds a vote. Returns `Err` if the validator has already voted.
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
    /// Marks the validator in `verified` to skip re-verification in `build_qc_with_message`.
    pub fn add_verified_vote(
        &mut self,
        validator_index: u32,
        signature: BlsSignature,
    ) -> ConsensusResult<()> {
        self.add_vote(validator_index, signature)?;
        self.verified.insert(validator_index);
        Ok(())
    }

    pub fn block_hash(&self) -> B256 {
        self.block_hash
    }

    pub fn vote_count(&self) -> usize {
        self.votes.len()
    }

    pub fn has_quorum(&self, quorum_size: usize) -> bool {
        self.votes.len() >= quorum_size
    }

    /// Builds a QuorumCertificate using the standard vote signing message.
    pub fn build_qc(&self, validator_set: &ValidatorSet) -> ConsensusResult<QuorumCertificate> {
        let message = signing_message(self.view, &self.block_hash);
        self.build_qc_with_message(validator_set, &message)
    }

    /// Builds a QuorumCertificate using a custom signing message.
    ///
    /// Used for CommitVote (Round 2) which uses `commit_signing_message` instead of the
    /// standard `signing_message`. Votes with invalid signatures are skipped (defense-in-depth);
    /// the QC fails only if fewer than `quorum_size` valid votes remain.
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

        let mut valid_sigs: Vec<&BlsSignature> = Vec::new();
        let mut signers = bitvec![u8, Msb0; 0; self.set_size as usize];

        for (&idx, sig) in &self.votes {
            if idx >= self.set_size {
                tracing::warn!(view = self.view, idx, "skipping out-of-range validator in QC build");
                continue;
            }

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
    view: ViewNumber,
    timeouts: HashMap<u32, (BlsSignature, QuorumCertificate)>,
    set_size: u32,
    verified: HashSet<u32>,
}

impl TimeoutCollector {
    pub fn new(view: ViewNumber, set_size: u32) -> Self {
        Self {
            view,
            timeouts: HashMap::new(),
            set_size,
            verified: HashSet::new(),
        }
    }

    pub fn view(&self) -> ViewNumber {
        self.view
    }

    /// Adds a timeout message. Returns `Err` if the validator has already submitted one.
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

    /// Adds a timeout message that has already been signature-verified by the caller.
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

    pub fn timeout_count(&self) -> usize {
        self.timeouts.len()
    }

    pub fn has_quorum(&self, quorum_size: usize) -> bool {
        self.timeouts.len() >= quorum_size
    }

    /// Builds a TimeoutCertificate from collected timeout signatures.
    ///
    /// Timeout messages with invalid signatures are skipped (defense-in-depth).
    /// The TC fails only if fewer than `quorum_size` valid timeouts remain.
    pub fn build_tc(&self, validator_set: &ValidatorSet) -> ConsensusResult<TimeoutCertificate> {
        let quorum_size = validator_set.quorum_size();
        if self.timeouts.len() < quorum_size {
            return Err(ConsensusError::InsufficientVotes {
                view: self.view,
                have: self.timeouts.len(),
                need: quorum_size,
            });
        }

        let message = timeout_signing_message(self.view);
        let mut highest_qc: Option<&QuorumCertificate> = None;
        let mut valid_sigs: Vec<&BlsSignature> = Vec::new();
        let mut signers = bitvec![u8, Msb0; 0; self.set_size as usize];

        for (&idx, (sig, high_qc)) in &self.timeouts {
            if idx >= self.set_size {
                tracing::warn!(view = self.view, idx, "skipping out-of-range validator in TC build");
                continue;
            }

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
            if highest_qc.as_ref().is_none_or(|hq| high_qc.view > hq.view) {
                highest_qc = Some(high_qc);
            }
        }

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

/// Collects and validates signer public keys from a signers bitmap.
///
/// Validates bitmap length against the validator set (prevents short bitmaps from
/// bypassing quorum checks), then collects the public keys of all set bits.
fn collect_signer_keys<'a>(
    signers: &BitVec<u8, Msb0>,
    validator_set: &'a ValidatorSet,
    quorum_size: usize,
    view: ViewNumber,
    cert_kind: &str,
) -> ConsensusResult<Vec<&'a BlsPublicKey>> {
    let expected_len = validator_set.len() as usize;
    if signers.len() != expected_len {
        let reason = format!(
            "signers bitmap length mismatch: got {}, expected {}",
            signers.len(),
            expected_len,
        );
        return if cert_kind == "TC" {
            Err(ConsensusError::InvalidTC { view, reason })
        } else {
            Err(ConsensusError::InvalidQC { view, reason })
        };
    }

    let signer_count = signers.iter().filter(|b| **b).count();
    if signer_count < quorum_size {
        let reason = format!("insufficient signers: have {signer_count}, need {quorum_size}");
        return if cert_kind == "TC" {
            Err(ConsensusError::InvalidTC { view, reason })
        } else {
            Err(ConsensusError::InvalidQC { view, reason })
        };
    }

    signers
        .iter()
        .enumerate()
        .filter(|(_, bit)| **bit)
        .map(|(idx, _)| validator_set.get_public_key(idx as u32))
        .collect::<ConsensusResult<Vec<_>>>()
}

/// Verifies a QuorumCertificate (Round 1) against the validator set.
pub fn verify_qc(qc: &QuorumCertificate, validator_set: &ValidatorSet) -> ConsensusResult<()> {
    let quorum_size = validator_set.quorum_size();
    let signer_pks = collect_signer_keys(&qc.signers, validator_set, quorum_size, qc.view, "QC")?;
    let message = signing_message(qc.view, &qc.block_hash);
    AggregateSignature::verify_aggregate(&message, &qc.aggregate_signature, &signer_pks)
        .map_err(|_| ConsensusError::InvalidQC {
            view: qc.view,
            reason: "aggregated signature verification failed".to_string(),
        })
}

/// Verifies a CommitQC (Round 2) against the validator set.
///
/// Uses `commit_signing_message` format ("commit" || view || block_hash)
/// rather than the standard `signing_message` format.
pub fn verify_commit_qc(
    qc: &QuorumCertificate,
    validator_set: &ValidatorSet,
) -> ConsensusResult<()> {
    let quorum_size = validator_set.quorum_size();
    let signer_pks = collect_signer_keys(&qc.signers, validator_set, quorum_size, qc.view, "QC")?;
    let message = commit_signing_message(qc.view, &qc.block_hash);
    AggregateSignature::verify_aggregate(&message, &qc.aggregate_signature, &signer_pks)
        .map_err(|_| ConsensusError::InvalidQC {
            view: qc.view,
            reason: "commit QC aggregated signature verification failed".to_string(),
        })
}

/// Verifies a TimeoutCertificate against the validator set.
pub fn verify_tc(tc: &TimeoutCertificate, validator_set: &ValidatorSet) -> ConsensusResult<()> {
    let quorum_size = validator_set.quorum_size();
    let signer_pks = collect_signer_keys(&tc.signers, validator_set, quorum_size, tc.view, "TC")?;
    let message = timeout_signing_message(tc.view);
    AggregateSignature::verify_aggregate(&message, &tc.aggregate_signature, &signer_pks)
        .map_err(|_| ConsensusError::InvalidTC {
            view: tc.view,
            reason: "aggregated signature verification failed".to_string(),
        })
}

/// Signing message for Round 1 votes: view (8 bytes LE) || block_hash (32 bytes).
pub fn signing_message(view: ViewNumber, block_hash: &B256) -> Vec<u8> {
    let mut msg = Vec::with_capacity(40);
    msg.extend_from_slice(&view.to_le_bytes());
    msg.extend_from_slice(block_hash.as_slice());
    msg
}

/// Signing message for Round 2 commit votes: "commit" || view (8 bytes LE) || block_hash (32 bytes).
pub fn commit_signing_message(view: ViewNumber, block_hash: &B256) -> Vec<u8> {
    let mut msg = Vec::with_capacity(46);
    msg.extend_from_slice(b"commit");
    msg.extend_from_slice(&view.to_le_bytes());
    msg.extend_from_slice(block_hash.as_slice());
    msg
}

/// Signing message for timeout messages: "timeout" || view (8 bytes LE).
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

        assert_eq!(collector.vote_count(), 0);
        assert!(!collector.has_quorum(vs.quorum_size()));

        let msg = signing_message(view, &block_hash);
        for i in 0..3u32 {
            let sig = sks[i as usize].sign(&msg);
            collector.add_vote(i, sig).expect("adding vote should succeed");
        }

        assert_eq!(collector.vote_count(), 3);
        assert!(collector.has_quorum(vs.quorum_size()));
    }

    #[test]
    fn test_vote_collector_duplicate() {
        let (sks, vs) = test_validator_set(4);
        let view = 1u64;
        let block_hash = B256::repeat_byte(0xCC);
        let mut collector = VoteCollector::new(view, block_hash, vs.len());

        let msg = signing_message(view, &block_hash);
        let sig = sks[0].sign(&msg);

        collector.add_vote(0, sig.clone()).expect("first vote should succeed");

        let result = collector.add_vote(0, sks[0].sign(&msg));
        assert!(result.is_err());
        match result.unwrap_err() {
            ConsensusError::DuplicateVote { view: v, validator_index: idx } => {
                assert_eq!(v, view);
                assert_eq!(idx, 0);
            }
            other => panic!("expected DuplicateVote, got: {:?}", other),
        }
    }

    #[test]
    fn test_build_qc_and_verify() {
        let (sks, vs) = test_validator_set(4);
        let view = 5u64;
        let block_hash = B256::repeat_byte(0xDD);
        let mut collector = VoteCollector::new(view, block_hash, vs.len());

        let msg = signing_message(view, &block_hash);
        for i in 0..3u32 {
            let sig = sks[i as usize].sign(&msg);
            collector.add_vote(i, sig).unwrap();
        }

        let qc = collector.build_qc(&vs).expect("build_qc should succeed");
        assert_eq!(qc.view, view);
        assert_eq!(qc.block_hash, block_hash);
        assert_eq!(qc.signer_count(), 3);
        assert!(qc.signers[0]);
        assert!(qc.signers[1]);
        assert!(qc.signers[2]);
        assert!(!qc.signers[3]);

        verify_qc(&qc, &vs).expect("verify_qc should succeed");
    }

    #[test]
    fn test_verify_qc_insufficient_signers() {
        let (sks, vs) = test_validator_set(4);
        let view = 10u64;
        let block_hash = B256::repeat_byte(0xEE);

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
        assert!(result.is_err());
        match result.unwrap_err() {
            ConsensusError::InvalidQC { view: v, reason } => {
                assert_eq!(v, view);
                assert!(reason.contains("insufficient signers"), "got: {reason}");
            }
            other => panic!("expected InvalidQC, got: {:?}", other),
        }
    }

    #[test]
    fn test_signing_message() {
        let view: ViewNumber = 42;
        let block_hash = B256::repeat_byte(0xFF);
        let msg = signing_message(view, &block_hash);

        assert_eq!(msg.len(), 40);
        assert_eq!(&msg[..8], &42u64.to_le_bytes());
        assert_eq!(&msg[8..], block_hash.as_slice());
    }

    #[test]
    fn test_timeout_signing_message() {
        let view: ViewNumber = 99;
        let msg = timeout_signing_message(view);

        assert_eq!(msg.len(), 15);
        assert_eq!(&msg[..7], b"timeout");
        assert_eq!(&msg[7..], &99u64.to_le_bytes());
    }

    #[test]
    fn test_commit_signing_message_format() {
        let view: ViewNumber = 7;
        let block_hash = B256::repeat_byte(0x11);
        let msg = commit_signing_message(view, &block_hash);

        assert_eq!(msg.len(), 46);
        assert_eq!(&msg[..6], b"commit");
        assert_eq!(&msg[6..14], &7u64.to_le_bytes());
        assert_eq!(&msg[14..], block_hash.as_slice());
    }

    #[test]
    fn test_build_qc_insufficient_votes() {
        let (sks, vs) = test_validator_set(4);
        let view = 1u64;
        let block_hash = B256::repeat_byte(0xAA);
        let mut collector = VoteCollector::new(view, block_hash, vs.len());

        let msg = signing_message(view, &block_hash);
        collector.add_vote(0, sks[0].sign(&msg)).unwrap();

        let result = collector.build_qc(&vs);
        assert!(result.is_err());
        match result.unwrap_err() {
            ConsensusError::InsufficientVotes { have, need, .. } => {
                assert_eq!(have, 1);
                assert_eq!(need, 3);
            }
            other => panic!("expected InsufficientVotes, got: {:?}", other),
        }
    }

    #[test]
    fn test_build_qc_skips_invalid_signature() {
        let (sks, vs) = test_validator_set(4);
        let view = 3u64;
        let block_hash = B256::repeat_byte(0xF1);
        let mut collector = VoteCollector::new(view, block_hash, vs.len());

        let msg = signing_message(view, &block_hash);
        for i in 0..3u32 {
            let sig = sks[i as usize].sign(&msg);
            collector.add_vote(i, sig).unwrap();
        }

        // Invalid vote: signed with wrong block hash
        let wrong_msg = signing_message(view, &B256::repeat_byte(0xFF));
        collector.add_vote(3, sks[3].sign(&wrong_msg)).unwrap();

        let qc = collector.build_qc(&vs).expect("QC should form from valid votes");
        assert_eq!(qc.signer_count(), 3);
        assert!(qc.signers[0]);
        assert!(qc.signers[1]);
        assert!(qc.signers[2]);
        assert!(!qc.signers[3]);
        verify_qc(&qc, &vs).expect("QC should verify");
    }

    #[test]
    fn test_build_qc_fails_with_too_many_invalid() {
        let (sks, vs) = test_validator_set(4);
        let view = 4u64;
        let block_hash = B256::repeat_byte(0xF2);
        let mut collector = VoteCollector::new(view, block_hash, vs.len());

        let msg = signing_message(view, &block_hash);
        for i in 0..2u32 {
            collector.add_vote(i, sks[i as usize].sign(&msg)).unwrap();
        }

        let wrong_msg = signing_message(99, &block_hash);
        for i in 2..4u32 {
            collector.add_vote(i, sks[i as usize].sign(&wrong_msg)).unwrap();
        }

        let result = collector.build_qc(&vs);
        assert!(result.is_err());
        match result.unwrap_err() {
            ConsensusError::InsufficientVotes { have, need, .. } => {
                assert_eq!(have, 2);
                assert_eq!(need, 3);
            }
            other => panic!("expected InsufficientVotes, got: {:?}", other),
        }
    }

    #[test]
    fn test_build_tc_skips_invalid_signature() {
        let (sks, vs) = test_validator_set(4);
        let view = 5u64;
        let genesis_qc = QuorumCertificate::genesis();
        let mut collector = TimeoutCollector::new(view, vs.len());
        let msg = timeout_signing_message(view);

        for i in 0..3u32 {
            collector.add_timeout(i, sks[i as usize].sign(&msg), genesis_qc.clone()).unwrap();
        }

        // Invalid timeout: wrong view
        let wrong_msg = timeout_signing_message(999);
        collector.add_timeout(3, sks[3].sign(&wrong_msg), genesis_qc.clone()).unwrap();

        let tc = collector.build_tc(&vs).expect("TC should form from valid timeouts");
        assert_eq!(tc.signers.iter().filter(|b| **b).count(), 3);
        assert!(tc.signers[0]);
        assert!(tc.signers[1]);
        assert!(tc.signers[2]);
        assert!(!tc.signers[3]);
    }

    #[test]
    fn test_build_tc_fails_with_too_many_invalid() {
        let (sks, vs) = test_validator_set(4);
        let view = 6u64;
        let genesis_qc = QuorumCertificate::genesis();
        let mut collector = TimeoutCollector::new(view, vs.len());
        let msg = timeout_signing_message(view);

        for i in 0..2u32 {
            collector.add_timeout(i, sks[i as usize].sign(&msg), genesis_qc.clone()).unwrap();
        }

        let wrong_msg = timeout_signing_message(999);
        for i in 2..4u32 {
            collector.add_timeout(i, sks[i as usize].sign(&wrong_msg), genesis_qc.clone()).unwrap();
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

    #[test]
    fn test_build_tc_highest_qc_from_valid_only() {
        let (sks, vs) = test_validator_set(4);
        let view = 7u64;
        let genesis_qc = QuorumCertificate::genesis();

        // Higher QC that will be attached to the invalid timeout
        let higher_qc = QuorumCertificate {
            view: 5,
            block_hash: B256::repeat_byte(0xAB),
            aggregate_signature: genesis_qc.aggregate_signature.clone(),
            signers: genesis_qc.signers.clone(),
        };

        let mut collector = TimeoutCollector::new(view, vs.len());
        let msg = timeout_signing_message(view);

        for i in 0..3u32 {
            collector.add_timeout(i, sks[i as usize].sign(&msg), genesis_qc.clone()).unwrap();
        }

        let wrong_msg = timeout_signing_message(999);
        collector.add_timeout(3, sks[3].sign(&wrong_msg), higher_qc).unwrap();

        let tc = collector.build_tc(&vs).expect("TC should form");
        // high_qc should be genesis (view 0), not the invalid timeout's view 5
        assert_eq!(tc.high_qc.view, 0);
    }

    #[test]
    fn test_verify_commit_qc_valid() {
        let (sks, vs) = test_validator_set(4);
        let view = 10u64;
        let block_hash = B256::repeat_byte(0xC1);

        let mut collector = VoteCollector::new(view, block_hash, vs.len());
        let msg = commit_signing_message(view, &block_hash);
        for i in 0..3u32 {
            collector.add_vote(i, sks[i as usize].sign(&msg)).unwrap();
        }
        let commit_qc = collector.build_qc_with_message(&vs, &msg).unwrap();

        verify_commit_qc(&commit_qc, &vs).expect("verify_commit_qc should succeed");
    }

    #[test]
    fn test_verify_commit_qc_insufficient_signers() {
        let (sks, vs) = test_validator_set(4);
        let view = 11u64;
        let block_hash = B256::repeat_byte(0xC2);

        let msg = commit_signing_message(view, &block_hash);
        let sig = sks[0].sign(&msg);
        let agg_sig = AggregateSignature::aggregate(&[&sig]).unwrap();

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

        let result = verify_commit_qc(&qc, &vs);
        assert!(result.is_err());
        match result.unwrap_err() {
            ConsensusError::InvalidQC { view: v, reason } => {
                assert_eq!(v, view);
                assert!(reason.contains("insufficient signers"), "got: {reason}");
            }
            other => panic!("expected InvalidQC, got: {:?}", other),
        }
    }

    #[test]
    fn test_verify_qc_rejects_commit_qc() {
        let (sks, vs) = test_validator_set(4);
        let view = 12u64;
        let block_hash = B256::repeat_byte(0xC3);

        let mut collector = VoteCollector::new(view, block_hash, vs.len());
        let msg = commit_signing_message(view, &block_hash);
        for i in 0..3u32 {
            collector.add_vote(i, sks[i as usize].sign(&msg)).unwrap();
        }
        let commit_qc = collector.build_qc_with_message(&vs, &msg).unwrap();

        let result = verify_qc(&commit_qc, &vs);
        assert!(result.is_err(), "verify_qc should reject a CommitQC");
    }

    #[test]
    fn test_verify_commit_qc_rejects_prepare_qc() {
        let (sks, vs) = test_validator_set(4);
        let view = 13u64;
        let block_hash = B256::repeat_byte(0xC4);

        let mut collector = VoteCollector::new(view, block_hash, vs.len());
        let msg = signing_message(view, &block_hash);
        for i in 0..3u32 {
            collector.add_vote(i, sks[i as usize].sign(&msg)).unwrap();
        }
        let prepare_qc = collector.build_qc(&vs).unwrap();

        let result = verify_commit_qc(&prepare_qc, &vs);
        assert!(result.is_err(), "verify_commit_qc should reject a PrepareQC");
    }

    #[test]
    fn test_timeout_collector_duplicate_rejected() {
        let (sks, _vs) = test_validator_set(4);
        let view = 10u64;
        let genesis_qc = QuorumCertificate::genesis();
        let mut collector = TimeoutCollector::new(view, 4);
        let msg = timeout_signing_message(view);
        let sig = sks[0].sign(&msg);

        collector
            .add_timeout(0, sig.clone(), genesis_qc.clone())
            .expect("first timeout should succeed");

        let result = collector.add_timeout(0, sks[0].sign(&msg), genesis_qc);
        assert!(result.is_err());
        match result.unwrap_err() {
            ConsensusError::DuplicateVote { view: v, validator_index } => {
                assert_eq!(v, view);
                assert_eq!(validator_index, 0);
            }
            other => panic!("expected DuplicateVote, got: {:?}", other),
        }
    }

    #[test]
    fn test_timeout_collector_view_getter() {
        let collector = TimeoutCollector::new(42, 4);
        assert_eq!(collector.view(), 42);

        let collector2 = TimeoutCollector::new(0, 7);
        assert_eq!(collector2.view(), 0);
    }
}
