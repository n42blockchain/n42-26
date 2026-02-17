use crate::packet_builder::{build_verification_packet, PacketBuildError};
use alloy_consensus::BlockHeader;
use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{Bytes, B256, U256};
use n42_execution::{execute_block_with_witness, N42EvmConfig};
use n42_mobile::code_cache::CacheSyncMessage;
use n42_mobile::packet::{encode_packet, PacketError, VerificationPacket};
use n42_network::StarHubHandle;
use reth_chainspec::ChainSpec;
use reth_ethereum_primitives::EthPrimitives;
use reth_primitives_traits::NodePrimitives;
use reth_provider::BlockHashReader;
use alloy_eips::BlockHashOrNumber;
use reth_storage_api::{BlockReader, StateProviderFactory, StateProvider, TransactionVariant};
use std::collections::HashMap;
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

pub async fn mobile_packet_loop<P>(
    mut rx: mpsc::UnboundedReceiver<BlockCommitNotification>,
    provider: P,
    chain_spec: Arc<ChainSpec>,
    hub_handle: StarHubHandle,
    mut phone_connected_rx: mpsc::UnboundedReceiver<()>,
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
    // Subsequent blocks only transmit codes NOT in this map (differential).
    // New phone connections trigger a CacheSyncMessage with all tracked codes.
    let mut previously_sent_codes: HashMap<B256, Bytes> = HashMap::new();

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
                let max_retries = 10;
                let base_delay = std::time::Duration::from_millis(200);

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

            _ = phone_connected_rx.recv() => {
                // A new phone connected. Broadcast all previously-sent codes so the
                // new phone can populate its cache. Existing phones will just re-insert
                // (idempotent). Per-session targeted send is a future optimization.
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
                        let data_len = data.len();
                        if let Err(e) = hub_handle.broadcast_cache_sync(data) {
                            warn!(error = %e, "failed to broadcast cache sync to new phone");
                        } else {
                            info!(
                                codes = code_count,
                                bytes = data_len,
                                "broadcast cache sync for new phone connection"
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
    hub_handle: &StarHubHandle,
    block_hash: B256,
    block_number: u64,
    previously_sent_codes: &mut HashMap<B256, Bytes>,
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

    // Step 6: Encode and broadcast.
    let encoded = encode_packet(&packet)?;
    let packet_size = encoded.len();
    let _ = hub_handle.broadcast_packet(encoded);

    // Step 7: Update previously_sent_codes with ALL codes from this block
    // (not just the ones we sent, since phones that received earlier blocks already have them).
    for (hash, code) in all_codes {
        previously_sent_codes.insert(hash, code);
    }
    // Evict oldest entries if over capacity.
    while previously_sent_codes.len() > MAX_PREVIOUSLY_SENT_CODES {
        // HashMap doesn't have ordering; remove an arbitrary entry.
        if let Some(key) = previously_sent_codes.keys().next().copied() {
            previously_sent_codes.remove(&key);
        }
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
