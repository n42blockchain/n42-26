use alloy_primitives::B256;
use bitvec::prelude::*;
use n42_primitives::{
    bls::{AggregateSignature, BlsPublicKey, BlsSignature},
    consensus::{QuorumCertificate, TimeoutCertificate, ViewNumber},
};
use std::collections::HashMap;

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
}

impl VoteCollector {
    /// Creates a new vote collector for the given view and block.
    pub fn new(view: ViewNumber, block_hash: B256, set_size: u32) -> Self {
        Self {
            view,
            block_hash,
            votes: HashMap::new(),
            set_size,
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
    /// Returns an error if there aren't enough valid votes for a quorum.
    pub fn build_qc(
        &self,
        validator_set: &ValidatorSet,
    ) -> ConsensusResult<QuorumCertificate> {
        let quorum_size = validator_set.quorum_size();
        if self.votes.len() < quorum_size {
            return Err(ConsensusError::InsufficientVotes {
                view: self.view,
                have: self.votes.len(),
                need: quorum_size,
            });
        }

        // Build the signing message: (view || block_hash)
        let message = signing_message(self.view, &self.block_hash);

        // Verify each vote and collect valid signatures
        let mut valid_sigs: Vec<&BlsSignature> = Vec::new();
        let mut valid_pks: Vec<&BlsPublicKey> = Vec::new();
        let mut signers = bitvec![u8, Msb0; 0; self.set_size as usize];

        for (&idx, sig) in &self.votes {
            let pk = validator_set.get_public_key(idx)?;
            // Verify individual signature
            pk.verify(&message, sig).map_err(|_| {
                ConsensusError::InvalidSignature {
                    view: self.view,
                    validator_index: idx,
                }
            })?;
            valid_sigs.push(sig);
            valid_pks.push(pk);
            signers.set(idx as usize, true);
        }

        // Aggregate all valid signatures
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
}

impl TimeoutCollector {
    /// Creates a new timeout collector for the given view.
    pub fn new(view: ViewNumber, set_size: u32) -> Self {
        Self {
            view,
            timeouts: HashMap::new(),
            set_size,
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

    /// Returns the current number of collected timeouts.
    pub fn timeout_count(&self) -> usize {
        self.timeouts.len()
    }

    /// Checks if we have enough timeouts for a quorum.
    pub fn has_quorum(&self, quorum_size: usize) -> bool {
        self.timeouts.len() >= quorum_size
    }

    /// Builds a TimeoutCertificate by aggregating collected timeout signatures.
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
            let pk = validator_set.get_public_key(idx)?;
            pk.verify(&message, sig).map_err(|_| {
                ConsensusError::InvalidSignature {
                    view: self.view,
                    validator_index: idx,
                }
            })?;
            valid_sigs.push(sig);
            signers.set(idx as usize, true);

            // Track highest QC
            if highest_qc.as_ref().is_none_or(|hq| high_qc.view > hq.view) {
                highest_qc = Some(high_qc);
            }
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

/// Constructs the signing message for timeout: "timeout" || view (8 bytes LE).
pub fn timeout_signing_message(view: ViewNumber) -> Vec<u8> {
    let mut msg = Vec::with_capacity(15);
    msg.extend_from_slice(b"timeout");
    msg.extend_from_slice(&view.to_le_bytes());
    msg
}
