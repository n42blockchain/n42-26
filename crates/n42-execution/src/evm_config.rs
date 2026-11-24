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
    EthBlockAssembler, EthBlockExecutionCtx, EthBlockExecutorFactory, EthEvmConfig, EvmFactory,
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
    /// The factory, for the spec raise of [`Self::amsterdam_execution_at`].
    evm_factory: N42EvmFactory,
    /// Profile 1 (AM1): from this time on blocks execute under evm2's
    /// Amsterdam rules, while reth's Amsterdam hardfork stays off in the chain
    /// spec. See [`profile1_amsterdam_execution_at`].
    amsterdam_execution_at: Option<u64>,
}

/// Profile 1 (mixed production standard AM1): `config.amsterdamTime` of a
/// Profile-1 genesis switches on exactly the Amsterdam execution set of
/// n42-rs `docs/GLAMSTERDAM_EXECUTION_PARITY.md`: evm2's
/// [`SpecId::AMSTERDAM`](evm2::SpecId::AMSTERDAM) version (its gas table,
/// EIP-8037 state gas and the 2^32-1 transaction gas cap, EIP-2780 intrinsic
/// gas, EIP-7623/7976 floor, EIP-7981, EIP-8038, EIP-7708 transfer logs,
/// EIP-7778, EIP-7954 code and initcode caps, EIP-8246, EIP-8024, EIP-7843
/// SLOTNUM = the block number). The chain spec built for a Profile-1 genesis
/// carries no reth Amsterdam hardfork
/// (`n42_chainspec::profile1::profile1_chain_spec`), so what reth and evm2
/// gate on the chain spec's Amsterdam stays off: the EIP-7928 block access
/// list (builder and payload validator) and the EIP-8282 end-of-block
/// builder-request calls. EIP-7997 is in neither client.
pub fn profile1_amsterdam_execution_at<C: reth_chainspec::EthChainSpec + ?Sized>(
    chain_spec: &C,
) -> Option<u64> {
    let genesis = chain_spec.genesis();
    if !n42_chainspec::profile1::genesis_declares_profile(genesis) {
        return None;
    }
    genesis.config.amsterdam_time
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
                evm_factory.clone(),
            ));
        let amsterdam_execution_at = profile1_amsterdam_execution_at(&chain_spec);
        Self {
            inner,
            factory,
            assembler: EthBlockAssembler::new(chain_spec),
            evm_factory,
            amsterdam_execution_at,
        }
    }

    /// Whether a block at `timestamp` executes under the Amsterdam rules
    /// while the chain spec schedules no reth Amsterdam (Profile 1, AM1).
    pub fn amsterdam_execution_active_at(&self, timestamp: u64) -> bool {
        self.amsterdam_execution_at
            .is_some_and(|at| timestamp >= at)
    }

    /// Raises an environment to evm2's Amsterdam version for a block under
    /// the Profile-1 Amsterdam execution, with SLOTNUM = `slot` (EIP-7843;
    /// a Profile-1 header's slot number is its block number, B10).
    fn raise_to_amsterdam(
        &self,
        mut env: EvmEnvFor<Self>,
        timestamp: u64,
        slot: u64,
    ) -> EvmEnvFor<Self> {
        if !self.amsterdam_execution_active_at(timestamp) {
            return env;
        }
        if !env.spec.enables(evm2::SpecId::AMSTERDAM) {
            let max_blobs_per_tx = env.version.max_blobs_per_tx;
            let chain_id = self.chain_spec().chain.id();
            env.spec = self.evm_factory.spec_id(evm2::SpecId::AMSTERDAM);
            env.version = self
                .evm_factory
                .version(evm2::SpecId::AMSTERDAM, chain_id);
            env.version.max_blobs_per_tx = max_blobs_per_tx;
        }
        env.block.slot_num = alloy_primitives::U256::from(slot);
        env
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
        let env = self.inner.evm_env(header)?;
        Ok(self.raise_to_amsterdam(
            env,
            header.timestamp,
            header.slot_number.unwrap_or(header.number),
        ))
    }

    fn next_evm_env(
        &self,
        parent: &HeaderTy<Self::Primitives>,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        let env = self.inner.next_evm_env(parent, attributes)?;
        let number = parent.number.saturating_add(1);
        Ok(self.raise_to_amsterdam(
            env,
            attributes.timestamp,
            attributes.slot_number.unwrap_or(number),
        ))
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
        let env = self.inner.evm_env_for_payload(payload)?;
        let slot = payload
            .payload
            .as_v4()
            .map_or(payload.payload.block_number(), |v4| v4.slot_number);
        Ok(self.raise_to_amsterdam(env, payload.payload.timestamp(), slot))
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
            let (tx, source) = recover_payload_transaction(sender_recovery_cache.as_ref(), &tx)?;
            if let Some(source) = source {
                source.counter().fetch_add(1, Ordering::Relaxed);
            }
            Ok(tx)
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
        let cache = self.inner.sender_recovery_cache.as_ref();
        let raw = payload.payload.transactions();
        // Decoded from the payload's own bytes: a clone of each would count
        // a reference on a buffer the transactions may all share, from every
        // worker at once. Each lands in its own place of the result, and the
        // senders' sources are counted once for the payload, not once per
        // transaction on counters every worker writes.
        let recovered: Vec<_> =
            raw.par_iter().map(|tx| recover_payload_transaction(cache, tx)).collect();
        let (mut transactions, mut senders) =
            (Vec::with_capacity(raw.len()), Vec::with_capacity(raw.len()));
        let mut sources = [0u64; 3];
        for recovered in recovered {
            let (tx, source) = recovered?;
            if let Some(source) = source {
                sources[source as usize] += 1;
            }
            let (tx, sender) = tx.into_parts();
            transactions.push(tx);
            senders.push(sender);
        }
        for (source, count) in
            [SenderSource::Hit, SenderSource::Frame, SenderSource::Miss].into_iter().zip(sources)
        {
            if count > 0 {
                source.counter().fetch_add(count, Ordering::Relaxed);
            }
        }
        Ok((transactions, senders))
    }
}

/// Where a 0x50 transaction's sender came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SenderSource {
    /// The admission cache.
    Hit = 0,
    /// A frame this node holds.
    Frame = 1,
    /// Verified here.
    Miss = 2,
}

impl SenderSource {
    fn counter(self) -> &'static AtomicU64 {
        match self {
            Self::Hit => &SENDER_HIT,
            Self::Frame => &SENDER_FRAME,
            Self::Miss => &SENDER_MISS,
        }
    }
}

/// One payload transaction decoded from its bytes with its sender, and where
/// a 0x50 transaction's sender came from.
fn recover_payload_transaction(
    sender_recovery_cache: Option<&SenderRecoveryCache>,
    tx: &[u8],
) -> Result<(Recovered<N42TxEnvelope>, Option<SenderSource>), AnyError> {
    let tx = N42TxEnvelope::decode_2718_exact(tx).map_err(AnyError::new)?;
    let (signer, source) = match &tx {
        // Ed25519 senders verified at admission are reused; anything
        // else is verified here and remembered.
        N42TxEnvelope::AltSig(alt) => {
            let cache = AltSigSenderCache::global();
            match cache.get(alt.hash()) {
                Some(sender) => (Ok(sender), Some(SenderSource::Hit)),
                None => {
                    // The cache drops entries under load; a frame this
                    // node admitted and still holds has the sender too.
                    let held = n42_tx_types::frame_ref_blocks_enabled()
                        .then(|| n42_tx_types::FrameStore::global().sender_of(alt.hash()))
                        .flatten();
                    if let Some(sender) = held {
                        (Ok(sender), Some(SenderSource::Frame))
                    } else {
                        (
                            tx.try_recover().inspect(|sender| cache.insert(*alt.hash(), *sender)),
                            Some(SenderSource::Miss),
                        )
                    }
                }
            }
        }
        N42TxEnvelope::Eth(_) => (
            match sender_recovery_cache {
                Some(cache) => cache.recover(&tx),
                None => tx.try_recover(),
            },
            None,
        ),
    };
    let signer = signer.map_err(AnyError::new)?;
    Ok((tx.with_signer(signer), source))
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

    /// A full block's payload recovery, timed: `N42_BENCH_TXS` 0x50
    /// transfers (95,238 by default) whose senders the admission cache holds,
    /// once as slices of one buffer and once as a buffer each.
    ///
    /// `N42_ALTSIG_SENDER_CACHE=4194304 cargo test --release -p n42-execution --lib payload_recovery_of_a_full_block -- --ignored --nocapture`
    #[test]
    #[ignore = "timing run, not a check"]
    fn payload_recovery_of_a_full_block() {
        use alloy_eips::eip2718::Encodable2718;
        use alloy_primitives::{Address, Bytes};
        let count: u64 =
            std::env::var("N42_BENCH_TXS").ok().and_then(|v| v.parse().ok()).unwrap_or(95_238);
        let cache = AltSigSenderCache::global();
        let mut buffer = Vec::new();
        let mut bounds = Vec::new();
        for i in 0..count {
            let mut seed = [0x42u8; 32];
            seed[..8].copy_from_slice(&i.to_be_bytes());
            let key = ed25519_dalek::SigningKey::from_bytes(&seed);
            let tx = n42_tx_types::TxAltSig {
                    chain_id: N42_CHAIN_ID,
                    nonce: 0,
                    max_priority_fee_per_gas: 1,
                    max_fee_per_gas: 100,
                    gas_limit: 21_000,
                    to: Address::left_padding_from(&(0x100_0000 + i).to_be_bytes()),
                    value: U256::from(1_000u64),
                    input: Bytes::new(),
                    access_list: Default::default(),
                    alg_type: n42_tx_types::ALG_ED25519,
                    pubkey: Bytes::copy_from_slice(key.verifying_key().as_bytes()),
            }
            .sign_ed25519(&key);
            cache.insert(*tx.hash(), tx.tx().sender());
            let start = buffer.len();
            N42TxEnvelope::AltSig(tx).encode_2718(&mut buffer);
            bounds.push(start..buffer.len());
        }
        let buffer = Bytes::from(buffer);
        let shared: Vec<Bytes> = bounds.iter().map(|range| buffer.slice(range.clone())).collect();
        let apart: Vec<Bytes> = shared.iter().map(|tx| Bytes::copy_from_slice(tx)).collect();
        let config = N42EvmConfig::new(n42_dev_chainspec());
        for (name, transactions) in [("one buffer", shared), ("a buffer each", apart)] {
            let mut input = payload(Vec::new());
            input.payload.as_v1_mut().transactions = transactions;
            let mut times = Vec::new();
            for _ in 0..14 {
                let at = std::time::Instant::now();
                let (txs, senders) = config.recover_payload_transactions(&input).expect("recovered");
                times.push(at.elapsed().as_secs_f64() * 1e3);
                assert_eq!((txs.len() as u64, senders.len() as u64), (count, count));
                assert_eq!(senders[1], txs[1].as_alt_sig().expect("0x50").tx().sender());
            }
            times.drain(..2);
            times.sort_by(f64::total_cmp);
            println!(
                "PAYLOAD_RECOVERY {name}: txs={count} threads={} median_ms={:.1} min_ms={:.1} max_ms={:.1}",
                rayon::current_num_threads(),
                times[times.len() / 2],
                times[0],
                times[times.len() - 1],
            );
        }
        // The cache is direct-mapped: a few of a block's entries displace
        // each other and are verified again.
        println!(
            "PAYLOAD_RECOVERY senders: hit={} miss={}",
            SENDER_HIT.load(Ordering::Relaxed),
            SENDER_MISS.load(Ordering::Relaxed)
        );
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
