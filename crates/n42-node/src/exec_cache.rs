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
    accept_peer_outputs: bool,
    shared_execution: bool,
}

impl RethExecutionOutputCache {
    pub const fn new(
        qmdb_store: Option<Arc<crate::qmdb_state_root::Gov5QmdbStateRootStore>>,
        chain_spec: Arc<reth_chainspec::ChainSpec>,
    ) -> Self {
        Self {
            qmdb_store,
            chain_spec,
            accept_peer_outputs: true,
            shared_execution: false,
        }
    }

    /// Lets this node take the execution output of a co-located leader on the
    /// QMDB route (`N42_SHARED_EXECUTION`). The output's QMDB root and receipts
    /// root are still compared with the header; see
    /// `n42_consensus_service::orchestrator::shared_execution_enabled`. It has
    /// no effect without a QMDB store: the trusted-root mode compares no root.
    pub fn with_shared_execution(mut self, shared: bool) -> Self {
        self.shared_execution = shared;
        self
    }

    /// Controls whether peer-supplied compact outputs may enter the payload cache.
    ///
    /// Must be false whenever the Gov5 QMDB store (or the trusted-root mode) is
    /// authoritative. No honest node emits a compact output there, the output
    /// is unauthenticated, and the trusted-root job never compares a root, so
    /// a forged bundle would be committed as the block's state.
    pub fn with_peer_outputs(mut self, accept: bool) -> Self {
        self.accept_peer_outputs = accept;
        self
    }
}

impl ExecutionOutputCache for RethExecutionOutputCache {
    fn take_serialized(&self, hash: B256) -> Option<Vec<u8>> {
        take_and_serialize_execution_output(&hash)
    }

    fn inject_for_payload(
        &self,
        execution: &alloy_rpc_types_engine::ExecutionData,
        compressed: &[u8],
        source: &'static str,
    ) -> bool {
        if self.qmdb_store.is_none() {
            return self.inject(execution.block_hash(), compressed, source);
        }
        if !self.shared_execution {
            metrics::counter!("n42_compact_block_peer_output_refused_total").increment(1);
            return false;
        }
        let chain_spec = &self.chain_spec;
        inject_compact_block_with(&execution.block_hash(), compressed, source, |slots| {
            // The key is derived from the payload in hand, never taken from
            // the wire, so a list can only be filed for the block it came with.
            let Some(slots) = slots else {
                return Err("shared execution output without restored slots");
            };
            let key = gov5_restored_slots_key_for_payload(execution, chain_spec, None)
                .ok_or("payload has no execution environment")?;
            n42_execution::record_restored_slots(key, slots);
            Ok(())
        })
    }

    fn take_gov5_normalization(
        &self,
        execution: &alloy_rpc_types_engine::ExecutionData,
    ) -> Option<(B256, B256)> {
        take_gov5_normalization_roots(execution, self.qmdb_store.as_deref(), &self.chain_spec)
    }

    fn built_transactions_root(&self, hash: B256) -> Option<B256> {
        reth_evm::payload_cache::payload_transactions_root(&hash)
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
        // Shared execution keeps a broadcast copy under the builder's hash;
        // followers know the block by its normalized hash.
        if let Some(shared) =
            reth_evm::payload_cache::take_broadcast_execution::<CachedPayloadData>(&old_hash)
        {
            reth_evm::payload_cache::store_broadcast_execution(new_hash, shared);
            let mut restored = SHARED_RESTORED_SLOTS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((hash, _)) = restored.as_mut()
                && *hash == old_hash
            {
                *hash = new_hash;
            }
        }
        metrics::counter!("n42_gov5_normalized_execution_cache_rekeys_total").increment(1);
        true
    }

    fn inject(&self, hash: B256, compressed: &[u8], source: &'static str) -> bool {
        if !self.accept_peer_outputs {
            metrics::counter!("n42_compact_block_peer_output_refused_total").increment(1);
            return false;
        }
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

type CachedPayloadData = (BlockExecutionOutput<n42_tx_types::Receipt>, Vec<Address>);

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
    output: BlockExecutionOutput<n42_tx_types::Receipt>,
    senders: Vec<Address>,
) -> Option<Vec<u8>> {
    let ser_start = std::time::Instant::now();
    let transactions_root = reth_evm::payload_cache::payload_transactions_root(hash);
    let restored_slots = SHARED_RESTORED_SLOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take_if(|(shared_hash, _)| shared_hash == hash)
        .map(|(_, slots)| slots);
    let compact = CompactBlockExecution {
        bundle_state: output.state,
        receipts: output.result.receipts,
        requests: output.result.requests,
        gas_used: output.result.gas_used,
        blob_gas_used: output.result.blob_gas_used,
        senders,
        transactions_root,
        restored_slots,
    };
    match compact.encode() {
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

/// The restored slots of the block whose shared execution output is waiting
/// to be serialized, by block hash. One entry, like reth's broadcast slot.
static SHARED_RESTORED_SLOTS: Mutex<Option<(B256, Vec<n42_execution::RestoredSlot>)>> =
    Mutex::new(None);

/// The restored-slots registry key of a payload: parent, execution
/// environment, beacon root, withdrawals and ordered transaction hashes.
/// `tx_hashes` saves hashing when the caller already has them.
fn gov5_restored_slots_key_for_payload(
    execution: &alloy_rpc_types_engine::ExecutionData,
    chain_spec: &Arc<reth_chainspec::ChainSpec>,
    tx_hashes: Option<Vec<B256>>,
) -> Option<B256> {
    use reth_evm::ConfigureEngineEvm;
    let config = n42_execution::N42EvmConfig::new(Arc::clone(chain_spec));
    let env = config.evm_env_for_payload(execution).ok()?;
    let ctx = config.context_for_payload(execution).ok()?;
    let context = n42_execution::restored_slots_execution_key(
        execution.parent_hash(),
        n42_execution::restored_slots_env_key(&env.block_env),
        ctx.parent_beacon_block_root,
        ctx.withdrawals.as_deref(),
        [],
    );
    let tx_hashes = tx_hashes.unwrap_or_else(|| {
        use rayon::prelude::*;
        execution
            .payload
            .as_v1()
            .transactions
            .par_iter()
            .map(alloy_primitives::keccak256)
            .collect()
    });
    Some(n42_execution::restored_slots_key(context, tx_hashes))
}

/// Consume the builder's broadcast copy once and bind Gov5's native receipt
/// commitment to exactly that execution output. Followers execute the
/// normalized H2 payload themselves unless shared execution is on, in which
/// case the copy is kept for `take_serialized` together with the block's
/// restored slots.
fn take_gov5_normalization_roots(
    execution: &alloy_rpc_types_engine::ExecutionData,
    qmdb_store: Option<&crate::qmdb_state_root::Gov5QmdbStateRootStore>,
    chain_spec: &Arc<reth_chainspec::ChainSpec>,
) -> Option<(B256, B256)> {
    let hash = &execution.block_hash();
    let parent_hash = execution.parent_hash();
    let (output, _senders) =
        reth_evm::payload_cache::take_broadcast_execution::<CachedPayloadData>(hash)?;
    let started = std::time::Instant::now();
    let payload = execution.payload.as_v1();
    // The receipts root and the transaction hashes are independent of each
    // other and of the tree; on a full block they are the serial part of this
    // step, so they run side by side on the rayon pool.
    let (receipts_root, tx_hashes) = rayon::join(
        || n42_network::gov5_native_receipts_root(&output.result.receipts),
        || {
            use rayon::prelude::*;
            payload
                .transactions
                .par_iter()
                .map(alloy_primitives::keccak256)
                .collect::<Vec<_>>()
        },
    );
    let hashed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let key = gov5_restored_slots_key_for_payload(execution, chain_spec, Some(tx_hashes))?;
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
    let operations_ms = started.elapsed().as_secs_f64() * 1000.0 - hashed_ms;
    let operation_count = operations.len();
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
    if n42_consensus_service::orchestrator::shared_execution_enabled() {
        *SHARED_RESTORED_SLOTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((*hash, restored.as_ref().clone()));
        reth_evm::payload_cache::store_broadcast_execution(*hash, (output, _senders));
    }
    let total_ms = started.elapsed().as_secs_f64() * 1000.0;
    metrics::histogram!("n42_gov5_normalization_roots_ms").record(total_ms);
    tracing::info!(
        target: "n42::interop::h2v4",
        %hash,
        tx_count = payload.transactions.len(),
        operation_count,
        hashed_ms,
        operations_ms,
        root_ms = total_ms - hashed_ms - operations_ms,
        total_ms,
        "N42_GOV5_NORMALIZATION_ROOTS: leader roots for the built payload"
    );
    Some((state_root, receipts_root))
}

/// Checks the parts of a peer's compact output that reth's cached-output path
/// never compares against the block. The state root commits to code hashes, not
/// code, and every `contracts` entry is written to the bytecode table: a wrong
/// body would overwrite the code of an existing contract on this node for good.
/// Reth also adopts `senders` without a length check; receipts are bound to the
/// transaction count by the receipts root. A refusal only costs re-execution.
fn validate_compact_structure(compact: &CompactBlockExecution) -> Result<(), &'static str> {
    if compact.senders.len() != compact.receipts.len() {
        return Err("sender count differs from receipt count");
    }
    if compact
        .bundle_state
        .contracts
        .iter()
        .any(|(hash, code)| code.hash_slow() != *hash)
    {
        return Err("bytecode does not match its code hash");
    }
    Ok(())
}

/// Deserialize compact block execution output and load it into the payload cache.
pub(crate) fn inject_compact_block(hash: &B256, compressed: &[u8], source: &'static str) -> bool {
    inject_compact_block_with(hash, compressed, source, |_| Ok(()))
}

/// [`inject_compact_block`], handing the output's restored slots to
/// `restored` once the structure checks passed and before anything is stored.
/// A refusal from `restored` refuses the whole output.
fn inject_compact_block_with(
    hash: &B256,
    compressed: &[u8],
    source: &'static str,
    restored: impl FnOnce(Option<Vec<n42_execution::RestoredSlot>>) -> Result<(), &'static str>,
) -> bool {
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
    let compact = match CompactBlockExecution::decode(&decompressed) {
        Ok(c) => c,
        Err(e) => {
            warn!(target: "n42::cl::exec_bridge", %hash, error = %e, "compact block: failed to deserialize");
            return false;
        }
    };
    let deser_ms = deser_start.elapsed().as_millis() as u64;

    if let Err(reason) = validate_compact_structure(&compact) {
        metrics::counter!("n42_compact_block_structure_rejects_total").increment(1);
        warn!(target: "n42::cl::exec_bridge", %hash, source, reason, "compact block: refusing peer output");
        return false;
    }

    if let Err(reason) = restored(compact.restored_slots) {
        metrics::counter!("n42_compact_block_structure_rejects_total").increment(1);
        warn!(target: "n42::cl::exec_bridge", %hash, source, reason, "compact block: refusing peer output");
        return false;
    }

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
    fn shared_output_carries_restored_slots_to_the_follower_or_is_refused() {
        let _lock = CACHE_TEST_LOCK.lock().unwrap();
        let hash = B256::repeat_byte(0x61);
        let slots = vec![(
            Address::repeat_byte(7),
            alloy_primitives::U256::from(1),
            alloy_primitives::U256::from(2),
        )];
        let output = || {
            (
                BlockExecutionOutput::<n42_tx_types::Receipt>::default(),
                Vec::<Address>::new(),
            )
        };

        // Leader: a list filed for another block is not attached.
        *SHARED_RESTORED_SLOTS.lock().unwrap() = Some((B256::repeat_byte(0x62), slots.clone()));
        reth_evm::payload_cache::store_broadcast_execution(hash, output());
        let other = take_and_serialize_execution_output(&hash).unwrap();
        let other = CompactBlockExecution::decode(&decompress_payload(&other).unwrap()).unwrap();
        assert_eq!(other.restored_slots, None);

        // Leader: the block's own list travels with its output, once.
        *SHARED_RESTORED_SLOTS.lock().unwrap() = Some((hash, slots.clone()));
        reth_evm::payload_cache::store_broadcast_execution(hash, output());
        let shared = take_and_serialize_execution_output(&hash).unwrap();
        assert!(SHARED_RESTORED_SLOTS.lock().unwrap().is_none());

        // Follower: the list reaches the caller before the output is stored,
        // and a refusal there keeps the output out of the payload cache.
        reth_evm::payload_cache::take_payload_execution::<CachedPayloadData>(&hash);
        assert!(!inject_compact_block_with(&hash, &shared, "test", |_| Err("refused")));
        assert!(
            reth_evm::payload_cache::take_payload_execution::<CachedPayloadData>(&hash).is_none()
        );
        let mut seen = None;
        assert!(inject_compact_block_with(&hash, &shared, "test", |restored| {
            seen = restored;
            Ok(())
        }));
        assert_eq!(seen, Some(slots));
        assert!(
            reth_evm::payload_cache::take_payload_execution::<CachedPayloadData>(&hash).is_some()
        );

        // Without a QMDB store the payload-aware entry point is the plain one.
        let cache = RethExecutionOutputCache::new(None, reth_chainspec::MAINNET.clone())
            .with_peer_outputs(false)
            .with_shared_execution(true);
        assert!(!cache.inject(hash, &shared, "test"));
    }

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
                BlockExecutionOutput::<n42_tx_types::Receipt>::default(),
                Vec::<Address>::new(),
            ),
        );
        let compressed = cache
            .take_serialized(hash)
            .expect("cached execution output");
        let compact =
            CompactBlockExecution::decode(&decompress_payload(&compressed).unwrap()).unwrap();
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
    fn peer_output_is_refused_when_gov5_state_is_authoritative() {
        let _lock = CACHE_TEST_LOCK.lock().unwrap();
        let hash = B256::repeat_byte(0x45);
        let root = B256::repeat_byte(0x46);
        let compressed = compress_payload(
            &serde_json::to_vec(&CompactBlockExecution {
                bundle_state: Default::default(),
                receipts: Vec::new(),
                requests: Default::default(),
                gas_used: 0,
                blob_gas_used: 0,
                senders: Vec::new(),
                transactions_root: Some(root),
                restored_slots: None,
            })
            .unwrap(),
        );

        let cache = RethExecutionOutputCache::new(None, reth_chainspec::MAINNET.clone())
            .with_peer_outputs(false);
        assert!(!cache.inject(hash, &compressed, "test"));
        assert!(
            reth_evm::payload_cache::take_payload_execution::<CachedPayloadData>(&hash).is_none()
        );
        assert_eq!(
            reth_evm::payload_cache::payload_transactions_root(&hash),
            None
        );

        let cache = RethExecutionOutputCache::new(None, reth_chainspec::MAINNET.clone());
        assert!(cache.inject(hash, &compressed, "test"));
        cache.evict(hash);
    }

    #[test]
    fn compact_output_with_unbound_bytecode_or_senders_is_refused() {
        use alloy_primitives::{Bytes, keccak256};
        use revm::{bytecode::Bytecode, database::BundleState};

        let _lock = CACHE_TEST_LOCK.lock().unwrap();
        let hash = B256::repeat_byte(0x47);
        let code = Bytes::from_static(&[0x60, 0x00, 0x00]);
        let compact = |contract_hash: B256, senders: Vec<Address>| {
            let mut bundle_state = BundleState::default();
            bundle_state
                .contracts
                .insert(contract_hash, Bytecode::new_raw(code.clone()));
            compress_payload(
                &serde_json::to_vec(&CompactBlockExecution {
                    bundle_state,
                    receipts: Vec::new(),
                    requests: Default::default(),
                    gas_used: 0,
                    blob_gas_used: 0,
                    senders,
                    transactions_root: None,
                    restored_slots: None,
                })
                .unwrap(),
            )
        };

        // Another contract's code hash paired with attacker-chosen code.
        assert!(!inject_compact_block(
            &hash,
            &compact(B256::repeat_byte(0x48), Vec::new()),
            "test"
        ));
        // More senders than the block has transactions.
        assert!(!inject_compact_block(
            &hash,
            &compact(keccak256(&code), vec![Address::ZERO]),
            "test"
        ));
        assert!(
            reth_evm::payload_cache::take_payload_execution::<CachedPayloadData>(&hash).is_none()
        );

        assert!(inject_compact_block(
            &hash,
            &compact(keccak256(&code), Vec::new()),
            "test"
        ));
        assert!(
            reth_evm::payload_cache::take_payload_execution::<CachedPayloadData>(&hash).is_some()
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
        let mut block = n42_tx_types::Block::default();
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
            n42_network::gov5_native_receipts_root::<alloy_consensus::TxType>(&[]),
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
                BlockExecutionOutput::<n42_tx_types::Receipt>::default(),
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
                BlockExecutionOutput::<n42_tx_types::Receipt>::default(),
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
        let mut block = n42_tx_types::Block::default();
        block.header.parent_hash = base_hash;
        block.header.number = 1;
        let hash = block.header.hash_slow();
        let execution = alloy_rpc_types_engine::ExecutionData::from_block_unchecked(hash, &block);
        let address = Address::repeat_byte(0x62);
        let mut output = BlockExecutionOutput::<n42_tx_types::Receipt>::default();
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
