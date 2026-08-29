use alloy_primitives::{B256, Bytes, U256, keccak256};
use alloy_rpc_types_engine::{ExecutionData, PayloadAttributes, PayloadError};
use n42_consensus::{
    N42HeaderProfile, gov5_native_rewards_root, gov5_withdrawals_to_rewards,
    remembered_gov5_native_header, validate_gov5_h2_header, validate_gov5_header_extra,
    validate_gov5_replay_v2_header,
};
use reth_chainspec::{EthereumHardforks, Hardforks};
use reth_engine_primitives::{EngineApiValidator, EngineTypes, PayloadValidator};
use reth_ethereum_primitives::{Block as EthBlock, EthPrimitives, TransactionSigned};
use reth_node_api::{AddOnsContext, FullNodeComponents};
use reth_node_builder::{node::NodeTypes, rpc::PayloadValidatorBuilder};
use reth_node_ethereum::node::EthereumEngineValidator;
use reth_payload_primitives::{
    EngineApiMessageVersion, EngineObjectValidationError, NewPayloadError, PayloadOrAttributes,
    PayloadTypes,
};
use reth_primitives_traits::{Block, SealedBlock};
use std::sync::Arc;

/// Engine payload validator with an explicit, chain-bound N42 header profile.
#[derive(Clone, Debug)]
pub struct N42EngineValidator<ChainSpec> {
    inner: EthereumEngineValidator<ChainSpec>,
    header_profile: N42HeaderProfile,
}

impl<ChainSpec> N42EngineValidator<ChainSpec> {
    pub const fn new(chain_spec: Arc<ChainSpec>, header_profile: N42HeaderProfile) -> Self {
        Self {
            inner: EthereumEngineValidator::new(chain_spec),
            header_profile,
        }
    }
}

impl<ChainSpec, Types> PayloadValidator<Types> for N42EngineValidator<ChainSpec>
where
    ChainSpec: reth_chainspec::EthChainSpec + EthereumHardforks + 'static,
    Types: PayloadTypes<ExecutionData = ExecutionData>,
{
    type Block = EthBlock;

    fn convert_payload_to_block(
        &self,
        payload: ExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        if self.header_profile == N42HeaderProfile::Ethereum {
            return <EthereumEngineValidator<ChainSpec> as PayloadValidator<Types>>::convert_payload_to_block(
                &self.inner,
                payload,
            );
        }

        let expected_hash = payload.block_hash();
        let original_extra = payload.payload.as_v1().extra_data.clone();
        let replay_v2_shape = original_extra.as_ref() == [0_u8; 32];
        if !replay_v2_shape {
            validate_gov5_header_extra(&original_extra).map_err(NewPayloadError::other)?;
        }
        let mut standard_payload = payload;
        standard_payload.payload.set_extra_data(Bytes::new());
        let mut standard_block = standard_payload
            .clone()
            .try_into_block::<TransactionSigned>()?;
        if replay_v2_shape {
            standard_block.header.extra_data = original_extra;
            standard_block.header.withdrawals_root = Some(keccak256([]));
            validate_gov5_replay_v2_header(&standard_block.header)
                .map_err(NewPayloadError::other)?;
            let replay = standard_block.seal_slow();
            if replay.hash() == expected_hash {
                return Ok(replay);
            }
            return Err(PayloadError::BlockHash {
                execution: replay.hash(),
                consensus: expected_hash,
            }
            .into());
        }
        standard_payload
            .payload
            .set_block_hash(standard_block.header.hash_slow());
        let standard = <EthereumEngineValidator<ChainSpec> as PayloadValidator<Types>>::convert_payload_to_block(
            &self.inner,
            standard_payload,
        )?;
        let mut block = standard.into_block();
        block.header.ommers_hash = B256::ZERO;
        block.header.extra_data = original_extra;
        // Current gov5 uses difficulty 0. Preserved replay-v2 ranges were produced while H2 used
        // difficulty 1. Engine payloads omit the field, so reconstruct both permitted values and
        // let the hash-authenticated block identity select exactly one without operator guessing.
        let mut current = block.clone();
        current.header.difficulty = U256::ZERO;
        validate_gov5_h2_header(&current.header).map_err(NewPayloadError::other)?;
        let current = current.seal_slow();
        if current.hash() == expected_hash {
            return Ok(current);
        }
        block.header.difficulty = U256::from(1);
        validate_gov5_h2_header(&block.header).map_err(NewPayloadError::other)?;
        let legacy = block.seal_slow();
        if legacy.hash() == expected_hash {
            return Ok(legacy);
        }
        // Live gov5 headers on chains with rewards, a committee pool or the
        // mobileAnchor fork carry fields alloy cannot re-encode (see
        // `Gov5NativeHeader`). The network layer remembered the exact
        // encoding behind `expected_hash`; bind the payload to it field by
        // field and seal with the hash gov5 committed to.
        if let Some(native) = remembered_gov5_native_header(&expected_hash) {
            let mut block = legacy.into_block();
            block.header.difficulty = native.header.difficulty;
            validate_gov5_h2_header(&native.header).map_err(NewPayloadError::other)?;
            let payload_rewards = block
                .body
                .withdrawals
                .as_deref()
                .map(|withdrawals| gov5_withdrawals_to_rewards(withdrawals))
                .unwrap_or_default();
            let payload_rewards_root = block
                .body
                .withdrawals
                .is_some()
                .then(|| gov5_native_rewards_root(&payload_rewards));
            let mismatch = |field: &str| {
                NewPayloadError::other(std::io::Error::other(format!(
                    "gov5 payload {expected_hash} disagrees with its remembered native header on {field}"
                )))
            };
            let p = &block.header;
            let n = &native.header;
            if p.parent_hash != n.parent_hash {
                return Err(mismatch("parentHash"));
            }
            if p.beneficiary != n.beneficiary {
                return Err(mismatch("miner"));
            }
            if p.state_root != n.state_root {
                return Err(mismatch("stateRoot"));
            }
            if p.transactions_root != n.transactions_root {
                return Err(mismatch("transactionsRoot"));
            }
            if p.receipts_root != n.receipts_root {
                return Err(mismatch("receiptsRoot"));
            }
            if p.logs_bloom != n.logs_bloom {
                return Err(mismatch("logsBloom"));
            }
            if p.number != n.number {
                return Err(mismatch("number"));
            }
            if p.gas_limit != n.gas_limit {
                return Err(mismatch("gasLimit"));
            }
            if p.gas_used != n.gas_used {
                return Err(mismatch("gasUsed"));
            }
            if p.timestamp != n.timestamp {
                return Err(mismatch("timestamp"));
            }
            if p.extra_data != n.extra_data {
                return Err(mismatch("extraData"));
            }
            if p.mix_hash != n.mix_hash {
                return Err(mismatch("mixHash"));
            }
            if p.base_fee_per_gas != n.base_fee_per_gas {
                return Err(mismatch("baseFeePerGas"));
            }
            if payload_rewards_root != n.withdrawals_root {
                return Err(mismatch("withdrawalsRoot (rewards)"));
            }
            if p.blob_gas_used.unwrap_or(0) != n.blob_gas_used.unwrap_or(0)
                || p.excess_blob_gas.unwrap_or(0) != n.excess_blob_gas.unwrap_or(0)
            {
                return Err(mismatch("blob gas"));
            }
            if p.parent_beacon_block_root != n.parent_beacon_block_root {
                return Err(mismatch("parentBeaconBlockRoot"));
            }
            block.header = native.header;
            return Ok(SealedBlock::new_unchecked(block, expected_hash));
        }
        Err(PayloadError::BlockHash {
            execution: current.hash(),
            consensus: expected_hash,
        }
        .into())
    }
}

impl<ChainSpec, Types> EngineApiValidator<Types> for N42EngineValidator<ChainSpec>
where
    ChainSpec: reth_chainspec::EthChainSpec + EthereumHardforks + 'static,
    Types: PayloadTypes<PayloadAttributes = PayloadAttributes, ExecutionData = ExecutionData>,
{
    fn validate_version_specific_fields(
        &self,
        version: EngineApiMessageVersion,
        payload_or_attrs: PayloadOrAttributes<'_, ExecutionData, PayloadAttributes>,
    ) -> Result<(), EngineObjectValidationError> {
        <EthereumEngineValidator<ChainSpec> as EngineApiValidator<Types>>::validate_version_specific_fields(
            &self.inner,
            version,
            payload_or_attrs,
        )
    }

    fn ensure_well_formed_attributes(
        &self,
        version: EngineApiMessageVersion,
        attributes: &PayloadAttributes,
    ) -> Result<(), EngineObjectValidationError> {
        <EthereumEngineValidator<ChainSpec> as EngineApiValidator<Types>>::ensure_well_formed_attributes(
            &self.inner,
            version,
            attributes,
        )
    }
}

/// Builder used by both the Engine API boundary and the in-process engine tree.
#[derive(Clone, Copy, Debug, Default)]
pub struct N42EngineValidatorBuilder {
    header_profile: N42HeaderProfile,
}

impl N42EngineValidatorBuilder {
    pub const fn new(header_profile: N42HeaderProfile) -> Self {
        Self { header_profile }
    }
}

impl<Node, Types> PayloadValidatorBuilder<Node> for N42EngineValidatorBuilder
where
    Types: NodeTypes<
            ChainSpec: Hardforks + EthereumHardforks + Clone + 'static,
            Payload: EngineTypes<ExecutionData = ExecutionData>
                         + PayloadTypes<PayloadAttributes = PayloadAttributes>,
            Primitives = EthPrimitives,
        >,
    Node: FullNodeComponents<Types = Types>,
{
    type Validator = N42EngineValidator<Types::ChainSpec>;

    async fn build(self, ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        Ok(N42EngineValidator::new(
            ctx.config.chain.clone(),
            self.header_profile,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Block as ConsensusBlock, BlockBody, Header};
    use reth_chainspec::ChainSpec;
    use reth_ethereum_engine_primitives::EthEngineTypes;

    fn zero_ommers_payload_with_difficulty(difficulty: U256) -> ExecutionData {
        let extra_data = [b"N42H".as_slice(), &[0_u8; 8], &[0_u8; 96]].concat();
        let block = ConsensusBlock {
            header: Header {
                ommers_hash: B256::ZERO,
                difficulty,
                base_fee_per_gas: Some(0),
                extra_data: extra_data.into(),
                ..Default::default()
            },
            body: BlockBody::<TransactionSigned>::default(),
        };
        ExecutionData::from_block_unchecked(block.header.hash_slow(), &block)
    }

    fn zero_ommers_payload() -> ExecutionData {
        zero_ommers_payload_with_difficulty(U256::ZERO)
    }

    #[test]
    fn gov5_profile_reconstructs_zero_ommers_block_hash() {
        let validator =
            N42EngineValidator::new(Arc::new(ChainSpec::default()), N42HeaderProfile::Gov5H2);
        let payload = zero_ommers_payload();
        let expected = payload.block_hash();
        let sealed = <N42EngineValidator<ChainSpec> as PayloadValidator<EthEngineTypes>>::convert_payload_to_block(
            &validator,
            payload,
        )
        .unwrap();

        assert_eq!(sealed.hash(), expected);
        assert_eq!(sealed.header().ommers_hash, B256::ZERO);
        assert_eq!(sealed.header().difficulty, U256::ZERO);
    }

    #[test]
    fn gov5_profile_preserves_legacy_difficulty_one_history() {
        let validator =
            N42EngineValidator::new(Arc::new(ChainSpec::default()), N42HeaderProfile::Gov5H2);
        let payload = zero_ommers_payload_with_difficulty(U256::from(1));
        let sealed = <N42EngineValidator<ChainSpec> as PayloadValidator<EthEngineTypes>>::convert_payload_to_block(
            &validator,
            payload,
        )
        .unwrap();
        assert_eq!(sealed.header().difficulty, U256::from(1));
    }

    #[test]
    fn standard_profile_and_tampered_hash_reject_zero_ommers_payload() {
        let standard =
            N42EngineValidator::new(Arc::new(ChainSpec::default()), N42HeaderProfile::Ethereum);
        assert!(
            <N42EngineValidator<ChainSpec> as PayloadValidator<EthEngineTypes>>::convert_payload_to_block(
                &standard,
                zero_ommers_payload(),
            )
            .is_err()
        );

        let gov5 =
            N42EngineValidator::new(Arc::new(ChainSpec::default()), N42HeaderProfile::Gov5H2);
        let mut tampered = zero_ommers_payload();
        tampered.payload.set_block_hash(B256::repeat_byte(0x42));
        assert!(
            <N42EngineValidator<ChainSpec> as PayloadValidator<EthEngineTypes>>::convert_payload_to_block(
                &gov5,
                tampered,
            )
            .is_err()
        );
    }

    #[test]
    fn gov5_profile_seals_native_headers_through_the_registry() {
        use alloy_eips::eip4895::Withdrawals;
        use n42_consensus::{Gov5NativeHeader, gov5_native_rewards_root};
        // A live chain-94 shape: reward root, parent beacon root, a `0x80`
        // requests placeholder and a mobile-registry root. Alloy's
        // re-encoding cannot reproduce it, so its hash differs from gov5's.
        let extra_data = [b"N42H".as_slice(), &[0_u8; 8], &[0_u8; 96]].concat();
        let header = Header {
            ommers_hash: B256::ZERO,
            number: 13_560_376,
            base_fee_per_gas: Some(7),
            withdrawals_root: Some(gov5_native_rewards_root(&[])),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::repeat_byte(0x22)),
            extra_data: extra_data.into(),
            ..Default::default()
        };
        let native = Gov5NativeHeader {
            header: header.clone(),
            mobile_registry_root: Some(B256::ZERO),
        };
        let raw = native.encode();
        let hash = n42_consensus::remember_gov5_native_header(&raw);
        assert_ne!(hash, header.hash_slow());
        let block = ConsensusBlock {
            header,
            body: BlockBody::<TransactionSigned> {
                withdrawals: Some(Withdrawals::default()),
                ..Default::default()
            },
        };
        let payload = ExecutionData::from_block_unchecked(hash, &block);
        // Withdrawals and a parent beacon root need Shanghai/Cancun active.
        let chain_spec = reth_chainspec::ChainSpecBuilder::default()
            .chain(reth_chainspec::Chain::from_id(94))
            .genesis(Default::default())
            .cancun_activated()
            .build();
        let validator = N42EngineValidator::new(Arc::new(chain_spec), N42HeaderProfile::Gov5H2);
        let sealed = <N42EngineValidator<ChainSpec> as PayloadValidator<EthEngineTypes>>::convert_payload_to_block(
            &validator,
            payload,
        )
        .unwrap();
        assert_eq!(sealed.hash(), hash);
        assert_eq!(
            sealed.header().parent_beacon_block_root,
            Some(B256::repeat_byte(0x22))
        );
        assert_eq!(
            sealed.header().withdrawals_root,
            Some(gov5_native_rewards_root(&[]))
        );

        // Without a remembered encoding the same payload has no provable hash.
        let mut unknown = block.clone();
        unknown.header.number += 1;
        let payload = ExecutionData::from_block_unchecked(B256::repeat_byte(0x99), &unknown);
        assert!(
            <N42EngineValidator<ChainSpec> as PayloadValidator<EthEngineTypes>>::convert_payload_to_block(
                &validator,
                payload,
            )
            .is_err()
        );
    }

    #[test]
    fn gov5_profile_rejects_unidentified_header_extra() {
        let gov5 =
            N42EngineValidator::new(Arc::new(ChainSpec::default()), N42HeaderProfile::Gov5H2);
        let mut payload = zero_ommers_payload();
        payload
            .payload
            .set_extra_data(Bytes::from_static(b"not-n42h"));
        assert!(
            <N42EngineValidator<ChainSpec> as PayloadValidator<EthEngineTypes>>::convert_payload_to_block(
                &gov5,
                payload,
            )
            .is_err()
        );
    }
}
