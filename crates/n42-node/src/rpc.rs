use crate::consensus_state::{AttestationRecord, EquivocationEvidence, SharedConsensusState};
// VerificationTask is used by the #[subscription(item = ...)] macro attribute.
#[allow(unused_imports)]
use crate::consensus_state::VerificationTask;
use alloy_primitives::B256;
use jsonrpsee::core::RpcResult;
use jsonrpsee::core::SubscriptionResult;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;
use jsonrpsee::{PendingSubscriptionSink, SubscriptionMessage};
use n42_primitives::{BlsPublicKey, BlsSignature};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{info, warn};

/// Response for n42_consensusStatus.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsensusStatusResponse {
    pub latest_committed_view: Option<u64>,
    pub latest_committed_block_hash: Option<String>,
    pub validator_count: u32,
    pub has_committed_qc: bool,
}

/// Response for n42_validatorSet.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidatorInfoResponse {
    pub index: u32,
    pub public_key: String,
}

/// Response for n42_submitAttestation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttestationResponse {
    pub accepted: bool,
    pub attestation_count: u32,
    pub threshold_reached: bool,
}

/// Response for n42_health.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    pub status: String,
    pub has_committed_qc: bool,
    pub validator_count: u32,
}

/// Response for n42_attestationStats.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttestationStatsResponse {
    pub total_attestations: usize,
    pub earliest_block: Option<u64>,
    pub latest_block: Option<u64>,
}

/// Response for n42_equivocations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EquivocationsResponse {
    pub total: usize,
    pub evidence: Vec<EquivocationEvidence>,
}

/// N42-specific RPC API.
#[rpc(server, namespace = "n42")]
pub trait N42Api {
    /// Returns the node health status for load balancers and K8s probes.
    /// Returns "ok" when consensus has committed at least one block,
    /// "syncing" when no block has been committed yet.
    #[method(name = "health")]
    async fn health(&self) -> RpcResult<HealthResponse>;

    /// Returns the current consensus status.
    #[method(name = "consensusStatus")]
    async fn consensus_status(&self) -> RpcResult<ConsensusStatusResponse>;

    /// Returns the current validator set.
    #[method(name = "validatorSet")]
    async fn validator_set(&self) -> RpcResult<Vec<ValidatorInfoResponse>>;

    /// Mobile subscribes to verification tasks. Pushes a notification each time
    /// a new block is committed by consensus.
    #[subscription(name = "subscribeVerification", unsubscribe = "unsubscribeVerification", item = VerificationTask)]
    async fn subscribe_verification(&self) -> SubscriptionResult;

    /// Mobile submits a BLS attestation for a committed block.
    #[method(name = "submitAttestation")]
    async fn submit_attestation(
        &self,
        pubkey: String,
        signature: String,
        block_hash: B256,
        slot: u64,
    ) -> RpcResult<AttestationResponse>;

    /// Returns the attestation record for a specific block hash.
    #[method(name = "blockAttestation")]
    async fn block_attestation(&self, block_hash: B256) -> RpcResult<Option<AttestationRecord>>;

    /// Returns summary statistics about mobile attestation history.
    #[method(name = "attestationStats")]
    async fn attestation_stats(&self) -> RpcResult<AttestationStatsResponse>;

    /// Returns all recorded equivocation evidence.
    #[method(name = "equivocations")]
    async fn equivocations(&self) -> RpcResult<EquivocationsResponse>;
}

/// Implementation of the N42 RPC API.
pub struct N42RpcServer {
    consensus_state: Arc<SharedConsensusState>,
}

impl N42RpcServer {
    pub fn new(consensus_state: Arc<SharedConsensusState>) -> Self {
        Self { consensus_state }
    }
}

#[async_trait::async_trait]
impl N42ApiServer for N42RpcServer {
    async fn health(&self) -> RpcResult<HealthResponse> {
        let has_qc = self.consensus_state.load_committed_qc().is_some();
        let status = if has_qc { "ok" } else { "syncing" };
        Ok(HealthResponse {
            status: status.to_string(),
            has_committed_qc: has_qc,
            validator_count: self.consensus_state.validator_set.len(),
        })
    }

    async fn consensus_status(&self) -> RpcResult<ConsensusStatusResponse> {
        let committed_qc = self.consensus_state.load_committed_qc();
        let (view, block_hash, has_qc) = match committed_qc.as_ref() {
            Some(qc) => (
                Some(qc.view),
                Some(format!("{:?}", qc.block_hash)),
                true,
            ),
            None => (None, None, false),
        };

        Ok(ConsensusStatusResponse {
            latest_committed_view: view,
            latest_committed_block_hash: block_hash,
            validator_count: self.consensus_state.validator_set.len(),
            has_committed_qc: has_qc,
        })
    }

    async fn validator_set(&self) -> RpcResult<Vec<ValidatorInfoResponse>> {
        let vs = &self.consensus_state.validator_set;
        let mut result = Vec::with_capacity(vs.len() as usize);
        for i in 0..vs.len() {
            if let Ok(pk) = vs.get_public_key(i) {
                result.push(ValidatorInfoResponse {
                    index: i,
                    public_key: hex::encode(pk.to_bytes()),
                });
            }
        }
        Ok(result)
    }

    async fn subscribe_verification(
        &self,
        pending: PendingSubscriptionSink,
    ) -> SubscriptionResult {
        let sink = pending.accept().await?;
        let mut rx = self.consensus_state.block_committed_tx.subscribe();

        info!("mobile verification subscriber connected");

        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(task) => {
                        let msg = SubscriptionMessage::new(
                            sink.method_name(),
                            sink.subscription_id(),
                            &task,
                        )
                        .expect("VerificationTask serialization cannot fail");
                        if sink.send(msg).await.is_err() {
                            break; // client disconnected
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "verification subscription lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break; // channel closed
                    }
                }
            }
        });

        Ok(())
    }

    async fn submit_attestation(
        &self,
        pubkey: String,
        signature: String,
        block_hash: B256,
        slot: u64,
    ) -> RpcResult<AttestationResponse> {
        // 1. Decode pubkey hex -> 48 bytes -> BlsPublicKey
        let pubkey_hex = pubkey.strip_prefix("0x").unwrap_or(&pubkey);
        let pubkey_bytes = hex::decode(pubkey_hex).map_err(|e| {
            ErrorObjectOwned::owned(-32602, format!("invalid pubkey hex: {e}"), None::<()>)
        })?;

        let pubkey_array: [u8; 48] = pubkey_bytes.try_into().map_err(|v: Vec<u8>| {
            ErrorObjectOwned::owned(
                -32602,
                format!("pubkey must be exactly 48 bytes, got {}", v.len()),
                None::<()>,
            )
        })?;

        let bls_pubkey = BlsPublicKey::from_bytes(&pubkey_array).map_err(|e| {
            ErrorObjectOwned::owned(-32602, format!("invalid BLS public key: {e}"), None::<()>)
        })?;

        // 2. Decode signature hex -> 96 bytes -> BlsSignature
        let sig_hex = signature.strip_prefix("0x").unwrap_or(&signature);
        let sig_bytes = hex::decode(sig_hex).map_err(|e| {
            ErrorObjectOwned::owned(-32602, format!("invalid signature hex: {e}"), None::<()>)
        })?;

        let sig_array: [u8; 96] = sig_bytes.try_into().map_err(|v: Vec<u8>| {
            ErrorObjectOwned::owned(
                -32602,
                format!("signature must be exactly 96 bytes, got {}", v.len()),
                None::<()>,
            )
        })?;

        let bls_sig = BlsSignature::from_bytes(&sig_array).map_err(|e| {
            ErrorObjectOwned::owned(-32602, format!("invalid BLS signature: {e}"), None::<()>)
        })?;

        // 3. Verify BLS signature over block_hash
        bls_pubkey
            .verify(block_hash.as_slice(), &bls_sig)
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    -32003,
                    format!("BLS signature verification failed: {e}"),
                    None::<()>,
                )
            })?;

        // 4. Record attestation
        let canonical_pubkey_hex = hex::encode(pubkey_array);
        let mut att_state =
            self.consensus_state
                .attestation_state
                .lock()
                .map_err(|_| {
                    ErrorObjectOwned::owned(
                        -32603,
                        "internal error: attestation state lock poisoned",
                        None::<()>,
                    )
                })?;

        match att_state.record_attestation(block_hash, canonical_pubkey_hex) {
            Some((count, threshold_reached)) => {
                if threshold_reached {
                    info!(
                        %block_hash,
                        slot,
                        count,
                        "mobile attestation threshold reached"
                    );
                }
                Ok(AttestationResponse {
                    accepted: true,
                    attestation_count: count,
                    threshold_reached,
                })
            }
            None => Err(ErrorObjectOwned::owned(
                -32001,
                format!("unknown block hash: {block_hash}"),
                None::<()>,
            )),
        }
    }

    async fn block_attestation(&self, block_hash: B256) -> RpcResult<Option<AttestationRecord>> {
        Ok(self.consensus_state.get_block_attestation(&block_hash))
    }

    async fn attestation_stats(&self) -> RpcResult<AttestationStatsResponse> {
        let (total, earliest, latest) = self.consensus_state.attestation_stats();
        Ok(AttestationStatsResponse {
            total_attestations: total,
            earliest_block: earliest,
            latest_block: latest,
        })
    }

    async fn equivocations(&self) -> RpcResult<EquivocationsResponse> {
        let evidence = self.consensus_state.get_equivocations();
        Ok(EquivocationsResponse {
            total: evidence.len(),
            evidence,
        })
    }
}
