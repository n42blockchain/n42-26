use crate::consensus_state::SharedConsensusState;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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

/// N42-specific RPC API.
#[rpc(server, namespace = "n42")]
pub trait N42Api {
    /// Returns the current consensus status.
    #[method(name = "consensusStatus")]
    async fn consensus_status(&self) -> RpcResult<ConsensusStatusResponse>;

    /// Returns the current validator set.
    #[method(name = "validatorSet")]
    async fn validator_set(&self) -> RpcResult<Vec<ValidatorInfoResponse>>;
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
}
