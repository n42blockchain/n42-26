use crate::packet_builder::{build_verification_packet, PacketBuildError};
use alloy_consensus::BlockHeader;
use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{Bytes, B256, U256};
use n42_execution::{execute_block_with_witness, N42EvmConfig};
use n42_mobile::code_cache::CacheSyncMessage;
use n42_mobile::packet::{encode_packet, PacketError, VerificationPacket};
use bytes::BufMut;
use n42_network::ShardedStarHubHandle;
use reth_chainspec::ChainSpec;
use reth_ethereum_primitives::EthPrimitives;
use reth_primitives_traits::NodePrimitives;
use reth_provider::BlockHashReader;
use alloy_eips::BlockHashOrNumber;
use reth_storage_api::{BlockReader, StateProviderFactory, StateProvider, TransactionVariant};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Errors that can occur during mobile packet generation.
#[derive(Debug, thiserror::Error)]
pub enum MobilePacketError {
    #[error("block not found: {0}")]
    BlockNotFound(B256),

    #[error("state provider error: {0}")]
    StateProvider(String),

    #[error("execution error: {0}")]
    Execution(String),

    #[error("packet build error: {0}")]
    PacketBuild(#[from] PacketBuildError),

    #[error("packet encode error: {0}")]
    PacketEncode(#[from] PacketError),

    #[error("compression error: {0}")]
    Compression(String),
}

/// Runs the mobile packet generation loop.
///
/// Receives `(block_hash, block_number)` notifications from the orchestrator after
/// each `BlockCommitted` event, then:
///
/// 1. Fetches the committed block from the reth provider
/// 2. Gets the parent block's state provider
/// 3. Re-executes the block with witness capture (`execute_block_with_witness`)
/// 4. Converts the witness to a `VerificationPacket` via `build_verification_packet`
/// 5. Broadcasts the encoded packet to all connected mobile verifiers via StarHub
///
/// This runs as a separate task to avoid blocking the consensus critical path.
/// At 8-second slots, the ~200ms re-execution overhead is easily absorbed.
/// A block commit notification: `(block_hash, view)`.
/// The view is the consensus view number (roughly corresponding to block sequence).
pub type BlockCommitNotification = (B256, u64);

/// Maximum number of previously-sent bytecodes to track for differential transmission.
/// At ~20 codes per block and 8-second slots, 2000 entries covers ~100 blocks (~13 min).
const MAX_PREVIOUSLY_SENT_CODES: usize = 2000;

/// Maximum number of retries when block is not yet available in reth.
/// Configurable via `N42_PACKET_MAX_RETRIES` environment variable.
fn packet_max_retries() -> u32 {
    std::env::var("N42_PACKET_MAX_RETRIES")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(10)
}

/// Base delay for exponential backoff when retrying block fetch.
/// Configurable via `N42_PACKET_RETRY_BASE_MS` environment variable.
fn packet_retry_base_ms() -> u64 {
    std::env::var("N42_PACKET_RETRY_BASE_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(200)
}

/// Insertion-order bounded code cache (P1 fix).
///
/// Evicts oldest entries (FIFO) when capacity is exceeded, unlike HashMap's
/// arbitrary eviction which could remove recently-sent codes and cause
/// unnecessary retransmission.
struct CodeCache {
    map: HashMap<B256, Bytes>,
    order: VecDeque<B256>,
    capacity: usize,
}

impl CodeCache {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    fn contains_key(&self, key: &B256) -> bool {
        self.map.contains_key(key)
    }

    fn insert(&mut self, key: B256, value: Bytes) {
        if self.map.contains_key(&key) {
            return; // already present, no ordering change needed
        }
        self.map.insert(key, value);
        self.order.push_back(key);
        // Evict oldest entries if over capacity
        while self.map.len() > self.capacity {
            if let Some(old_key) = self.order.pop_front() {
                self.map.remove(&old_key);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn iter(&self) -> impl Iterator<Item = (&B256, &Bytes)> {
        self.map.iter()
    }
}

pub async fn mobile_packet_loop<P>(
    mut rx: mpsc::Receiver<BlockCommitNotification>,
    provider: P,
    chain_spec: Arc<ChainSpec>,
    hub_handle: ShardedStarHubHandle,
    mut phone_connected_rx: mpsc::Receiver<u64>,
) where
    P: BlockReader<Block = <EthPrimitives as NodePrimitives>::Block>
        + StateProviderFactory
        + BlockHashReader
        + Clone
        + 'static,
{
    info!("mobile packet generation loop started");

    let evm_config = N42EvmConfig::new(chain_spec);

    // Track bytecodes that have been broadcast to phones.
    // Subsequent blocks only transmit codes NOT in this cache (differential).
    // New phone connections trigger a CacheSyncMessage with all tracked codes.
    // Uses insertion-order eviction (FIFO) to avoid dropping recently-sent codes.
    let mut previously_sent_codes = CodeCache::new(MAX_PREVIOUSLY_SENT_CODES);

    loop {
        tokio::select! {
            commit = rx.recv() => {
                let Some((block_hash, block_number)) = commit else {
                    break;
                };
                debug!(block_number, %block_hash, "generating mobile verification packet");

                // Retry with exponential backoff: the block may not be persisted in reth yet
                // when the orchestrator sends the notification (fcu finalization is async).
                let mut last_err = None;
                let max_retries = packet_max_retries();
                let base_delay = std::time::Duration::from_millis(packet_retry_base_ms());

                for attempt in 0..max_retries {
                    if attempt > 0 {
                        let delay = base_delay * (1 << attempt.min(4)); // cap at 200ms * 16 = 3.2s
                        tokio::time::sleep(delay).await;
                        debug!(block_number, %block_hash, attempt, "retrying mobile packet generation");
                    }

                    match generate_and_broadcast(
                        &provider,
                        &evm_config,
                        &hub_handle,
                        block_hash,
                        block_number,
                        &mut previously_sent_codes,
                    ) {
                        Ok(packet_size) => {
                            if attempt > 0 {
                                info!(
                                    block_number,
                                    %block_hash,
                                    packet_size,
                                    attempt,
                                    "mobile verification packet broadcast (after retry)"
                                );
                            } else {
                                info!(
                                    block_number,
                                    %block_hash,
                                    packet_size,
                                    "mobile verification packet broadcast"
                                );
                            }
                            last_err = None;
                            break;
                        }
                        Err(e) => {
                            // Only retry on BlockNotFound — other errors are likely permanent
                            if matches!(e, MobilePacketError::BlockNotFound(_)) {
                                last_err = Some(e);
                                continue;
                            }
                            warn!(
                                block_number,
                                %block_hash,
                                error = %e,
                                "failed to generate mobile packet (non-retryable)"
                            );
                            last_err = None;
                            break;
                        }
                    }
                }

                if let Some(e) = last_err {
                    warn!(
                        block_number,
                        %block_hash,
                        error = %e,
                        max_retries,
                        "failed to generate mobile packet after all retries"
                    );
                }
            }

            session_id = phone_connected_rx.recv() => {
                let Some(session_id) = session_id else { continue; };
                // A new phone connected. Send cached codes only to this specific phone
                // (targeted send via per-session channel, avoiding broadcast to all phones).
                if previously_sent_codes.is_empty() {
                    continue;
                }
                let msg = CacheSyncMessage {
                    codes: previously_sent_codes
                        .iter()
                        .map(|(h, c)| (*h, c.clone()))
                        .collect(),
                    evict_hints: vec![],
                };
                match bincode::serialize(&msg) {
                    Ok(data) => {
                        let code_count = msg.codes.len();
                        let raw_len = data.len();
                        let compressed = zstd::bulk::compress(&data, 3)
                            .unwrap_or(data); // fallback to raw on compression failure
                        let compressed_len = compressed.len();
                        // Pre-frame: prepend type prefix so StarHub does a single write_all
                        let mut framed = bytes::BytesMut::with_capacity(1 + compressed.len());
                        framed.put_u8(n42_network::MSG_TYPE_CACHE_SYNC_ZSTD);
                        framed.extend_from_slice(&compressed);
                        if let Err(e) = hub_handle.send_to_session(session_id, framed.freeze()) {
                            warn!(error = %e, session_id, "failed to send cache sync to new phone");
                        } else {
                            info!(
                                codes = code_count,
                                raw_bytes = raw_len,
                                compressed_bytes = compressed_len,
                                session_id,
                                "sent targeted cache sync to new phone"
                            );
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to serialize CacheSyncMessage");
                    }
                }
            }
        }
    }

    info!("mobile packet generation loop stopped (channel closed)");
}

/// Generates a verification packet for a committed block and broadcasts it.
///
/// Filters `uncached_bytecodes` against `previously_sent_codes` to avoid
/// re-transmitting bytecodes that phones already have cached. After broadcast,
/// updates `previously_sent_codes` with the full set of codes from this block.
///
/// Returns the encoded packet size in bytes on success.
fn generate_and_broadcast<P>(
    provider: &P,
    evm_config: &N42EvmConfig,
    hub_handle: &ShardedStarHubHandle,
    block_hash: B256,
    block_number: u64,
    previously_sent_codes: &mut CodeCache,
) -> Result<usize, MobilePacketError>
where
    P: BlockReader<Block = <EthPrimitives as NodePrimitives>::Block>
        + StateProviderFactory
        + BlockHashReader
        + 'static,
{
    // Step 1: Fetch the committed block with senders (needed for EVM execution).
    let recovered_block = provider
        .recovered_block(
            BlockHashOrNumber::Hash(block_hash),
            TransactionVariant::WithHash,
        )
        .map_err(|e| MobilePacketError::StateProvider(e.to_string()))?
        .ok_or(MobilePacketError::BlockNotFound(block_hash))?;

    // Step 2: Get state provider for the parent block (pre-execution state).
    let parent_hash = recovered_block.header().parent_hash();
    let state_provider = provider
        .history_by_block_hash(parent_hash)
        .map_err(|e| MobilePacketError::StateProvider(e.to_string()))?;

    // Step 3: Re-execute the block with witness capture.
    let db = reth_revm::database::StateProviderDatabase::new(&state_provider);
    let result = execute_block_with_witness(evm_config, db, &recovered_block)
        .map_err(|e| MobilePacketError::Execution(e.to_string()))?;

    // Step 4: Collect ancestor block hashes for BLOCKHASH opcode.
    let block_hashes = if let Some(lowest) = result.witness.lowest_block_number {
        collect_block_hashes(provider, lowest, block_number)?
    } else {
        Vec::new()
    };

    // Step 5: Build the verification packet.
    let sealed_block = recovered_block.into_sealed_block();
    let mut packet = build_verification_packet(&result.witness, &sealed_block, &block_hashes)?;

    // Step 5b: Fix witness accounts with PRE-execution state.
    //
    // ExecutionWitness captures POST-execution state from the EVM cache, but mobile
    // verifiers need PRE-execution state to re-execute the block correctly.
    // Query the parent state provider for the original nonce/balance/storage values.
    fix_witness_pre_state(&mut packet, &state_provider)
        .map_err(|e| MobilePacketError::StateProvider(format!("pre-state fixup: {e}")))?;

    // Step 5c: Differential code transmission — filter out previously-sent bytecodes.
    // Phones cache bytecodes received in earlier blocks, so we only need to transmit
    // codes that are new since the last broadcast. This reduces bandwidth significantly
    // for blocks that reuse the same contracts (USDC, Uniswap router, etc.).
    let total_codes = packet.uncached_bytecodes.len();
    let all_codes: Vec<(B256, Bytes)> = packet.uncached_bytecodes.clone();
    packet.uncached_bytecodes.retain(|(hash, _)| !previously_sent_codes.contains_key(hash));
    let filtered_codes = total_codes - packet.uncached_bytecodes.len();

    debug!(
        block_number,
        witness_accounts = packet.witness_accounts.len(),
        uncached_bytecodes = packet.uncached_bytecodes.len(),
        filtered_codes,
        total_codes,
        txs = packet.transactions.len(),
        estimated_size = packet.estimated_size(),
        "verification packet built (differential)"
    );

    // Step 6: Encode, compress, and broadcast.
    let encoded = encode_packet(&packet)?;
    let raw_size = encoded.len();
    let compressed = zstd::bulk::compress(&encoded, 3)
        .map_err(|e| MobilePacketError::Compression(e.to_string()))?;
    let packet_size = compressed.len();
    debug!(block_number, raw_size, compressed_size = packet_size, "verification packet compressed");
    let _ = hub_handle.broadcast_packet(bytes::Bytes::from(compressed));

    // Step 7: Update previously_sent_codes with ALL codes from this block
    // (not just the ones we sent, since phones that received earlier blocks already have them).
    // CodeCache handles FIFO eviction internally when capacity is exceeded.
    for (hash, code) in all_codes {
        previously_sent_codes.insert(hash, code);
    }

    Ok(packet_size)
}

/// Collects ancestor block hashes needed for the BLOCKHASH opcode.
fn collect_block_hashes<P>(
    provider: &P,
    lowest_block: u64,
    current_block: u64,
) -> Result<Vec<(u64, B256)>, MobilePacketError>
where
    P: BlockHashReader,
{
    let mut hashes = Vec::new();
    // BLOCKHASH can reference up to 256 ancestors. Collect from lowest to current-1.
    for num in lowest_block..current_block {
        match provider.block_hash(num) {
            Ok(Some(hash)) => hashes.push((num, hash)),
            Ok(None) => {
                warn!(block_number = num, "ancestor block hash not found");
                break;
            }
            Err(e) => {
                return Err(MobilePacketError::StateProvider(format!(
                    "failed to get block hash for {}: {}",
                    num, e
                )));
            }
        }
    }
    Ok(hashes)
}

/// Fixes the verification packet's witness accounts with PRE-execution state values.
///
/// The `ExecutionWitness` captures POST-execution state from the EVM cache (nonce and
/// balance are after all transactions have been applied). However, mobile verifiers need
/// PRE-execution state to correctly re-execute the block — starting from the same state
/// as the original execution.
///
/// This function queries the parent block's state provider for each witness account's
/// original values and replaces them in the packet. Accounts that didn't exist before
/// the block (newly created via CREATE) are removed since the verifier's EmptyDB will
/// correctly return "no account" for them.
fn fix_witness_pre_state(
    packet: &mut VerificationPacket,
    state_provider: &dyn StateProvider,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut accounts_to_remove = Vec::new();

    for (idx, wa) in packet.witness_accounts.iter_mut().enumerate() {
        match state_provider.basic_account(&wa.address)? {
            Some(account) => {
                // Account existed before this block — use pre-execution values.
                wa.nonce = account.nonce;
                wa.balance = account.balance;
                wa.code_hash = account.bytecode_hash.unwrap_or(KECCAK_EMPTY);
            }
            None => {
                // Account didn't exist before this block (e.g., created via CREATE).
                // Remove it — the verifier's EmptyDB fallback will handle it correctly.
                accounts_to_remove.push(idx);
                continue;
            }
        }

        // Fix storage values: replace post-execution values with pre-execution values.
        for (slot_key, value) in &mut wa.storage {
            let slot_b256 = B256::from(slot_key.to_be_bytes());
            let original_value = state_provider
                .storage(wa.address, slot_b256)?
                .unwrap_or(U256::ZERO);
            *value = original_value;
        }
    }

    // Remove newly created accounts (in reverse order to preserve indices).
    for idx in accounts_to_remove.into_iter().rev() {
        debug!(
            address = %packet.witness_accounts[idx].address,
            "removing newly created account from witness (not in pre-state)"
        );
        packet.witness_accounts.swap_remove(idx);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, B256};

    #[test]
    fn test_code_cache_insert_and_contains() {
        let mut cache = CodeCache::new(10);
        let hash = B256::repeat_byte(0xAA);
        let code = Bytes::from(vec![0x60, 0x00]);

        assert!(!cache.contains_key(&hash));
        assert!(cache.is_empty());

        cache.insert(hash, code.clone());
        assert!(cache.contains_key(&hash));
        assert!(!cache.is_empty());
    }

    #[test]
    fn test_code_cache_duplicate_insert_noop() {
        let mut cache = CodeCache::new(10);
        let hash = B256::repeat_byte(0xBB);
        let code = Bytes::from(vec![0x60, 0x01]);

        cache.insert(hash, code.clone());
        cache.insert(hash, code); // duplicate

        // Should only have one entry
        assert_eq!(cache.order.len(), 1);
        assert_eq!(cache.map.len(), 1);
    }

    #[test]
    fn test_code_cache_fifo_eviction() {
        let mut cache = CodeCache::new(3);

        let h1 = B256::repeat_byte(0x01);
        let h2 = B256::repeat_byte(0x02);
        let h3 = B256::repeat_byte(0x03);
        let h4 = B256::repeat_byte(0x04);

        cache.insert(h1, Bytes::from(vec![1]));
        cache.insert(h2, Bytes::from(vec![2]));
        cache.insert(h3, Bytes::from(vec![3]));
        assert_eq!(cache.map.len(), 3);
        assert_eq!(cache.order.len(), 3);

        // Insert 4th — should evict h1 (FIFO)
        cache.insert(h4, Bytes::from(vec![4]));
        assert_eq!(cache.map.len(), 3);
        assert!(!cache.contains_key(&h1), "oldest entry should be evicted");
        assert!(cache.contains_key(&h2));
        assert!(cache.contains_key(&h3));
        assert!(cache.contains_key(&h4));
    }

    #[test]
    fn test_code_cache_iter() {
        let mut cache = CodeCache::new(10);
        let h1 = B256::repeat_byte(0x01);
        let h2 = B256::repeat_byte(0x02);
        cache.insert(h1, Bytes::from(vec![1]));
        cache.insert(h2, Bytes::from(vec![2]));

        let items: Vec<_> = cache.iter().collect();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn test_code_cache_zero_capacity() {
        // Capacity 0 means every insert immediately evicts.
        let mut cache = CodeCache::new(0);
        let hash = B256::repeat_byte(0xFF);
        cache.insert(hash, Bytes::from(vec![0xFF]));
        // With capacity 0, the while loop evicts until map.len() <= 0
        assert!(cache.is_empty(), "capacity 0 should keep nothing");
    }

    #[test]
    fn test_code_cache_capacity_one() {
        let mut cache = CodeCache::new(1);
        let h1 = B256::repeat_byte(0x01);
        let h2 = B256::repeat_byte(0x02);

        cache.insert(h1, Bytes::from(vec![1]));
        assert!(cache.contains_key(&h1));

        cache.insert(h2, Bytes::from(vec![2]));
        assert!(!cache.contains_key(&h1), "h1 should be evicted");
        assert!(cache.contains_key(&h2));
        assert_eq!(cache.map.len(), 1);
    }

    #[test]
    fn test_packet_retry_defaults() {
        // Verify defaults when env vars are not set
        assert_eq!(packet_max_retries(), 10);
        assert_eq!(packet_retry_base_ms(), 200);
    }

    #[test]
    fn test_zstd_roundtrip() {
        let data = vec![0xABu8; 10_000];
        let compressed = zstd::bulk::compress(&data, 3).unwrap();
        assert!(compressed.len() < data.len());
        let decompressed = zstd::bulk::decompress(&compressed, 16 * 1024 * 1024).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_zstd_witness_like_data_compresses_well() {
        let mut data = Vec::new();
        for _ in 0..100 {
            data.extend_from_slice(&[0x00; 20]); // address
            data.extend_from_slice(&[0x00; 32]); // storage key
            data.extend_from_slice(&[0xAB; 32]); // repeated value
        }
        let compressed = zstd::bulk::compress(&data, 3).unwrap();
        assert!(compressed.len() * 2 < data.len(), "should achieve >50% compression");
    }
}
