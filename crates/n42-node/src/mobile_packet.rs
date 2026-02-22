use alloy_consensus::BlockHeader;
use alloy_primitives::{Bytes, B256};
use alloy_rlp::Encodable;
use bytes::BufMut;
use n42_execution::{read_log::ReadLogDatabase, N42EvmConfig};
use n42_mobile::code_cache::{encode_cache_sync, CacheSyncMessage};
use n42_mobile::packet::{encode_stream_packet, StreamPacket};
use n42_network::ShardedStarHubHandle;
use reth_chainspec::ChainSpec;
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::{execute::Executor, ConfigureEvm};
use reth_primitives_traits::{BlockBody, NodePrimitives};
use reth_provider::BlockHashReader;
use alloy_eips::BlockHashOrNumber;
use reth_storage_api::{BlockReader, StateProviderFactory, TransactionVariant};
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

    #[error("compression error: {0}")]
    Compression(String),
}

/// A block commit notification: `(block_hash, consensus_view)`.
pub type BlockCommitNotification = (B256, u64);

/// Maximum number of previously-sent bytecodes to track for differential transmission.
/// At ~20 codes per block and 8-second slots, 2000 entries covers ~100 blocks (~13 min).
const MAX_PREVIOUSLY_SENT_CODES: usize = 2000;

fn packet_max_retries() -> u32 {
    std::env::var("N42_PACKET_MAX_RETRIES")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(10)
}

fn packet_retry_base_ms() -> u64 {
    std::env::var("N42_PACKET_RETRY_BASE_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(200)
}

/// Insertion-order bounded code cache with FIFO eviction.
struct CodeCache {
    map: HashMap<B256, Bytes>,
    order: VecDeque<B256>,
    capacity: usize,
}

impl CodeCache {
    fn new(capacity: usize) -> Self {
        Self { map: HashMap::new(), order: VecDeque::new(), capacity }
    }

    fn contains_key(&self, key: &B256) -> bool {
        self.map.contains_key(key)
    }

    fn insert(&mut self, key: B256, value: Bytes) {
        if self.map.contains_key(&key) {
            return;
        }
        self.map.insert(key, value);
        self.order.push_back(key);
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

/// Runs the mobile packet generation loop (V2 stream format).
///
/// Uses `ReadLogDatabase` to capture ordered state reads during block execution,
/// then broadcasts a compact `StreamPacket`. Only new bytecodes are transmitted
/// (differential encoding against `previously_sent_codes`).
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
    info!("mobile packet generation loop started (V2 stream format)");

    let evm_config = N42EvmConfig::new(chain_spec);
    let mut previously_sent_codes = CodeCache::new(MAX_PREVIOUSLY_SENT_CODES);
    let max_retries = packet_max_retries();
    let base_delay = std::time::Duration::from_millis(packet_retry_base_ms());

    loop {
        tokio::select! {
            commit = rx.recv() => {
                let Some((block_hash, _view)) = commit else { break };
                debug!(%block_hash, "generating mobile stream packet");

                let mut last_err = None;
                for attempt in 0..max_retries {
                    if attempt > 0 {
                        tokio::time::sleep(base_delay * (1 << attempt.min(4))).await;
                        debug!(%block_hash, attempt, "retrying mobile packet generation");
                    }

                    match generate_and_broadcast_v2(
                        &provider,
                        &evm_config,
                        &hub_handle,
                        block_hash,
                        &mut previously_sent_codes,
                    ) {
                        Ok(packet_size) => {
                            if attempt > 0 {
                                info!(%block_hash, packet_size, attempt, "mobile stream packet broadcast (after retry)");
                            } else {
                                info!(%block_hash, packet_size, "mobile stream packet broadcast");
                            }
                            last_err = None;
                            break;
                        }
                        Err(e) => {
                            if matches!(e, MobilePacketError::BlockNotFound(_)) {
                                last_err = Some(e);
                                continue;
                            }
                            warn!(%block_hash, error = %e, "failed to generate mobile packet (non-retryable)");
                            last_err = None;
                            break;
                        }
                    }
                }

                if let Some(e) = last_err {
                    warn!(%block_hash, error = %e, max_retries, "failed to generate mobile packet after all retries");
                }
            }

            session_id = phone_connected_rx.recv() => {
                let Some(session_id) = session_id else { continue };
                if previously_sent_codes.is_empty() {
                    continue;
                }
                let msg = CacheSyncMessage {
                    codes: previously_sent_codes.iter().map(|(h, c)| (*h, c.clone())).collect(),
                    evict_hints: vec![],
                };
                let data = encode_cache_sync(&msg);
                let code_count = msg.codes.len();
                let raw_len = data.len();
                let compressed = zstd::bulk::compress(&data, 3).unwrap_or(data);
                let compressed_len = compressed.len();
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
                        "sent targeted cache sync to new phone (V2 format)"
                    );
                }
            }
        }
    }

    info!("mobile packet generation loop stopped (channel closed)");
}

/// Generates a V2 stream packet and broadcasts it.
///
/// Flow: fetch block → get parent state → wrap with ReadLogDatabase → execute
/// → extract read log + bytecodes → differential filter → encode → zstd → broadcast.
fn generate_and_broadcast_v2<P>(
    provider: &P,
    evm_config: &N42EvmConfig,
    hub_handle: &ShardedStarHubHandle,
    block_hash: B256,
    previously_sent_codes: &mut CodeCache,
) -> Result<usize, MobilePacketError>
where
    P: BlockReader<Block = <EthPrimitives as NodePrimitives>::Block>
        + StateProviderFactory
        + BlockHashReader
        + 'static,
{
    let recovered_block = provider
        .recovered_block(BlockHashOrNumber::Hash(block_hash), TransactionVariant::WithHash)
        .map_err(|e| MobilePacketError::StateProvider(e.to_string()))?
        .ok_or(MobilePacketError::BlockNotFound(block_hash))?;

    let parent_hash = recovered_block.header().parent_hash();
    let state_provider = provider
        .history_by_block_hash(parent_hash)
        .map_err(|e| MobilePacketError::StateProvider(e.to_string()))?;

    let inner_db = reth_revm::database::StateProviderDatabase::new(&state_provider);
    let logged_db = ReadLogDatabase::new(inner_db);
    let log_handle = logged_db.log_handle();
    let codes_handle = logged_db.codes_handle();

    let mut executor = evm_config.executor(logged_db);
    let _output = executor
        .execute_one(&recovered_block)
        .map_err(|e| MobilePacketError::Execution(e.to_string()))?;

    let read_log = match Arc::try_unwrap(log_handle) {
        Ok(mutex) => mutex.into_inner().unwrap(),
        Err(arc) => arc.lock().unwrap().clone(),
    };
    let read_log_count = read_log.len();
    let read_log_data = n42_execution::read_log::encode_read_log(&read_log);

    let captured_codes = match Arc::try_unwrap(codes_handle) {
        Ok(mutex) => mutex.into_inner().unwrap(),
        Err(arc) => arc.lock().unwrap().clone(),
    };

    let header = recovered_block.header();
    let block_number = header.number();
    let mut header_buf = Vec::new();
    header.encode(&mut header_buf);
    let transactions = recovered_block.body().encoded_2718_transactions();

    let all_codes: Vec<(B256, Bytes)> = captured_codes.into_iter().collect();
    let total_codes = all_codes.len();
    let new_codes: Vec<(B256, Bytes)> = all_codes
        .iter()
        .filter(|(hash, _)| !previously_sent_codes.contains_key(hash))
        .cloned()
        .collect();
    let filtered_codes = total_codes - new_codes.len();

    let packet = StreamPacket {
        block_hash,
        header_rlp: Bytes::from(header_buf),
        transactions,
        read_log_data,
        bytecodes: new_codes,
    };

    debug!(
        block_number,
        read_log_entries = read_log_count,
        read_log_bytes = packet.read_log_data.len(),
        bytecodes = packet.bytecodes.len(),
        filtered_codes,
        total_codes,
        txs = packet.transactions.len(),
        estimated_size = packet.estimated_size(),
        "stream packet built (V2 differential)"
    );

    let encoded = encode_stream_packet(&packet);
    let raw_size = encoded.len();
    let compressed = zstd::bulk::compress(&encoded, 3)
        .map_err(|e| MobilePacketError::Compression(e.to_string()))?;
    let packet_size = compressed.len();
    debug!(block_number, raw_size, compressed_size = packet_size, "stream packet compressed");
    let _ = hub_handle.broadcast_packet(bytes::Bytes::from(compressed));

    for (hash, code) in all_codes {
        previously_sent_codes.insert(hash, code);
    }

    Ok(packet_size)
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

        cache.insert(hash, code);
        assert!(cache.contains_key(&hash));
        assert!(!cache.is_empty());
    }

    #[test]
    fn test_code_cache_duplicate_insert_noop() {
        let mut cache = CodeCache::new(10);
        let hash = B256::repeat_byte(0xBB);
        let code = Bytes::from(vec![0x60, 0x01]);

        cache.insert(hash, code.clone());
        cache.insert(hash, code);

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
        cache.insert(B256::repeat_byte(0x01), Bytes::from(vec![1]));
        cache.insert(B256::repeat_byte(0x02), Bytes::from(vec![2]));

        assert_eq!(cache.iter().count(), 2);
    }

    #[test]
    fn test_code_cache_zero_capacity() {
        let mut cache = CodeCache::new(0);
        cache.insert(B256::repeat_byte(0xFF), Bytes::from(vec![0xFF]));
        assert!(cache.is_empty(), "capacity 0 should keep nothing");
    }

    #[test]
    fn test_code_cache_capacity_one() {
        let mut cache = CodeCache::new(1);
        let h1 = B256::repeat_byte(0x01);
        let h2 = B256::repeat_byte(0x02);

        cache.insert(h1, Bytes::from(vec![1]));
        cache.insert(h2, Bytes::from(vec![2]));
        assert!(!cache.contains_key(&h1), "h1 should be evicted");
        assert!(cache.contains_key(&h2));
        assert_eq!(cache.map.len(), 1);
    }

    #[test]
    fn test_packet_retry_defaults() {
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
            data.extend_from_slice(&[0x00; 20]);
            data.extend_from_slice(&[0x00; 32]);
            data.extend_from_slice(&[0xAB; 32]);
        }
        let compressed = zstd::bulk::compress(&data, 3).unwrap();
        assert!(compressed.len() * 2 < data.len(), "should achieve >50% compression");
    }
}
