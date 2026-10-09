//! Node-side compact-block execution-output cache adapter. The
//! `ExecutionOutputCache` trait lives in `n42-consensus-service`; this module
//! provides the in-process adapter [`RethExecutionOutputCache`] + the free
//! functions over reth's global `reth_evm::payload_cache` (Caplin stage 6a-2 / 6).

use alloy_primitives::{Address, B256};
use n42_consensus_service::orchestrator::{
    CompactBlockExecution, compress_payload, decompress_payload,
};
use reth_execution_types::{BlockExecutionOutput, BlockExecutionResult};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

pub use n42_consensus_service::exec_cache::ExecutionOutputCache;

/// In-process adapter over reth's global `reth_evm::payload_cache`.
pub struct RethExecutionOutputCache {
    qmdb_store: Option<Arc<crate::qmdb_state_root::Gov5QmdbStateRootStore>>,
    chain_spec: Arc<reth_chainspec::ChainSpec>,
}

impl RethExecutionOutputCache {
    pub const fn new(
        qmdb_store: Option<Arc<crate::qmdb_state_root::Gov5QmdbStateRootStore>>,
        chain_spec: Arc<reth_chainspec::ChainSpec>,
    ) -> Self {
        Self {
            qmdb_store,
            chain_spec,
        }
    }
}

impl ExecutionOutputCache for RethExecutionOutputCache {
    fn take_serialized(&self, hash: B256) -> Option<Vec<u8>> {
        take_and_serialize_execution_output(&hash)
    }

    fn take_gov5_normalization(
        &self,
        execution: &alloy_rpc_types_engine::ExecutionData,
    ) -> Option<(B256, B256)> {
        take_gov5_normalization_roots(execution, self.qmdb_store.as_deref(), &self.chain_spec)
    }

    fn rekey_gov5_normalized(
        &self,
        original: &alloy_rpc_types_engine::ExecutionData,
        normalized: &alloy_rpc_types_engine::ExecutionData,
    ) -> bool {
        if !n42_network::gov5_normalization_preserves_execution_input(original, normalized) {
            return false;
        }
        let old_hash = original.block_hash();
        let new_hash = normalized.block_hash();
        if old_hash == new_hash {
            return true;
        }
        let Some(output) =
            reth_evm::payload_cache::take_payload_execution::<CachedPayloadData>(&old_hash)
        else {
            return false;
        };
        let transactions_root = reth_evm::payload_cache::payload_transactions_root(&old_hash);
        reth_evm::payload_cache::remove_payload_transactions_root(&old_hash);
        if let Some(root) = transactions_root {
            reth_evm::payload_cache::store_payload_transactions_root(new_hash, root);
        }
        reth_evm::payload_cache::store_payload_execution(new_hash, output);
        metrics::counter!("n42_gov5_normalized_execution_cache_rekeys_total").increment(1);
        true
    }

    fn inject(&self, hash: B256, compressed: &[u8], source: &'static str) -> bool {
        inject_compact_block(&hash, compressed, source)
    }

    fn evict(&self, hash: B256) {
        let removed =
            reth_evm::payload_cache::take_payload_execution::<CachedPayloadData>(&hash).is_some();
        reth_evm::payload_cache::remove_payload_transactions_root(&hash);
        metrics::counter!(
            "n42_compact_block_cache_evictions_total",
            "removed" => if removed { "true" } else { "false" }
        )
        .increment(1);
        if removed {
            warn!(
                target: "n42::cl::exec_bridge",
                %hash,
                "evicted rejected compact execution output"
            );
        }
    }
}

type CachedPayloadData = (
    BlockExecutionOutput<reth_ethereum_primitives::Receipt>,
    Vec<Address>,
);

#[derive(Default)]
struct CompactInjectTracker {
    order: VecDeque<B256>,
    counts: HashMap<B256, u64>,
}

const COMPACT_INJECT_TRACKER_LIMIT: usize = 2048;

fn observe_compact_inject_attempt(hash: B256, source: &'static str) -> Option<u64> {
    static TRACKER: std::sync::OnceLock<Mutex<CompactInjectTracker>> = std::sync::OnceLock::new();
    let tracker = TRACKER.get_or_init(|| Mutex::new(CompactInjectTracker::default()));
    let mut tracker = tracker.lock().unwrap_or_else(|e| {
        tracing::warn!("compact_inject_tracker mutex poisoned, recovering");
        e.into_inner()
    });

    if let Some(seen) = tracker.counts.get_mut(&hash) {
        *seen += 1;
        let duplicate_attempt = *seen;
        metrics::counter!("n42_compact_inject_duplicate_total").increment(1);
        info!(
            target: "n42::cl::exec_bridge",
            %hash,
            source,
            duplicate_attempt,
            "N42_COMPACT_INJECT_DUP: repeated compact inject attempt"
        );
        return Some(duplicate_attempt);
    }

    tracker.counts.insert(hash, 1);
    tracker.order.push_back(hash);
    if tracker.order.len() > COMPACT_INJECT_TRACKER_LIMIT
        && let Some(evicted) = tracker.order.pop_front()
    {
        tracker.counts.remove(&evicted);
    }
    None
}

/// Take execution output from broadcast cache and serialize it for followers.
pub(crate) fn take_and_serialize_execution_output(hash: &B256) -> Option<Vec<u8>> {
    let (output, senders) =
        reth_evm::payload_cache::take_broadcast_execution::<CachedPayloadData>(hash)?;
    serialize_execution_output(hash, output, senders)
}

fn serialize_execution_output(
    hash: &B256,
    output: BlockExecutionOutput<reth_ethereum_primitives::Receipt>,
    senders: Vec<Address>,
) -> Option<Vec<u8>> {
    let ser_start = std::time::Instant::now();
    let transactions_root = reth_evm::payload_cache::payload_transactions_root(hash);
    let compact = CompactBlockExecution {
        bundle_state: output.state,
        receipts: output.result.receipts,
        requests: output.result.requests,
        gas_used: output.result.gas_used,
        blob_gas_used: output.result.blob_gas_used,
        senders,
        transactions_root,
    };
    match serde_json::to_vec(&compact) {
        Ok(serialized) => {
            let compressed = compress_payload(&serialized);
            let ser_ms = ser_start.elapsed().as_millis() as u64;
            info!(
                target: "n42::cl::exec_bridge",
                %hash,
                raw_kb = serialized.len() / 1024,
                compressed_kb = compressed.len() / 1024,
                ser_ms,
                "N42_COMPACT_BLOCK: execution output serialized for broadcast"
            );
            metrics::counter!("n42_compact_block_serialized").increment(1);
            metrics::histogram!("n42_compact_block_size_bytes").record(compressed.len() as f64);
            Some(compressed)
        }
        Err(e) => {
            warn!(target: "n42::cl::exec_bridge", %hash, error = %e, "compact block: failed to serialize execution output");
            None
        }
    }
}

/// Consume the builder's broadcast copy once and bind Gov5's native receipt
/// commitment to exactly that execution output. The normalized H2 payload is
/// subsequently re-executed; no compact execution blob is used on this path.
fn take_gov5_normalization_roots(
    execution: &alloy_rpc_types_engine::ExecutionData,
    qmdb_store: Option<&crate::qmdb_state_root::Gov5QmdbStateRootStore>,
    chain_spec: &reth_chainspec::ChainSpec,
) -> Option<(B256, B256)> {
    let hash = &execution.block_hash();
    let parent_hash = execution.parent_hash();
    let (output, _senders) =
        reth_evm::payload_cache::take_broadcast_execution::<CachedPayloadData>(hash)?;
    let receipts_root = n42_network::gov5_native_receipts_root(&output.result.receipts);
    let payload = execution.payload.as_v1();
    let key = n42_execution::restored_slots_key(
        parent_hash,
        payload.transactions.iter().map(alloy_primitives::keccak256),
    );
    let restored = n42_execution::restored_slots_for(key).unwrap_or_default();
    let mut operations =
        crate::qmdb_state::gov5_qmdb_operations_with_restored(&output.state, &restored);
    if reth_chainspec::EthereumHardforks::is_prague_active_at_timestamp(
        chain_spec,
        payload.timestamp,
    ) {
        crate::qmdb_state::with_gov5_prague_system_caller(&mut operations);
    }
    // Own the ordering buffer here, before entering the tree lock. The tree
    // can borrow this canonical sequence while the operations remain ours.
    operations.sort_unstable_by_key(|operation| operation.key);
    let state_root = match qmdb_store {
        Some(store) => match store.prepare_candidate(parent_hash, operations) {
            Ok(root) => root,
            Err(error) => {
                warn!(
                    target: "n42::interop::h2v4",
                    %hash,
                    %parent_hash,
                    %error,
                    "failed to derive Gov5 QMDB root for locally built payload"
                );
                return None;
            }
        },
        None => {
            warn!(
                target: "n42::interop::h2v4",
                %hash,
                "Gov5 normalization requires an authenticated QMDB state-root store"
            );
            return None;
        }
    };
    Some((state_root, receipts_root))
}

/// Deserialize compact block execution output and load it into the payload cache.
pub(crate) fn inject_compact_block(hash: &B256, compressed: &[u8], source: &'static str) -> bool {
    let duplicate_attempt = observe_compact_inject_attempt(*hash, source);
    let inject_start = std::time::Instant::now();

    let decompress_start = std::time::Instant::now();
    let decompressed = match decompress_payload(compressed) {
        Ok(d) => d,
        Err(e) => {
            warn!(target: "n42::cl::exec_bridge", %hash, error = %e, "compact block: failed to decompress");
            return false;
        }
    };
    let decompress_ms = decompress_start.elapsed().as_millis() as u64;

    let deser_start = std::time::Instant::now();
    let compact: CompactBlockExecution = match serde_json::from_slice(&decompressed) {
        Ok(c) => c,
        Err(e) => {
            warn!(target: "n42::cl::exec_bridge", %hash, error = %e, "compact block: failed to deserialize");
            return false;
        }
    };
    let deser_ms = deser_start.elapsed().as_millis() as u64;

    let store_start = std::time::Instant::now();
    let output = BlockExecutionOutput {
        state: compact.bundle_state,
        result: BlockExecutionResult {
            receipts: compact.receipts,
            requests: compact.requests,
            gas_used: compact.gas_used,
            blob_gas_used: compact.blob_gas_used,
        },
    };
    if let Some(transactions_root) = compact.transactions_root {
        reth_evm::payload_cache::store_payload_transactions_root(*hash, transactions_root);
    }
    reth_evm::payload_cache::store_payload_execution(*hash, (output, compact.senders));
    let store_ms = store_start.elapsed().as_millis() as u64;

    let total_ms = inject_start.elapsed().as_millis() as u64;
    info!(target: "n42::cl::exec_bridge", %hash,
        source,
        duplicate_attempt = duplicate_attempt.unwrap_or_default(),
        compressed_kb = compressed.len() / 1024,
        decompressed_kb = decompressed.len() / 1024,
        decompress_ms, deser_ms, store_ms, total_ms,
        "N42_COMPACT_INJECT: execution output injected into payload cache");
    metrics::counter!("n42_compact_block_cache_injected").increment(1);
    metrics::histogram!("n42_compact_inject_ms").record(total_ms as f64);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reth's broadcast cache is one process-global slot.
    static CACHE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn transaction_root_survives_compact_round_trip_and_is_evicted_by_hash() {
        let _lock = CACHE_TEST_LOCK.lock().unwrap();
        let hash = B256::repeat_byte(0x42);
        let other_hash = B256::repeat_byte(0x43);
        let root = B256::repeat_byte(0x44);
        let cache = RethExecutionOutputCache::new(None, reth_chainspec::MAINNET.clone());

        reth_evm::payload_cache::store_payload_transactions_root(hash, root);
        reth_evm::payload_cache::store_broadcast_execution(
            hash,
            (
                BlockExecutionOutput::<reth_ethereum_primitives::Receipt>::default(),
                Vec::<Address>::new(),
            ),
        );
        let compressed = cache
            .take_serialized(hash)
            .expect("cached execution output");
        let compact: CompactBlockExecution =
            serde_json::from_slice(&decompress_payload(&compressed).unwrap()).unwrap();
        assert_eq!(compact.transactions_root, Some(root));

        reth_evm::payload_cache::remove_payload_transactions_root(&hash);
        assert!(cache.inject(hash, &compressed, "test"));
        assert_eq!(
            reth_evm::payload_cache::payload_transactions_root(&hash),
            Some(root)
        );
        assert_eq!(
            reth_evm::payload_cache::payload_transactions_root(&other_hash),
            None
        );

        cache.evict(other_hash);
        assert_eq!(
            reth_evm::payload_cache::payload_transactions_root(&hash),
            Some(root)
        );
        cache.evict(hash);
        assert_eq!(
            reth_evm::payload_cache::payload_transactions_root(&hash),
            None
        );
        assert!(
            reth_evm::payload_cache::take_payload_execution::<CachedPayloadData>(&hash).is_none()
        );
    }

    #[test]
    fn native_gov5_normalization_rekeys_exact_builder_output() {
        use alloy_consensus::proofs::calculate_transaction_root;
        use n42_consensus::Gov5NativeHeader;

        let _lock = CACHE_TEST_LOCK.lock().unwrap();
        let parent = Gov5NativeHeader {
            header: alloy_consensus::Header {
                number: 0,
                withdrawals_root: Some(alloy_consensus::constants::EMPTY_ROOT_HASH),
                blob_gas_used: Some(0),
                excess_blob_gas: Some(0),
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
            mobile_registry_root: None,
        };
        let parent_hash = n42_consensus::remember_gov5_native_header(&parent.encode());
        let mut block = reth_ethereum_primitives::Block::default();
        block.header.parent_hash = parent_hash;
        block.header.number = 1;
        block.header.base_fee_per_gas = Some(0);
        block.header.blob_gas_used = Some(0);
        block.header.excess_blob_gas = Some(0);
        block.header.parent_beacon_block_root = Some(B256::ZERO);
        block.header.withdrawals_root = Some(alloy_consensus::constants::EMPTY_ROOT_HASH);
        block.header.transactions_root =
            calculate_transaction_root::<alloy_consensus::TxEnvelope>(&[]);
        block.body.withdrawals = Some(Default::default());
        let old_hash = block.header.hash_slow();
        let original =
            alloy_rpc_types_engine::ExecutionData::from_block_unchecked(old_hash, &block);
        let normalized = n42_network::normalize_execution_payload_for_gov5_h2(
            &original,
            1,
            B256::repeat_byte(0xa1),
            n42_network::gov5_native_receipts_root(&[]),
        )
        .unwrap();
        let new_hash = normalized.block_hash();
        assert_ne!(old_hash, new_hash);

        let cache = RethExecutionOutputCache::new(None, reth_chainspec::MAINNET.clone());
        let root = block.header.transactions_root;
        reth_evm::payload_cache::store_payload_transactions_root(old_hash, root);
        reth_evm::payload_cache::store_payload_execution(
            old_hash,
            (
                BlockExecutionOutput::<reth_ethereum_primitives::Receipt>::default(),
                Vec::<Address>::new(),
            ),
        );
        assert!(cache.rekey_gov5_normalized(&original, &normalized));
        assert_eq!(
            reth_evm::payload_cache::payload_transactions_root(&old_hash),
            None
        );
        assert_eq!(
            reth_evm::payload_cache::payload_transactions_root(&new_hash),
            Some(root)
        );
        assert!(
            reth_evm::payload_cache::take_payload_execution::<CachedPayloadData>(&old_hash)
                .is_none()
        );
        assert!(
            reth_evm::payload_cache::take_payload_execution::<CachedPayloadData>(&new_hash)
                .is_some()
        );
        cache.evict(new_hash);
        reth_evm::payload_cache::store_payload_execution(
            old_hash,
            (
                BlockExecutionOutput::<reth_ethereum_primitives::Receipt>::default(),
                Vec::<Address>::new(),
            ),
        );
        let mut changed = normalized.clone();
        changed.payload.as_v1_mut().timestamp += 1;
        assert!(!cache.rekey_gov5_normalized(&original, &changed));
        assert!(
            reth_evm::payload_cache::take_payload_execution::<CachedPayloadData>(&old_hash)
                .is_some()
        );
    }

    #[test]
    fn gov5_normalization_consumes_exact_output_and_only_prices_roots() {
        use crate::qmdb_state_root::Gov5QmdbStateRootStore;
        use alloy_primitives::U256;
        use n42_twig_core::qmdb_compat::{
            QmdbCompatTree, encode_gov5_account_value, gov5_account_key,
        };
        use revm::{
            database::states::{AccountStatus, BundleAccount},
            state::AccountInfo,
        };
        let _lock = CACHE_TEST_LOCK.lock().unwrap();
        let base = QmdbCompatTree::new();
        let base_hash = B256::repeat_byte(0x61);
        let base_root = B256::from(base.root());
        let store =
            Arc::new(Gov5QmdbStateRootStore::new(base_hash, base_root, base.snapshot()).unwrap());
        let cache =
            RethExecutionOutputCache::new(Some(store.clone()), reth_chainspec::MAINNET.clone());
        let mut block = reth_ethereum_primitives::Block::default();
        block.header.parent_hash = base_hash;
        block.header.number = 1;
        let hash = block.header.hash_slow();
        let execution = alloy_rpc_types_engine::ExecutionData::from_block_unchecked(hash, &block);
        let address = Address::repeat_byte(0x62);
        let mut output = BlockExecutionOutput::<reth_ethereum_primitives::Receipt>::default();
        output.state.state.insert(
            address,
            BundleAccount::new(
                None,
                Some(AccountInfo {
                    nonce: 1,
                    balance: U256::from(1234),
                    ..Default::default()
                }),
                Default::default(),
                AccountStatus::Changed,
            ),
        );
        output.result.receipts.push(Default::default());
        let receipts_root = n42_network::gov5_native_receipts_root(&output.result.receipts);
        let mut oracle = base;
        oracle.set(
            gov5_account_key(address.as_ref()),
            encode_gov5_account_value(
                1,
                &U256::from(1234).to_be_bytes(),
                &alloy_primitives::KECCAK256_EMPTY.0,
            ),
        );
        let expected = (B256::from(oracle.root()), receipts_root);
        reth_evm::payload_cache::store_broadcast_execution(hash, (output, vec![address]));

        // A sibling's hash cannot consume this execution output.
        let sibling = alloy_rpc_types_engine::ExecutionData::from_block_unchecked(
            B256::repeat_byte(0x63),
            &block,
        );
        assert!(cache.take_gov5_normalization(&sibling).is_none());
        assert_eq!(cache.take_gov5_normalization(&execution), Some(expected));
        assert!(cache.take_gov5_normalization(&execution).is_none());
        assert!(!store.contains(hash).unwrap());
        assert_eq!(store.root_for(base_hash).unwrap(), Some(base_root));
        assert_eq!(store.compute_candidate(base_hash, &[]).unwrap(), base_root);
    }
}
