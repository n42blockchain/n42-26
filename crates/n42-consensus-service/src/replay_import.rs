use alloy_consensus::{Block, BlockBody, EMPTY_OMMER_ROOT_HASH, Header, TxEnvelope};
use alloy_rpc_types_engine::ExecutionData;
use n42_network::{FinalizedRangeVerification, VerifiedFinalizedRange};

/// Side-effect-free Engine API input built exclusively from an authenticated
/// finalized range. Calling `new_payload` remains a separate, explicit phase.
#[derive(Debug, Clone)]
pub struct ReplayExecutionPlan {
    verification: FinalizedRangeVerification,
    payloads: Vec<ExecutionData>,
}

impl ReplayExecutionPlan {
    pub const fn verification(&self) -> &FinalizedRangeVerification {
        &self.verification
    }

    pub fn payloads(&self) -> &[ExecutionData] {
        &self.payloads
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplayImportPlanError {
    #[error("verified range entry count does not match its summary")]
    EntryCount,
    #[error("finalized block {0} uses unsupported ommers hash {1}")]
    UnsupportedOmmersHash(u64, alloy_primitives::B256),
    #[error("finalized block {0} requires withdrawals absent from finalized-range v1")]
    MissingWithdrawals(u64),
    #[error("finalized block {0} requires execution requests absent from finalized-range v1")]
    MissingRequests(u64),
    #[error("finalized block {0} requires a block access list absent from finalized-range v1")]
    MissingBlockAccessList(u64),
    #[error("finalized block {0} could not be reconstructed from its Engine API payload")]
    PayloadReconstruction(u64),
    #[error("finalized block {0} produced an inconsistent Engine API payload identity")]
    PayloadIdentity(u64),
}

/// Converts already-authenticated entries to Engine API payloads without
/// submitting them. Finalized-range v1 does not carry ommers, withdrawals,
/// execution requests, or full block access lists, so any header requiring
/// those values is rejected instead of synthesizing them.
pub fn build_replay_execution_plan(
    range: &VerifiedFinalizedRange,
) -> Result<ReplayExecutionPlan, ReplayImportPlanError> {
    let verification = range.verification();
    if range.entries().len() as u64 != verification.block_count {
        return Err(ReplayImportPlanError::EntryCount);
    }

    let mut payloads = Vec::with_capacity(range.entries().len());
    for entry in range.entries() {
        validate_v1_payload_inputs(entry.number(), entry.header())?;
        let block = Block {
            header: entry.header().clone(),
            body: BlockBody {
                transactions: entry.transactions().to_vec(),
                ommers: Vec::new(),
                withdrawals: None,
            },
        };
        let payload = ExecutionData::from_block_unchecked(entry.block_hash(), &block);
        let reconstructed = payload
            .clone()
            .try_into_block::<TxEnvelope>()
            .map_err(|_| ReplayImportPlanError::PayloadReconstruction(entry.number()))?;
        if payload.block_hash() != entry.block_hash()
            || payload.parent_hash() != entry.parent_hash()
            || payload.block_number() != entry.number()
            || reconstructed.header.hash_slow() != entry.block_hash()
        {
            return Err(ReplayImportPlanError::PayloadIdentity(entry.number()));
        }
        payloads.push(payload);
    }

    Ok(ReplayExecutionPlan {
        verification: verification.clone(),
        payloads,
    })
}

fn validate_v1_payload_inputs(number: u64, header: &Header) -> Result<(), ReplayImportPlanError> {
    if header.ommers_hash != EMPTY_OMMER_ROOT_HASH {
        return Err(ReplayImportPlanError::UnsupportedOmmersHash(
            number,
            header.ommers_hash,
        ));
    }
    if header.withdrawals_root.is_some() {
        return Err(ReplayImportPlanError::MissingWithdrawals(number));
    }
    if header.requests_hash.is_some() {
        return Err(ReplayImportPlanError::MissingRequests(number));
    }
    if header.block_access_list_hash.is_some() {
        return Err(ReplayImportPlanError::MissingBlockAccessList(number));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    #[test]
    fn v1_payload_profile_accepts_standard_empty_ommers_shape() {
        validate_v1_payload_inputs(7, &Header::default()).unwrap();
    }

    #[test]
    fn v1_payload_profile_rejects_omitted_fork_data() {
        let mut header = Header {
            withdrawals_root: Some(B256::ZERO),
            ..Default::default()
        };
        assert_eq!(
            validate_v1_payload_inputs(7, &header),
            Err(ReplayImportPlanError::MissingWithdrawals(7))
        );

        header.withdrawals_root = None;
        header.requests_hash = Some(B256::ZERO);
        assert_eq!(
            validate_v1_payload_inputs(8, &header),
            Err(ReplayImportPlanError::MissingRequests(8))
        );

        header.requests_hash = None;
        header.block_access_list_hash = Some(B256::ZERO);
        assert_eq!(
            validate_v1_payload_inputs(9, &header),
            Err(ReplayImportPlanError::MissingBlockAccessList(9))
        );
    }

    #[test]
    fn standard_engine_profile_rejects_gov5_zero_ommers_hash() {
        let header = Header {
            ommers_hash: B256::ZERO,
            ..Default::default()
        };
        assert_eq!(
            validate_v1_payload_inputs(7, &header),
            Err(ReplayImportPlanError::UnsupportedOmmersHash(7, B256::ZERO))
        );
    }
}
