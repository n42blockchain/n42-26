use alloy_primitives::{B256, Bytes, U256};
use alloy_rpc_types_engine::{ExecutionData, PayloadAttributes, PayloadError};
use n42_consensus::{N42HeaderProfile, validate_gov5_h2_header, validate_gov5_header_extra};
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
        validate_gov5_header_extra(&original_extra).map_err(NewPayloadError::other)?;
        let mut standard_payload = payload;
        standard_payload.payload.set_extra_data(Bytes::new());
        let standard_block = standard_payload
            .clone()
            .try_into_block::<TransactionSigned>()?;
        standard_payload
            .payload
            .set_block_hash(standard_block.header.hash_slow());
        let standard = <EthereumEngineValidator<ChainSpec> as PayloadValidator<Types>>::convert_payload_to_block(
            &self.inner,
            standard_payload,
        )?;

        let mut block = standard.into_block();
        block.header.ommers_hash = B256::ZERO;
        block.header.difficulty = U256::from(1);
        block.header.extra_data = original_extra;
        validate_gov5_h2_header(&block.header).map_err(NewPayloadError::other)?;
        let sealed = block.seal_slow();
        if sealed.hash() != expected_hash {
            return Err(PayloadError::BlockHash {
                execution: sealed.hash(),
                consensus: expected_hash,
            }
            .into());
        }
        Ok(sealed)
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

    fn zero_ommers_payload() -> ExecutionData {
        let extra_data = [b"N42H".as_slice(), &[0_u8; 8], &[0_u8; 96]].concat();
        let block = ConsensusBlock {
            header: Header {
                ommers_hash: B256::ZERO,
                difficulty: U256::from(1),
                base_fee_per_gas: Some(0),
                extra_data: extra_data.into(),
                ..Default::default()
            },
            body: BlockBody::<TransactionSigned>::default(),
        };
        ExecutionData::from_block_unchecked(block.header.hash_slow(), &block)
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
