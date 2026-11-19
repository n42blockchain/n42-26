use alloy_eips::Decodable2718;
use alloy_primitives::Bytes;
use alloy_rpc_types_engine::ExecutionData;
use n42_tx_types::{AltSigSenderCache, N42Primitives, N42TxEnvelope, N42TxType, Receipt};
use reth_chainspec::ChainSpec;
use reth_evm::{
    ConfigureEngineEvm, ConfigureEvm, EvmEnvFor, ExecutableTxIterator, ExecutionCtxFor,
    ReceiptBuilder, ReceiptBuilderCtx, SenderRecoveryCache,
};
use reth_evm_ethereum::{
    EthBlockAssembler, EthBlockExecutionCtx, EthBlockExecutorFactory, EthEvmConfig,
};
use reth_primitives_traits::{
    BlockTy, HeaderTy, Recovered, SealedBlock, SealedHeader, SignedTransaction,
};
use reth_storage_errors::any::AnyError;
use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

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

    fn build_receipt<T: evm2::EvmTypes>(
        &self,
        ctx: ReceiptBuilderCtx<N42TxType, evm2::TxResult<T>>,
    ) -> Self::Receipt {
        let ReceiptBuilderCtx {
            tx_type,
            result,
            cumulative_gas_used,
        } = ctx;
        Receipt {
            tx_type,
            success: result.status,
            cumulative_gas_used,
            logs: result.logs,
        }
    }
}

/// The block executor factory over N42 transactions and receipts.
pub type N42BlockExecutorFactory =
    EthBlockExecutorFactory<N42ReceiptBuilder, ChainSpec, N42EvmFactory>;

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
        let factory =
            TrackingBlockExecutorFactory::new(EthBlockExecutorFactory::new_with_receipt_builder(
                N42ReceiptBuilder,
                chain_spec.clone(),
                evm_factory,
            ));
        Self {
            inner,
            factory,
            assembler: EthBlockAssembler::new(chain_spec),
        }
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
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error>
    where
        Self: 'a,
    {
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
        // Published once per payload: a registry lookup per transaction costs
        // more than the sender lookup it counts.
        for (outcome, count) in [
            ("hit", &SENDER_HIT),
            ("frame", &SENDER_FRAME),
            ("miss", &SENDER_MISS),
        ] {
            metrics::counter!("n42_alt_sig_payload_sender_total", "cache" => outcome)
                .absolute(count.load(Ordering::Relaxed));
        }
        let txs = payload.payload.transactions().clone();
        let convert = self.payload_transaction_converter();
        Ok((txs, convert))
    }
}

impl N42EvmConfig {
    /// Decodes one payload transaction and gives it its sender, from the
    /// caches the node's admission filled where they hold it.
    fn payload_transaction_converter(
        &self,
    ) -> impl Fn(Bytes) -> Result<Recovered<N42TxEnvelope>, AnyError> + Clone + Send + Sync + 'static
    {
        let sender_recovery_cache = self.inner.sender_recovery_cache.clone();
        move |tx: Bytes| {
            let tx =
                N42TxEnvelope::decode_2718_exact(tx.as_ref()).map_err(AnyError::new)?;
            let signer = match &tx {
                // Ed25519 senders verified at admission are reused; anything
                // else is verified here and remembered.
                N42TxEnvelope::AltSig(alt) => {
                    let cache = AltSigSenderCache::global();
                    match cache.get(alt.hash()) {
                        Some(sender) => {
                            SENDER_HIT.fetch_add(1, Ordering::Relaxed);
                            Ok(sender)
                        }
                        None => {
                            // The cache drops entries under load; a frame this
                            // node admitted and still holds has the sender too.
                            let held = n42_tx_types::frame_ref_blocks_enabled()
                                .then(|| n42_tx_types::FrameStore::global().sender_of(alt.hash()))
                                .flatten();
                            if let Some(sender) = held {
                                SENDER_FRAME.fetch_add(1, Ordering::Relaxed);
                                Ok(sender)
                            } else {
                                SENDER_MISS.fetch_add(1, Ordering::Relaxed);
                                tx.try_recover()
                                    .inspect(|sender| cache.insert(*alt.hash(), *sender))
                            }
                        }
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
        }
    }

    /// The payload's transactions decoded, with their senders, as the engine
    /// converts them: a sender found here is in the cache the engine's own
    /// conversion of the same payload reads.
    pub fn recover_payload_transactions(
        &self,
        payload: &ExecutionData,
    ) -> Result<(Vec<N42TxEnvelope>, Vec<alloy_primitives::Address>), AnyError> {
        use rayon::prelude::*;
        let convert = self.payload_transaction_converter();
        let recovered: Vec<_> = payload
            .payload
            .transactions()
            .par_iter()
            .map(|tx| convert(tx.clone()))
            .collect::<Result<_, _>>()?;
        Ok(recovered.into_iter().map(Recovered::into_parts).unzip())
    }
}

/// Where the senders of 0x50 payload transactions came from, since start.
static SENDER_HIT: AtomicU64 = AtomicU64::new(0);
static SENDER_FRAME: AtomicU64 = AtomicU64::new(0);
static SENDER_MISS: AtomicU64 = AtomicU64::new(0);

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
