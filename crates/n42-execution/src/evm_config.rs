use alloy_eips::Decodable2718;
use alloy_evm::eth::{
    EthBlockExecutionCtx, EthBlockExecutorFactory,
    receipt_builder::{ReceiptBuilder, ReceiptBuilderCtx},
};
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_engine::ExecutionData;
use n42_tx_types::{AltSigSenderCache, N42Primitives, N42TxEnvelope, N42TxType, Receipt};
use reth_chainspec::ChainSpec;
use reth_evm::{
    ConfigureEngineEvm, ConfigureEvm, Evm, EvmEnvFor, ExecutableTxIterator, ExecutionCtxFor,
    SenderRecoveryCache,
};
use reth_evm_ethereum::{EthBlockAssembler, EthEvmConfig};
use reth_primitives_traits::{
    BlockTy, HeaderTy, SealedBlock, SealedHeader, SignedTransaction, TxTy,
};
use reth_storage_api::{StateProvider, StateProviderBox};
use reth_storage_errors::{any::AnyError, provider::ProviderResult};
use std::{borrow::Cow, sync::Arc};

use crate::{evm_factory::N42EvmFactory, restored_slots::TrackingBlockExecutorFactory};

/// The Ethereum EVM configuration the header and payload environments are
/// taken from. Those depend only on the header and the chain spec, never on
/// the transaction type, so they are shared with Ethereum unchanged.
type InnerConfig = EthEvmConfig<ChainSpec, N42EvmFactory>;

/// Receipts for N42 transactions: Ethereum's receipt shape with the N42 type
/// byte, so a 0x50 transaction's receipt reports type 0x50.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct N42ReceiptBuilder;

impl ReceiptBuilder for N42ReceiptBuilder {
    type Transaction = N42TxEnvelope;
    type Receipt = Receipt;

    fn build_receipt<E: Evm>(&self, ctx: ReceiptBuilderCtx<'_, N42TxType, E>) -> Self::Receipt {
        let ReceiptBuilderCtx {
            tx_type,
            result,
            cumulative_gas_used,
            ..
        } = ctx;
        Receipt {
            tx_type,
            success: result.is_success(),
            cumulative_gas_used,
            logs: result.into_logs(),
        }
    }
}

/// The block executor factory over N42 transactions and receipts.
pub type N42BlockExecutorFactory =
    EthBlockExecutorFactory<N42ReceiptBuilder, Arc<ChainSpec>, N42EvmFactory>;

/// N42 EVM configuration over [`N42Primitives`].
///
/// Header and payload environments delegate to the Ethereum configuration.
/// Blocks are executed by an Ethereum block executor whose receipt builder
/// knows the N42 type byte, and every block executor is wrapped by
/// [`TrackingBlockExecutorFactory`] so the slots a block changes and restores
/// are recorded for the QMDB root job (see `restored_slots`).
#[derive(Debug, Clone)]
pub struct N42EvmConfig {
    /// Ethereum configuration used for the header/payload environments and
    /// for the shared secp256k1 sender-recovery cache.
    inner: InnerConfig,
    /// The block executor factory, watched for restored slots.
    factory: TrackingBlockExecutorFactory<N42BlockExecutorFactory>,
    /// Assembles blocks of N42 transactions.
    assembler: EthBlockAssembler<ChainSpec>,
    parallel_import: Option<ParallelImportProvider>,
}

type ImportStateOpener = dyn Fn(B256) -> ProviderResult<StateProviderBox> + Send + Sync;

#[derive(Clone)]
struct ParallelImportProvider(Arc<ImportStateOpener>);

impl std::fmt::Debug for ParallelImportProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ParallelImportProvider(exact parent)")
    }
}

impl N42EvmConfig {
    /// Creates a new N42 EVM configuration from a chain spec.
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self::with_evm_factory(chain_spec, N42EvmFactory::from_env())
    }

    /// Configure the transaction executor while retaining block-level system calls,
    /// receipts, withdrawals and restored-slot tracking.
    pub fn with_evm_factory(chain_spec: Arc<ChainSpec>, evm_factory: N42EvmFactory) -> Self {
        let inner = EthEvmConfig::new_with_evm_factory(chain_spec.clone(), evm_factory.clone());
        let factory = TrackingBlockExecutorFactory::new(EthBlockExecutorFactory::new(
            N42ReceiptBuilder,
            chain_spec.clone(),
            evm_factory,
        ));
        Self {
            inner,
            factory,
            assembler: EthBlockAssembler::new(chain_spec),
            parallel_import: None,
        }
    }

    /// Enable complete transfer-block import using independent views of the exact parent.
    /// The provider must preserve QMDB-only execution-read routing when configured.
    pub fn with_parallel_import_provider(
        mut self,
        open: impl Fn(B256) -> ProviderResult<StateProviderBox> + Send + Sync + 'static,
    ) -> Self {
        self.parallel_import = Some(ParallelImportProvider(Arc::new(open)));
        self
    }

    /// Returns a reference to the inner Ethereum configuration.
    pub fn inner(&self) -> &InnerConfig {
        &self.inner
    }

    /// Shares verified secp256k1 sender recovery results with transaction ingress.
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
    type Primitives = N42Primitives;
    type Error = <InnerConfig as ConfigureEvm>::Error;
    type NextBlockEnvCtx = <InnerConfig as ConfigureEvm>::NextBlockEnvCtx;
    type BlockExecutorFactory = TrackingBlockExecutorFactory<N42BlockExecutorFactory>;
    type BlockAssembler = EthBlockAssembler<ChainSpec>;

    fn batch_import_enabled(&self) -> bool {
        self.parallel_import.is_some()
    }

    fn try_execute_import_batch<'a, DB: alloy_evm::Database + 'a>(
        &self,
        executor: &mut reth_evm::BlockExecutorForEvm<'a, Self, DB>,
        env: &EvmEnvFor<Self>,
        parent_hash: alloy_primitives::B256,
        transactions: &[alloy_consensus::transaction::Recovered<N42TxEnvelope>],
    ) -> Result<Option<reth_evm::ImportBatchFinalize>, reth_evm::execute::BlockExecutionError> {
        let Some(provider) = &self.parallel_import else {
            return Ok(None);
        };
        crate::parallel_block::try_commit_import_batch(executor, env, transactions, &|| {
            (provider.0)(parent_hash).ok().map(|provider| {
                reth_revm::database::StateProviderDatabase::new(provider.into_evm_state_provider())
            })
        })
        .map(|graft| {
            graft.map(|graft| {
                metrics::counter!("n42_parallel_import_executed_total")
                    .increment(transactions.len() as u64);
                Box::new(move |bundle: &mut revm::database::BundleState| {
                    crate::parallel_transfer::append_reverts(bundle, graft.reverts);
                }) as reth_evm::ImportBatchFinalize
            })
        })
    }

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.assembler
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
        // Mirrors the Ethereum context; only the body's transaction type differs.
        Ok(EthBlockExecutionCtx {
            tx_count_hint: Some(block.transaction_count()),
            parent_hash: block.header().parent_hash,
            parent_beacon_block_root: block.header().parent_beacon_block_root,
            ommers: &block.body().ommers,
            withdrawals: block
                .body()
                .withdrawals
                .as_ref()
                .map(|w| Cow::Borrowed(w.as_slice())),
            extra_data: block.header().extra_data.clone(),
            slot_number: block.header().slot_number,
        })
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
            let signer = match &tx {
                // Ed25519 senders verified at admission are reused; anything
                // else is verified here and remembered.
                N42TxEnvelope::AltSig(alt) => {
                    let cache = AltSigSenderCache::global();
                    match cache.get(alt.hash()) {
                        Some(sender) => Ok(sender),
                        None => tx
                            .try_recover()
                            .inspect(|sender| cache.insert(*alt.hash(), *sender)),
                    }
                }
                N42TxEnvelope::Eth(_) => {
                    if let Some(cache) = &sender_recovery_cache {
                        cache.recover(&tx)
                    } else {
                        tx.try_recover()
                    }
                }
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
