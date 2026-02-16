use alloy_primitives::B256;
use n42_primitives::BlsSecretKey;
use tracing::{debug, error, info, warn};

use crate::rpc_client::RpcClient;

/// A simulated mobile phone verifier that submits BLS attestations.
#[allow(dead_code)]
pub struct MobileSimulator {
    pub bls_key: BlsSecretKey,
    pub pubkey_hex: String,
    pub node_url: String,
}

impl MobileSimulator {
    /// Creates a new mobile simulator with a deterministic BLS key.
    ///
    /// Uses `key_gen` (IKM derivation) instead of `from_bytes` because raw
    /// keccak256 output may exceed the BLS12-381 curve order.
    pub fn new(index: usize, node_url: String) -> Self {
        let seed = format!("n42-mobile-key-{index}");
        let seed_hash = alloy_primitives::keccak256(seed.as_bytes());
        let ikm: [u8; 32] = seed_hash.0;
        let bls_key = BlsSecretKey::key_gen(&ikm)
            .expect("valid BLS key from deterministic IKM");

        let pubkey = bls_key.public_key();
        let pubkey_hex = hex::encode(pubkey.to_bytes());

        Self {
            bls_key,
            pubkey_hex,
            node_url,
        }
    }

    /// Signs a block hash and submits the attestation via RPC.
    pub async fn attest_block(
        &self,
        rpc: &RpcClient,
        block_hash: B256,
        slot: u64,
    ) -> eyre::Result<bool> {
        // Sign the block hash using BLS.
        let signature = self.bls_key.sign_hash(&block_hash);
        let sig_hex = hex::encode(signature.to_bytes());

        debug!(
            pubkey = %self.pubkey_hex,
            %block_hash,
            slot,
            "submitting attestation"
        );

        match rpc.submit_attestation(&self.pubkey_hex, &sig_hex, block_hash, slot).await {
            Ok(resp) => {
                debug!(
                    accepted = resp.accepted,
                    count = resp.attestation_count,
                    threshold = resp.threshold_reached,
                    "attestation response"
                );
                Ok(resp.accepted)
            }
            Err(e) => {
                warn!(error = %e, "attestation submission failed");
                Err(e)
            }
        }
    }

    /// Runs the mobile simulator in a loop, polling for new blocks and attesting.
    pub async fn run_polling(
        &self,
        rpc: &RpcClient,
        duration: std::time::Duration,
    ) -> eyre::Result<MobileStats> {
        let start = tokio::time::Instant::now();
        let poll_interval = std::time::Duration::from_secs(2);
        let mut last_block = 0u64;
        let mut stats = MobileStats::default();

        info!(
            pubkey = %self.pubkey_hex,
            duration_secs = duration.as_secs(),
            "mobile simulator started (polling mode)"
        );

        while start.elapsed() < duration {
            match rpc.block_number().await {
                Ok(current_block) => {
                    // Attest any new blocks.
                    for block_num in (last_block + 1)..=current_block {
                        // Get the block hash for this block number.
                        match rpc.get_block_by_number(block_num).await {
                            Ok(block) => {
                                if let Some(hash_str) = block.get("hash").and_then(|h| h.as_str()) {
                                    let block_hash: B256 = hash_str.parse().unwrap_or_default();
                                    match self.attest_block(rpc, block_hash, block_num).await {
                                        Ok(true) => stats.accepted += 1,
                                        Ok(false) => stats.rejected += 1,
                                        Err(_) => stats.errors += 1,
                                    }
                                }
                            }
                            Err(e) => {
                                debug!(block = block_num, error = %e, "failed to get block");
                            }
                        }
                    }
                    last_block = current_block;
                }
                Err(e) => {
                    debug!(error = %e, "failed to get block number");
                }
            }

            tokio::time::sleep(poll_interval).await;
        }

        info!(
            pubkey = %self.pubkey_hex,
            accepted = stats.accepted,
            rejected = stats.rejected,
            errors = stats.errors,
            "mobile simulator finished"
        );

        Ok(stats)
    }
}

/// Statistics from a mobile simulator run.
#[derive(Debug, Default)]
pub struct MobileStats {
    pub accepted: u64,
    pub rejected: u64,
    pub errors: u64,
}

/// Spawns multiple mobile simulators for a node.
pub async fn run_mobile_fleet(
    node_url: String,
    count: usize,
    duration: std::time::Duration,
    start_index: usize,
) -> Vec<MobileStats> {
    let mut handles = Vec::with_capacity(count);

    for i in 0..count {
        let url = node_url.clone();
        let handle = tokio::spawn(async move {
            let sim = MobileSimulator::new(start_index + i, url.clone());
            let rpc = RpcClient::new(url);
            sim.run_polling(&rpc, duration).await.unwrap_or_default()
        });
        handles.push(handle);
    }

    let mut results = Vec::with_capacity(count);
    for handle in handles {
        match handle.await {
            Ok(stats) => results.push(stats),
            Err(e) => {
                error!(error = %e, "mobile simulator task panicked");
                results.push(MobileStats::default());
            }
        }
    }

    results
}
