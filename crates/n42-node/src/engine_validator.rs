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

struct PayloadConversionTimer {
    phase: &'static str,
    started: std::time::Instant,
}

impl PayloadConversionTimer {
    fn new(phase: &'static str) -> Self {
        Self {
            phase,
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for PayloadConversionTimer {
    fn drop(&mut self) {
        metrics::histogram!("n42_engine_payload_conversion_duration_ms", "phase" => self.phase)
            .record(self.started.elapsed().as_secs_f64() * 1_000.0);
    }
}

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
        let prepare_timer = PayloadConversionTimer::new(if replay_v2_shape {
            "replay_v2"
        } else {
            "standard_header"
        });
        let mut standard_payload = payload;
        standard_payload.payload.set_extra_data(Bytes::new());
        if replay_v2_shape {
            let mut standard_block = standard_payload.try_into_block::<TransactionSigned>()?;
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
        // This pass only needs the standard header hash. Build it from raw
        // envelopes; the upstream validator below still decodes every
        // transaction and checks the standard header and fork-specific fields.
        let standard_header = standard_payload.clone().into_block_raw()?.header;
        standard_payload
            .payload
            .set_block_hash(standard_header.hash_slow());
        drop(prepare_timer);
        let standard_timer = PayloadConversionTimer::new("ethereum_validate");
        let standard = <EthereumEngineValidator<ChainSpec> as PayloadValidator<Types>>::convert_payload_to_block(
            &self.inner,
            standard_payload,
        )?;
        drop(standard_timer);
        let _native_timer = PayloadConversionTimer::new("native_header_bind");
        let mut block = standard.into_block();
        block.header.ommers_hash = B256::ZERO;
        block.header.extra_data = original_extra;
        // Current gov5 uses difficulty 0. Preserved replay-v2 ranges were produced while H2 used
        // difficulty 1. Engine payloads omit the field, so reconstruct both permitted values and
        // let the hash-authenticated block identity select exactly one without operator guessing.
        block.header.difficulty = U256::ZERO;
        validate_gov5_h2_header(&block.header).map_err(NewPayloadError::other)?;
        let current = block.seal_slow();
        let current_hash = current.hash();
        if current.hash() == expected_hash {
            return Ok(current);
        }
        // Reuse the transaction body when trying the legacy header variant.
        let mut block = current.into_block();
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
            execution: current_hash,
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

    fn transaction_block(difficulty: U256) -> EthBlock {
        use alloy_consensus::{SignableTransaction, TxLegacy, proofs::calculate_transaction_root};
        use alloy_primitives::Signature;

        let transactions: Vec<TransactionSigned> = (0..2)
            .map(|nonce| {
                TxLegacy {
                    nonce,
                    ..Default::default()
                }
                .into_signed(Signature::new(U256::from(1), U256::from(2), false))
                .into()
            })
            .collect();
        let mut payload = zero_ommers_payload_with_difficulty(difficulty);
        payload.payload.set_extra_data(Bytes::new());
        let mut block = payload.try_into_block::<TransactionSigned>().unwrap();
        block.header.extra_data = [b"N42H".as_slice(), &[0_u8; 8], &[0_u8; 96]]
            .concat()
            .into();
        block.header.ommers_hash = B256::ZERO;
        block.header.difficulty = difficulty;
        block.header.transactions_root = calculate_transaction_root(&transactions);
        block.body.transactions = transactions;
        block
    }

    #[test]
    fn gov5_nonempty_body_survives_current_and_legacy_header_variants() {
        let validator =
            N42EngineValidator::new(Arc::new(ChainSpec::default()), N42HeaderProfile::Gov5H2);
        for difficulty in [U256::ZERO, U256::from(1)] {
            let block = transaction_block(difficulty);
            let expected = block.header.hash_slow();
            let payload = ExecutionData::from_block_unchecked(expected, &block);
            let sealed = <N42EngineValidator<ChainSpec> as PayloadValidator<EthEngineTypes>>::convert_payload_to_block(
                &validator, payload,
            ).unwrap();
            assert_eq!(sealed.hash(), expected);
            assert_eq!(sealed.body().transactions, block.body.transactions);
        }
    }

    #[test]
    fn gov5_rejects_malformed_or_nonexact_transaction_envelopes() {
        use alloy_eips::eip2718::Encodable2718;

        let validator =
            N42EngineValidator::new(Arc::new(ChainSpec::default()), N42HeaderProfile::Gov5H2);
        let block = transaction_block(U256::ZERO);
        let mut trailing = block.body.transactions[0].encoded_2718();
        trailing.push(0);
        for encoded in [vec![0x80], vec![0x7f, 0xc0], trailing] {
            let mut payload = ExecutionData::from_block_unchecked(block.header.hash_slow(), &block);
            payload.payload.transactions_mut()[0] = encoded.into();
            assert!(<N42EngineValidator<ChainSpec> as PayloadValidator<EthEngineTypes>>::convert_payload_to_block(
                &validator, payload,
            ).is_err());
        }
    }

    #[test]
    fn gov5_rejects_changed_body_under_the_original_header_hash() {
        let validator =
            N42EngineValidator::new(Arc::new(ChainSpec::default()), N42HeaderProfile::Gov5H2);
        let block = transaction_block(U256::ZERO);
        let mut payload = ExecutionData::from_block_unchecked(block.header.hash_slow(), &block);
        payload.payload.transactions_mut().swap(0, 1);
        assert!(<N42EngineValidator<ChainSpec> as PayloadValidator<EthEngineTypes>>::convert_payload_to_block(
            &validator, payload,
        ).is_err());
    }

    #[test]
    fn gov5_keeps_upstream_pre_shanghai_withdrawal_rejection() {
        let validator =
            N42EngineValidator::new(Arc::new(ChainSpec::default()), N42HeaderProfile::Gov5H2);
        let mut block = transaction_block(U256::ZERO);
        block.body.withdrawals = Some(Default::default());
        block.header.withdrawals_root = Some(alloy_consensus::constants::EMPTY_ROOT_HASH);
        let payload = ExecutionData::from_block_unchecked(block.header.hash_slow(), &block);
        assert!(<N42EngineValidator<ChainSpec> as PayloadValidator<EthEngineTypes>>::convert_payload_to_block(
            &validator, payload,
        ).is_err());
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
    fn gov5_replay_v2_preserves_nonempty_body_and_exact_decoding() {
        let validator =
            N42EngineValidator::new(Arc::new(ChainSpec::default()), N42HeaderProfile::Gov5H2);
        let mut block = transaction_block(U256::ZERO);
        block.header.ommers_hash = alloy_consensus::constants::EMPTY_OMMER_ROOT_HASH;
        block.header.extra_data = Bytes::from(vec![0; 32]);
        block.header.withdrawals_root = Some(keccak256([]));
        block.header.blob_gas_used = Some(0);
        block.header.excess_blob_gas = Some(0);
        block.header.parent_beacon_block_root = Some(B256::ZERO);
        block.header.requests_hash = Some(alloy_consensus::constants::EMPTY_ROOT_HASH);
        let expected = block.header.hash_slow();
        let payload = ExecutionData::from_block_unchecked(expected, &block);
        let sealed = <N42EngineValidator<ChainSpec> as PayloadValidator<EthEngineTypes>>::convert_payload_to_block(
            &validator, payload.clone(),
        ).unwrap();
        assert_eq!(sealed.hash(), expected);
        assert_eq!(sealed.body().transactions, block.body.transactions);
        let mut malformed = payload;
        malformed.payload.transactions_mut()[0] = Bytes::from_static(&[0x80]);
        assert!(<N42EngineValidator<ChainSpec> as PayloadValidator<EthEngineTypes>>::convert_payload_to_block(
            &validator, malformed,
        ).is_err());
    }

    #[test]
    fn gov5_profile_seals_native_headers_through_the_registry() {
        use alloy_eips::eip4895::Withdrawals;
        use n42_consensus::{Gov5NativeHeader, gov5_native_rewards_root};
        // A live chain-94 shape: reward root, parent beacon root, a `0x80`
        // requests placeholder and a mobile-registry root. Alloy's
        // re-encoding cannot reproduce it, so its hash differs from gov5's.
        let extra_data = [b"N42H".as_slice(), &[0_u8; 8], &[0_u8; 96]].concat();
        let transactions = transaction_block(U256::ZERO).body.transactions;
        let header = Header {
            ommers_hash: B256::ZERO,
            number: 13_560_376,
            transactions_root: alloy_consensus::proofs::calculate_transaction_root(&transactions),
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
                transactions,
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
        assert_eq!(sealed.body().transactions, block.body.transactions);
        let mut changed = ExecutionData::from_block_unchecked(hash, &block);
        changed.payload.transactions_mut().swap(0, 1);
        assert!(<N42EngineValidator<ChainSpec> as PayloadValidator<EthEngineTypes>>::convert_payload_to_block(
            &validator, changed,
        ).is_err());

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
