use alloy_eips::Decodable2718;
use alloy_primitives::Bytes;
use alloy_rpc_types_engine::ExecutionData;
use reth_chainspec::ChainSpec;
use reth_evm::{
    ConfigureEvm, ConfigureEngineEvm, EvmEnvFor, ExecutableTxIterator, ExecutionCtxFor,
};
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::{BlockTy, HeaderTy, SealedBlock, SealedHeader, SignedTransaction, TxTy};
use reth_storage_errors::any::AnyError;
use std::sync::Arc;

/// The inner EthEvmConfig type we delegate to.
type InnerConfig = EthEvmConfig<ChainSpec>;

/// N42 EVM configuration.
///
/// Wraps `EthEvmConfig<ChainSpec>` to provide the standard Ethereum EVM execution
/// while allowing N42-specific extensions for witness generation and state diff tracking.
///
/// All `ConfigureEvm` methods delegate to the inner `EthEvmConfig`. The wrapper gives us
/// a distinct type that can be extended with additional functionality without breaking
/// the reth trait system.
#[derive(Debug, Clone)]
pub struct N42EvmConfig {
    /// Inner Ethereum EVM configuration.
    inner: InnerConfig,
}

impl N42EvmConfig {
    /// Creates a new N42 EVM configuration from a chain spec.
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self {
            inner: EthEvmConfig::new(chain_spec),
        }
    }

    /// Returns a reference to the inner `EthEvmConfig`.
    pub fn inner(&self) -> &InnerConfig {
        &self.inner
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
    type BlockExecutorFactory = <InnerConfig as ConfigureEvm>::BlockExecutorFactory;
    type BlockAssembler = <InnerConfig as ConfigureEvm>::BlockAssembler;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        self.inner.block_executor_factory()
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        self.inner.block_assembler()
    }

    fn evm_env(
        &self,
        header: &HeaderTy<Self::Primitives>,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
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
    fn evm_env_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
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
        let convert = |tx: Bytes| {
            let tx =
                TxTy::<Self::Primitives>::decode_2718_exact(tx.as_ref()).map_err(AnyError::new)?;
            let signer = tx.try_recover().map_err(AnyError::new)?;
            Ok::<_, AnyError>(tx.with_signer(signer))
        };
        Ok((txs, convert))
    }
}
