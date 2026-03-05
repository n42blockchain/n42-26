use crate::consensus_state::{AttestationRecord, EquivocationEvidence, SharedConsensusState};
use crate::staking::{StakingManager, StakeStatus, MIN_STAKE_WEI, UNSTAKE_COOLDOWN_BLOCKS, STAKING_ADDRESS};
// VerificationTask is used by the #[subscription(item = ...)] macro attribute.
#[allow(unused_imports)]
use crate::consensus_state::VerificationTask;
use alloy_primitives::{Address, B256};
use jsonrpsee::core::RpcResult;
use jsonrpsee::core::SubscriptionResult;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;
use jsonrpsee::{PendingSubscriptionSink, SubscriptionMessage};
use n42_primitives::{BlsPublicKey, BlsSignature};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsensusStatusResponse {
    pub latest_committed_view: Option<u64>,
    pub latest_committed_block_hash: Option<String>,
    pub validator_count: u32,
    pub has_committed_qc: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidatorInfoResponse {
    pub index: u32,
    pub public_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttestationResponse {
    pub accepted: bool,
    pub attestation_count: u32,
    pub threshold_reached: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    pub status: String,
    pub has_committed_qc: bool,
    pub validator_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttestationStatsResponse {
    pub total_attestations: usize,
    pub earliest_block: Option<u64>,
    pub latest_block: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EquivocationsResponse {
    pub total: usize,
    pub evidence: Vec<EquivocationEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StakingStatusResponse {
    pub staked: bool,
    pub registered: bool,
    pub amount: String,
    pub bls_pubkey: String,
    pub status: String,
    pub cooldown_remaining_blocks: u64,
    pub staked_at_block: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StakingInfoResponse {
    pub total_staked: String,
    pub staker_count: u64,
    pub registered_count: u64,
    pub min_stake: String,
    pub cooldown_blocks: u64,
    pub staking_address: String,
}

/// N42-specific RPC API.
#[rpc(server, namespace = "n42")]
pub trait N42Api {
    /// Returns "ok" when consensus has committed at least one block, "syncing" otherwise.
    #[method(name = "health")]
    async fn health(&self) -> RpcResult<HealthResponse>;

    #[method(name = "consensusStatus")]
    async fn consensus_status(&self) -> RpcResult<ConsensusStatusResponse>;

    #[method(name = "validatorSet")]
    async fn validator_set(&self) -> RpcResult<Vec<ValidatorInfoResponse>>;

    /// Pushes a notification each time a new block is committed by consensus.
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

    #[method(name = "blockAttestation")]
    async fn block_attestation(&self, block_hash: B256) -> RpcResult<Option<AttestationRecord>>;

    #[method(name = "attestationStats")]
    async fn attestation_stats(&self) -> RpcResult<AttestationStatsResponse>;

    #[method(name = "equivocations")]
    async fn equivocations(&self) -> RpcResult<EquivocationsResponse>;

    /// Returns staking status for a given address.
    #[method(name = "stakingStatus")]
    async fn staking_status(&self, address: Address) -> RpcResult<StakingStatusResponse>;

    /// Returns global staking information.
    #[method(name = "stakingInfo")]
    async fn staking_info(&self) -> RpcResult<StakingInfoResponse>;
}

pub struct N42RpcServer {
    consensus_state: Arc<SharedConsensusState>,
    staking_manager: Option<Arc<Mutex<StakingManager>>>,
}

impl N42RpcServer {
    pub fn new(consensus_state: Arc<SharedConsensusState>) -> Self {
        Self {
            consensus_state,
            staking_manager: None,
        }
    }

    pub fn with_staking_manager(mut self, mgr: Arc<Mutex<StakingManager>>) -> Self {
        self.staking_manager = Some(mgr);
        self
    }
}

#[async_trait::async_trait]
impl N42ApiServer for N42RpcServer {
    async fn health(&self) -> RpcResult<HealthResponse> {
        let has_qc = self.consensus_state.load_committed_qc().is_some();
        Ok(HealthResponse {
            status: if has_qc { "ok" } else { "syncing" }.to_string(),
            has_committed_qc: has_qc,
            validator_count: self.consensus_state.validator_count(),
        })
    }

    async fn consensus_status(&self) -> RpcResult<ConsensusStatusResponse> {
        let committed_qc = self.consensus_state.load_committed_qc();
        let (view, block_hash, has_qc) = match committed_qc.as_ref() {
            Some(qc) => (Some(qc.view), Some(format!("{:?}", qc.block_hash)), true),
            None => (None, None, false),
        };

        Ok(ConsensusStatusResponse {
            latest_committed_view: view,
            latest_committed_block_hash: block_hash,
            validator_count: self.consensus_state.validator_count(),
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
        let mut rx = self.consensus_state.subscribe_block_committed();

        info!("mobile verification subscriber connected");

        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(task) => {
                        let msg = match SubscriptionMessage::new(
                            sink.method_name(),
                            sink.subscription_id(),
                            &task,
                        ) {
                            Ok(m) => m,
                            Err(e) => {
                                tracing::error!(error = %e, "failed to serialize VerificationTask");
                                continue;
                            }
                        };
                        if sink.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "verification subscription lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
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
        let pubkey_bytes = hex::decode(pubkey.strip_prefix("0x").unwrap_or(&pubkey))
            .map_err(|e| ErrorObjectOwned::owned(-32602, format!("invalid pubkey hex: {e}"), None::<()>))?;

        let pubkey_array: [u8; 48] = pubkey_bytes.try_into().map_err(|v: Vec<u8>| {
            ErrorObjectOwned::owned(
                -32602,
                format!("pubkey must be exactly 48 bytes, got {}", v.len()),
                None::<()>,
            )
        })?;

        let bls_pubkey = BlsPublicKey::from_bytes(&pubkey_array)
            .map_err(|e| ErrorObjectOwned::owned(-32602, format!("invalid BLS public key: {e}"), None::<()>))?;

        let sig_bytes = hex::decode(signature.strip_prefix("0x").unwrap_or(&signature))
            .map_err(|e| ErrorObjectOwned::owned(-32602, format!("invalid signature hex: {e}"), None::<()>))?;

        let sig_array: [u8; 96] = sig_bytes.try_into().map_err(|v: Vec<u8>| {
            ErrorObjectOwned::owned(
                -32602,
                format!("signature must be exactly 96 bytes, got {}", v.len()),
                None::<()>,
            )
        })?;

        let bls_sig = BlsSignature::from_bytes(&sig_array)
            .map_err(|e| ErrorObjectOwned::owned(-32602, format!("invalid BLS signature: {e}"), None::<()>))?;

        bls_pubkey.verify(block_hash.as_slice(), &bls_sig).map_err(|e| {
            ErrorObjectOwned::owned(-32003, format!("BLS signature verification failed: {e}"), None::<()>)
        })?;

        // Security: only accept attestations from verifiers that completed a QUIC
        // handshake with StarHub. Self-signed BLS proofs alone are not sufficient;
        // the pubkey must also be in the authorized set populated by the bridge.
        if !self.consensus_state.is_authorized_verifier(&pubkey_array) {
            return Err(ErrorObjectOwned::owned(
                -32004,
                "verifier not authorized: pubkey not registered via QUIC handshake",
                None::<()>,
            ));
        }

        let canonical_pubkey_hex = hex::encode(pubkey_array);
        let mut att_state = self.consensus_state.attestation_state.lock().map_err(|_| {
            ErrorObjectOwned::owned(-32603, "internal error: attestation state lock poisoned", None::<()>)
        })?;

        match att_state.record_attestation(block_hash, canonical_pubkey_hex) {
            Some((count, threshold_reached)) => {
                if threshold_reached {
                    info!(%block_hash, slot, count, "mobile attestation threshold reached");
                }
                Ok(AttestationResponse { accepted: true, attestation_count: count, threshold_reached })
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
        Ok(EquivocationsResponse { total: evidence.len(), evidence })
    }

    async fn staking_status(&self, address: Address) -> RpcResult<StakingStatusResponse> {
        let staking_mgr = match &self.staking_manager {
            Some(mgr) => mgr,
            None => {
                return Ok(StakingStatusResponse {
                    staked: false,
                    registered: false,
                    amount: "0".to_string(),
                    bls_pubkey: String::new(),
                    status: "no_staking_manager".to_string(),
                    cooldown_remaining_blocks: 0,
                    staked_at_block: 0,
                });
            }
        };

        let mgr = staking_mgr.lock().map_err(|_| {
            ErrorObjectOwned::owned(-32603, "staking manager lock poisoned", None::<()>)
        })?;

        if let Some(entry) = mgr.get_stake(&address) {
            let (status_str, cooldown_remaining) = match entry.status {
                StakeStatus::Active => ("active".to_string(), 0u64),
                StakeStatus::Unstaking { initiated_block } => {
                    let end = initiated_block + UNSTAKE_COOLDOWN_BLOCKS;
                    let current = mgr.last_scanned_block();
                    let remaining = end.saturating_sub(current);
                    ("unstaking".to_string(), remaining)
                }
            };
            let is_staked = matches!(entry.status, StakeStatus::Active)
                && entry.amount >= MIN_STAKE_WEI;
            Ok(StakingStatusResponse {
                staked: is_staked,
                registered: true, // staked implies registered
                amount: format!("{}", entry.amount),
                bls_pubkey: hex::encode(entry.bls_pubkey),
                status: status_str,
                cooldown_remaining_blocks: cooldown_remaining,
                staked_at_block: entry.staked_at_block,
            })
        } else if let Some(reg) = mgr.get_registration(&address) {
            Ok(StakingStatusResponse {
                staked: false,
                registered: true,
                amount: "0".to_string(),
                bls_pubkey: hex::encode(reg.bls_pubkey),
                status: "registered".to_string(),
                cooldown_remaining_blocks: 0,
                staked_at_block: 0,
            })
        } else {
            Ok(StakingStatusResponse {
                staked: false,
                registered: false,
                amount: "0".to_string(),
                bls_pubkey: String::new(),
                status: "not_registered".to_string(),
                cooldown_remaining_blocks: 0,
                staked_at_block: 0,
            })
        }
    }

    async fn staking_info(&self) -> RpcResult<StakingInfoResponse> {
        let staking_mgr = match &self.staking_manager {
            Some(mgr) => mgr,
            None => {
                return Ok(StakingInfoResponse {
                    total_staked: "0".to_string(),
                    staker_count: 0,
                    registered_count: 0,
                    min_stake: format!("{MIN_STAKE_WEI}"),
                    cooldown_blocks: UNSTAKE_COOLDOWN_BLOCKS,
                    staking_address: format!("{STAKING_ADDRESS}"),
                });
            }
        };

        let mgr = staking_mgr.lock().map_err(|_| {
            ErrorObjectOwned::owned(-32603, "staking manager lock poisoned", None::<()>)
        })?;

        let (total, count) = mgr.total_staked();
        Ok(StakingInfoResponse {
            total_staked: format!("{total}"),
            staker_count: count,
            registered_count: mgr.registration_count() as u64,
            min_stake: format!("{MIN_STAKE_WEI}"),
            cooldown_blocks: UNSTAKE_COOLDOWN_BLOCKS,
            staking_address: format!("{STAKING_ADDRESS}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use n42_consensus::ValidatorSet;
    use n42_chainspec::ValidatorInfo;
    use alloy_primitives::Address;
    use n42_primitives::{BlsSecretKey, QuorumCertificate};

    fn make_rpc() -> N42RpcServer {
        let vs = ValidatorSet::new(&[], 0);
        let state = Arc::new(SharedConsensusState::new(vs));
        N42RpcServer::new(state)
    }

    fn make_rpc_with_validators(count: usize) -> N42RpcServer {
        let validators: Vec<_> = (0..count)
            .map(|_| {
                let sk = BlsSecretKey::random().unwrap();
                ValidatorInfo { address: Address::ZERO, bls_public_key: sk.public_key() }
            })
            .collect();
        let vs = ValidatorSet::new(&validators, 0);
        let state = Arc::new(SharedConsensusState::new(vs));
        N42RpcServer::new(state)
    }

    fn make_qc(view: u64, block_hash: B256) -> QuorumCertificate {
        let mut qc = QuorumCertificate::genesis();
        qc.view = view;
        qc.block_hash = block_hash;
        qc
    }

    #[tokio::test]
    async fn test_consensus_status_empty() {
        let rpc = make_rpc();
        let status = rpc.consensus_status().await.unwrap();
        assert!(!status.has_committed_qc);
        assert!(status.latest_committed_view.is_none());
        assert!(status.latest_committed_block_hash.is_none());
        assert_eq!(status.validator_count, 0);
    }

    #[tokio::test]
    async fn test_consensus_status_with_qc() {
        let vs = ValidatorSet::new(&[], 0);
        let state = Arc::new(SharedConsensusState::new(vs));
        state.update_committed_qc(make_qc(42, B256::repeat_byte(0xAB)));

        let rpc = N42RpcServer::new(state);
        let status = rpc.consensus_status().await.unwrap();
        assert!(status.has_committed_qc);
        assert_eq!(status.latest_committed_view, Some(42));
        assert!(status.latest_committed_block_hash.is_some());
    }

    #[tokio::test]
    async fn test_validator_set_response() {
        let rpc = make_rpc_with_validators(3);
        let result = rpc.validator_set().await.unwrap();
        assert_eq!(result.len(), 3);
        for (i, v) in result.iter().enumerate() {
            assert_eq!(v.index, i as u32);
            assert!(!v.public_key.is_empty());
        }
    }

    #[tokio::test]
    async fn test_health_syncing() {
        let rpc = make_rpc();
        let health = rpc.health().await.unwrap();
        assert_eq!(health.status, "syncing");
        assert!(!health.has_committed_qc);
    }

    #[tokio::test]
    async fn test_health_ok() {
        let vs = ValidatorSet::new(&[], 0);
        let state = Arc::new(SharedConsensusState::new(vs));
        state.update_committed_qc(make_qc(1, B256::ZERO));

        let rpc = N42RpcServer::new(state);
        let health = rpc.health().await.unwrap();
        assert_eq!(health.status, "ok");
        assert!(health.has_committed_qc);
    }

    #[tokio::test]
    async fn test_attestation_stats_empty() {
        let rpc = make_rpc();
        let stats = rpc.attestation_stats().await.unwrap();
        assert_eq!(stats.total_attestations, 0);
        assert!(stats.earliest_block.is_none());
        assert!(stats.latest_block.is_none());
    }

    #[tokio::test]
    async fn test_equivocations_empty() {
        let rpc = make_rpc();
        let result = rpc.equivocations().await.unwrap();
        assert_eq!(result.total, 0);
        assert!(result.evidence.is_empty());
    }

    #[tokio::test]
    async fn test_submit_attestation_invalid_hex() {
        let rpc = make_rpc();
        let result = rpc
            .submit_attestation("not_hex".into(), "0000".into(), B256::ZERO, 0)
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), -32602);
    }

    #[tokio::test]
    async fn test_submit_attestation_wrong_pubkey_length() {
        let rpc = make_rpc();
        let result = rpc
            .submit_attestation(hex::encode([0u8; 32]), hex::encode([0u8; 96]), B256::ZERO, 0)
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), -32602);
        assert!(err.message().contains("48 bytes"));
    }

    #[tokio::test]
    async fn test_submit_attestation_wrong_sig_length() {
        let rpc = make_rpc();
        let sk = BlsSecretKey::random().unwrap();
        let result = rpc
            .submit_attestation(
                hex::encode(sk.public_key().to_bytes()),
                hex::encode([0u8; 48]),
                B256::ZERO,
                0,
            )
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), -32602);
        assert!(err.message().contains("96 bytes"));
    }

    #[tokio::test]
    async fn test_submit_attestation_unknown_block() {
        let vs = ValidatorSet::new(&[], 0);
        let state = Arc::new(SharedConsensusState::new(vs));
        let sk = BlsSecretKey::random().unwrap();
        // Authorize so we pass the identity check and reach the "unknown block" error.
        state.authorize_verifier(sk.public_key().to_bytes());

        let rpc = N42RpcServer::new(state);
        let block_hash = B256::repeat_byte(0xCC);
        let sig = sk.sign(block_hash.as_slice());

        let result = rpc
            .submit_attestation(
                hex::encode(sk.public_key().to_bytes()),
                hex::encode(sig.to_bytes()),
                block_hash,
                1,
            )
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), -32001);
    }

    #[tokio::test]
    async fn test_submit_attestation_valid() {
        let vs = ValidatorSet::new(&[], 0);
        let state = Arc::new(SharedConsensusState::new(vs));
        let block_hash = B256::repeat_byte(0xDD);
        state.notify_block_committed(block_hash, 10);

        let sk = BlsSecretKey::random().unwrap();
        // Authorize the verifier to simulate a completed QUIC handshake.
        state.authorize_verifier(sk.public_key().to_bytes());

        let rpc = N42RpcServer::new(state);
        let sig = sk.sign(block_hash.as_slice());

        let result = rpc
            .submit_attestation(
                hex::encode(sk.public_key().to_bytes()),
                hex::encode(sig.to_bytes()),
                block_hash,
                10,
            )
            .await;
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert!(resp.accepted);
        assert_eq!(resp.attestation_count, 1);
    }

    #[tokio::test]
    async fn test_submit_attestation_unauthorized_pubkey() {
        // A valid BLS signature from an unregistered pubkey must be rejected.
        let vs = ValidatorSet::new(&[], 0);
        let state = Arc::new(SharedConsensusState::new(vs));
        let block_hash = B256::repeat_byte(0xDE);
        state.notify_block_committed(block_hash, 11);
        // Deliberately NOT calling state.authorize_verifier(...).

        let rpc = N42RpcServer::new(state);
        let sk = BlsSecretKey::random().unwrap();
        let sig = sk.sign(block_hash.as_slice());

        let result = rpc
            .submit_attestation(
                hex::encode(sk.public_key().to_bytes()),
                hex::encode(sig.to_bytes()),
                block_hash,
                11,
            )
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), -32004, "unauthorized verifier must return -32004");
        assert!(err.message().contains("not authorized"));
    }

    #[tokio::test]
    async fn test_block_attestation_lookup() {
        let vs = ValidatorSet::new(&[], 0);
        let state = Arc::new(SharedConsensusState::new(vs));
        let hash = B256::repeat_byte(0xEE);
        state.record_attestation(hash, 5, 3);

        let rpc = N42RpcServer::new(state);
        let record = rpc.block_attestation(hash).await.unwrap();
        assert!(record.is_some());
        let r = record.unwrap();
        assert_eq!(r.block_number, 5);
        assert_eq!(r.valid_count, 3);

        assert!(rpc.block_attestation(B256::ZERO).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_equivocations_with_data() {
        let vs = ValidatorSet::new(&[], 0);
        let state = Arc::new(SharedConsensusState::new(vs));
        state.record_equivocation(10, 0, B256::repeat_byte(0xAA), B256::repeat_byte(0xBB));
        state.record_equivocation(11, 1, B256::repeat_byte(0xCC), B256::repeat_byte(0xDD));

        let rpc = N42RpcServer::new(state);
        let result = rpc.equivocations().await.unwrap();
        assert_eq!(result.total, 2);
        assert_eq!(result.evidence[0].view, 10);
        assert_eq!(result.evidence[1].validator_index, 1);
    }
}
