use alloy_consensus::{Block, BlockBody, EMPTY_OMMER_ROOT_HASH, Header, TxEnvelope};
use alloy_primitives::keccak256;
use alloy_rpc_types_engine::ExecutionData;
use n42_consensus::{
    N42HeaderProfile, validate_gov5_h2_header, validate_gov5_header_extra,
    validate_gov5_interop_header, validate_gov5_replay_v2_header,
};
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
    #[error("finalized block {0} could not be reconstructed from its Engine API payload: {1}")]
    PayloadReconstruction(u64, String),
    #[error("finalized block {0} violates the selected header profile: {1}")]
    HeaderProfile(u64, String),
    #[error(
        "finalized block {number} produced an inconsistent Engine API payload identity: expected {expected}, reconstructed {reconstructed}"
    )]
    PayloadIdentity {
        number: u64,
        expected: alloy_primitives::B256,
        reconstructed: alloy_primitives::B256,
    },
}

/// Converts already-authenticated entries to Engine API payloads without
/// submitting them. Finalized-range v1 does not carry ommers, withdrawals,
/// execution requests, or full block access lists, so any header requiring
/// those values is rejected instead of synthesizing them.
pub fn build_replay_execution_plan(
    range: &VerifiedFinalizedRange,
) -> Result<ReplayExecutionPlan, ReplayImportPlanError> {
    build_replay_execution_plan_with_profile(range, N42HeaderProfile::Ethereum)
}

/// Builds a replay plan using explicitly selected, chain-bound header semantics.
pub fn build_replay_execution_plan_with_profile(
    range: &VerifiedFinalizedRange,
    header_profile: N42HeaderProfile,
) -> Result<ReplayExecutionPlan, ReplayImportPlanError> {
    let verification = range.verification();
    if range.entries().len() as u64 != verification.block_count {
        return Err(ReplayImportPlanError::EntryCount);
    }

    let mut payloads = Vec::with_capacity(range.entries().len());
    for entry in range.entries() {
        payloads.push(build_execution_data(
            entry.block_hash(),
            entry.header(),
            entry.transactions(),
            header_profile,
        )?);
    }

    Ok(ReplayExecutionPlan {
        verification: verification.clone(),
        payloads,
    })
}

/// Converts a structurally verified Gov5 gossip block into an Engine payload
/// while proving that Engine's lossy header representation reconstructs to the
/// exact H2-authenticated outer block hash.
pub fn build_gov5_execution_data(
    block_hash: alloy_primitives::B256,
    header: &Header,
    transactions: &[TxEnvelope],
) -> Result<ExecutionData, ReplayImportPlanError> {
    build_execution_data(block_hash, header, transactions, N42HeaderProfile::Gov5H2)
}

fn build_execution_data(
    block_hash: alloy_primitives::B256,
    header: &Header,
    transactions: &[TxEnvelope],
    header_profile: N42HeaderProfile,
) -> Result<ExecutionData, ReplayImportPlanError> {
    let number = header.number;
    validate_v1_payload_inputs(number, header, header_profile)?;
    let block = Block {
        header: header.clone(),
        body: BlockBody {
            transactions: transactions.to_vec(),
            ommers: Vec::new(),
            withdrawals: None,
        },
    };
    let payload = ExecutionData::from_block_unchecked(block_hash, &block);
    let original_extra = payload.payload.as_v1().extra_data.clone();
    let mut reconstruction_payload = payload.clone();
    let replay_v2_header = header_profile == N42HeaderProfile::Gov5H2
        && validate_gov5_replay_v2_header(header).is_ok();
    if header_profile == N42HeaderProfile::Gov5H2 && !replay_v2_header {
        validate_gov5_header_extra(&original_extra).map_err(|error| {
            ReplayImportPlanError::PayloadReconstruction(number, error.to_string())
        })?;
        reconstruction_payload
            .payload
            .set_extra_data(alloy_primitives::Bytes::new());
    }
    let mut reconstructed = reconstruction_payload
        .try_into_block::<TxEnvelope>()
        .map_err(|error| ReplayImportPlanError::PayloadReconstruction(number, error.to_string()))?;
    if replay_v2_header {
        reconstructed.header.withdrawals_root = Some(keccak256([]));
        validate_gov5_replay_v2_header(&reconstructed.header).map_err(|error| {
            ReplayImportPlanError::PayloadReconstruction(number, error.to_string())
        })?;
    } else if header_profile == N42HeaderProfile::Gov5H2 {
        reconstructed.header.ommers_hash = alloy_primitives::B256::ZERO;
        reconstructed.header.extra_data = original_extra;
        for difficulty in [
            alloy_primitives::U256::ZERO,
            alloy_primitives::U256::from(1),
        ] {
            reconstructed.header.difficulty = difficulty;
            if reconstructed.header.hash_slow() == block_hash {
                break;
            }
        }
        validate_gov5_h2_header(&reconstructed.header).map_err(|error| {
            ReplayImportPlanError::PayloadReconstruction(number, error.to_string())
        })?;
    }
    if payload.block_hash() != block_hash
        || payload.parent_hash() != header.parent_hash
        || payload.block_number() != number
        || reconstructed.header.hash_slow() != block_hash
    {
        return Err(ReplayImportPlanError::PayloadIdentity {
            number,
            expected: block_hash,
            reconstructed: reconstructed.header.hash_slow(),
        });
    }
    Ok(payload)
}

fn validate_v1_payload_inputs(
    number: u64,
    header: &Header,
    header_profile: N42HeaderProfile,
) -> Result<(), ReplayImportPlanError> {
    let expected_ommers_hash = if header_profile == N42HeaderProfile::Gov5H2
        && validate_gov5_replay_v2_header(header).is_ok()
    {
        EMPTY_OMMER_ROOT_HASH
    } else if header_profile == N42HeaderProfile::Gov5H2 {
        alloy_primitives::B256::ZERO
    } else {
        EMPTY_OMMER_ROOT_HASH
    };
    if header.ommers_hash != expected_ommers_hash {
        return Err(ReplayImportPlanError::UnsupportedOmmersHash(
            number,
            header.ommers_hash,
        ));
    }
    let replay_v2_header = header_profile == N42HeaderProfile::Gov5H2
        && validate_gov5_replay_v2_header(header).is_ok();
    if !replay_v2_header && header.withdrawals_root.is_some() {
        return Err(ReplayImportPlanError::MissingWithdrawals(number));
    }
    if !replay_v2_header && header.requests_hash.is_some() {
        return Err(ReplayImportPlanError::MissingRequests(number));
    }
    if header.block_access_list_hash.is_some() {
        return Err(ReplayImportPlanError::MissingBlockAccessList(number));
    }
    if header_profile == N42HeaderProfile::Gov5H2 {
        validate_gov5_interop_header(header)
            .map_err(|error| ReplayImportPlanError::HeaderProfile(number, error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    #[test]
    fn v1_payload_profile_accepts_standard_empty_ommers_shape() {
        validate_v1_payload_inputs(7, &Header::default(), N42HeaderProfile::Ethereum).unwrap();
    }

    #[test]
    fn v1_payload_profile_rejects_omitted_fork_data() {
        let mut header = Header {
            withdrawals_root: Some(B256::ZERO),
            ..Default::default()
        };
        assert_eq!(
            validate_v1_payload_inputs(7, &header, N42HeaderProfile::Ethereum),
            Err(ReplayImportPlanError::MissingWithdrawals(7))
        );

        header.withdrawals_root = None;
        header.requests_hash = Some(B256::ZERO);
        assert_eq!(
            validate_v1_payload_inputs(8, &header, N42HeaderProfile::Ethereum),
            Err(ReplayImportPlanError::MissingRequests(8))
        );

        header.requests_hash = None;
        header.block_access_list_hash = Some(B256::ZERO);
        assert_eq!(
            validate_v1_payload_inputs(9, &header, N42HeaderProfile::Ethereum),
            Err(ReplayImportPlanError::MissingBlockAccessList(9))
        );
    }

    #[test]
    fn standard_engine_profile_rejects_gov5_zero_ommers_hash() {
        let header = Header {
            ommers_hash: B256::ZERO,
            difficulty: alloy_primitives::U256::from(1),
            extra_data: [b"N42H".as_slice(), &[0_u8; 8]].concat().into(),
            ..Default::default()
        };
        assert_eq!(
            validate_v1_payload_inputs(7, &header, N42HeaderProfile::Ethereum),
            Err(ReplayImportPlanError::UnsupportedOmmersHash(7, B256::ZERO))
        );
        validate_v1_payload_inputs(7, &header, N42HeaderProfile::Gov5H2).unwrap();

        let wrong_difficulty = Header {
            difficulty: alloy_primitives::U256::from(2),
            ..header
        };
        assert!(matches!(
            validate_v1_payload_inputs(7, &wrong_difficulty, N42HeaderProfile::Gov5H2),
            Err(ReplayImportPlanError::HeaderProfile(7, _))
        ));
    }
}
