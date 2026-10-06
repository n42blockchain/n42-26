use alloy_eips::Decodable2718;
use alloy_primitives::Bytes;
use alloy_rpc_types_engine::ExecutionData;
use reth_chainspec::ChainSpec;
use reth_evm::{
    ConfigureEngineEvm, ConfigureEvm, EvmEnvFor, ExecutableTxIterator, ExecutionCtxFor,
    SenderRecoveryCache,
};
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::{
    BlockTy, HeaderTy, SealedBlock, SealedHeader, SignedTransaction, TxTy,
};
use reth_storage_errors::any::AnyError;
use std::sync::Arc;

use crate::{evm_factory::N42EvmFactory, restored_slots::TrackingBlockExecutorFactory};

/// The inner EthEvmConfig type we delegate to (parameterized with N42EvmFactory).
type InnerConfig = EthEvmConfig<ChainSpec, N42EvmFactory>;

/// N42 EVM configuration wrapping `EthEvmConfig<ChainSpec>`.
/// All `ConfigureEvm` methods delegate to the inner config, except that every
/// block executor is wrapped by [`TrackingBlockExecutorFactory`] so the slots
/// a block changes and restores are recorded for the QMDB root job (see
/// `restored_slots`). Provides a distinct type for N42-specific extensions.
#[derive(Debug, Clone)]
pub struct N42EvmConfig {
    /// Inner Ethereum EVM configuration.
    inner: InnerConfig,
    /// The inner block executor factory, watched for restored slots.
    factory: TrackingBlockExecutorFactory<<InnerConfig as ConfigureEvm>::BlockExecutorFactory>,
}

impl N42EvmConfig {
    /// Creates a new N42 EVM configuration from a chain spec.
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        let inner = EthEvmConfig::new_with_evm_factory(chain_spec, N42EvmFactory);
        let factory = TrackingBlockExecutorFactory::new(inner.block_executor_factory().clone());
        Self { inner, factory }
    }

    /// Returns a reference to the inner `EthEvmConfig`.
    pub fn inner(&self) -> &InnerConfig {
        &self.inner
    }

    /// Shares verified sender recovery results with transaction ingress.
    pub fn with_sender_recovery_cache(mut self, cache: SenderRecoveryCache) -> Self {
        self.inner = self.inner.with_sender_recovery_cache(cache);
        self
    }

    /// Returns the chain spec.
    pub fn chain_spec(&self) -> &Arc<ChainSpec> {
        self.inner.chain_spec()
    }
}

impl ConfigureEvm for N42EvmConfig {
    type Primitives = <InnerConfig as ConfigureEvm>::Primitives;
    type Error = <InnerConfig as ConfigureEvm>::Error;
    type NextBlockEnvCtx = <InnerConfig as ConfigureEvm>::NextBlockEnvCtx;
    type BlockExecutorFactory =
        TrackingBlockExecutorFactory<<InnerConfig as ConfigureEvm>::BlockExecutorFactory>;
    type BlockAssembler = <InnerConfig as ConfigureEvm>::BlockAssembler;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        self.inner.block_assembler()
    }

    fn evm_env(&self, header: &HeaderTy<Self::Primitives>) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.evm_env(header)
    }

    fn next_evm_env(
        &self,
        parent: &HeaderTy<Self::Primitives>,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.next_evm_env(parent, attributes)
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<BlockTy<Self::Primitives>>,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        self.inner.context_for_block(block)
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader<HeaderTy<Self::Primitives>>,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<ExecutionCtxFor<'_, Self>, Self::Error> {
        self.inner.context_for_next_block(parent, attributes)
    }
}

impl ConfigureEngineEvm<ExecutionData> for N42EvmConfig {
    fn evm_env_for_payload(&self, payload: &ExecutionData) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.evm_env_for_payload(payload)
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a ExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        self.inner.context_for_payload(payload)
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        let txs = payload.payload.transactions().clone();
        let sender_recovery_cache = self.inner.sender_recovery_cache.clone();
        let convert = move |tx: Bytes| {
            let tx =
                TxTy::<Self::Primitives>::decode_2718_exact(tx.as_ref()).map_err(AnyError::new)?;
            let signer = if let Some(cache) = &sender_recovery_cache {
                cache.recover(&tx)
            } else {
                tx.try_recover()
            }
            .map_err(AnyError::new)?;
            Ok::<_, AnyError>(tx.with_signer(signer))
        };
        Ok((txs, convert))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Block, BlockBody, Header, TxLegacy};
    use alloy_primitives::{B256, Signature, U256};
    use n42_chainspec::{N42_CHAIN_ID, n42_dev_chainspec};
    use reth_ethereum_primitives::{Transaction, TransactionSigned};
    use reth_evm::{ConvertTx, ExecutableTxTuple};

    fn payload(transactions: Vec<TransactionSigned>) -> ExecutionData {
        let block = Block {
            header: Header::default(),
            body: BlockBody {
                transactions,
                ..Default::default()
            },
        };
        ExecutionData::from_block_unchecked(B256::ZERO, &block)
    }

    #[test]
    fn payload_recovery_shares_cache_with_ingress_and_config_clones() {
        let tx = TransactionSigned::new_unhashed(
            Transaction::Legacy(TxLegacy::default()),
            Signature::test_signature(),
        );
        let expected = tx.try_recover().unwrap();
        let cache = SenderRecoveryCache::new(4);
        let config =
            N42EvmConfig::new(n42_dev_chainspec()).with_sender_recovery_cache(cache.clone());
        assert_eq!(cache.get(tx.tx_hash()), None);
        let input = payload(vec![tx.clone()]);
        for config in [config.clone(), config] {
            let (raw, convert) = config.tx_iterator_for_payload(&input).unwrap().into_parts();
            for tx in raw {
                assert!(convert.convert(tx).is_ok());
            }
            assert_eq!(cache.get(tx.tx_hash()), Some(expected));
        }
    }

    #[test]
    fn payload_recovery_rejects_invalid_signatures_with_or_without_cache() {
        let tx = TransactionSigned::new_unhashed(
            Transaction::Legacy(TxLegacy::default()),
            Signature::new(U256::ZERO, U256::ZERO, false),
        );
        let cache = SenderRecoveryCache::new(4);
        let config = N42EvmConfig::new(n42_dev_chainspec());
        let input = payload(vec![tx.clone()]);
        for config in [
            config.clone(),
            config.with_sender_recovery_cache(cache.clone()),
        ] {
            let (raw, convert) = config.tx_iterator_for_payload(&input).unwrap().into_parts();
            for tx in raw {
                assert!(convert.convert(tx).is_err());
            }
            assert_eq!(cache.get(tx.tx_hash()), None);
        }
    }

    #[test]
    fn test_evm_config_creation() {
        let chain_spec = n42_dev_chainspec();
        let config = N42EvmConfig::new(chain_spec);

        // Should not panic, and the config should be usable.
        assert_eq!(
            config.chain_spec().chain().id(),
            N42_CHAIN_ID,
            "chain_spec should have N42 chain ID"
        );
    }

    #[test]
    fn test_evm_config_chain_spec() {
        let chain_spec = n42_dev_chainspec();
        let config = N42EvmConfig::new(chain_spec.clone());
        assert!(Arc::ptr_eq(config.chain_spec(), &chain_spec));
    }

    #[test]
    fn test_evm_config_inner() {
        let chain_spec = n42_dev_chainspec();
        let config = N42EvmConfig::new(chain_spec);
        let _inner = config.inner();
    }

    #[test]
    fn test_evm_config_clone() {
        let chain_spec = n42_dev_chainspec();
        let config = N42EvmConfig::new(chain_spec);
        let cloned = config.clone();

        assert_eq!(
            cloned.chain_spec().chain().id(),
            config.chain_spec().chain().id(),
            "cloned config should have the same chain ID"
        );
    }

    #[test]
    fn test_evm_config_debug() {
        let chain_spec = n42_dev_chainspec();
        let config = N42EvmConfig::new(chain_spec);
        let debug_str = format!("{:?}", config);
        assert!(!debug_str.is_empty());
    }
}
