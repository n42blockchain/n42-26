//! An [`ExecutionLayer`] for a validator key that shares its machine with
//! the key owning the execution layer ("import once per host").
//!
//! One node on the machine (the owner) builds and executes; the other keys
//! of the same operator run with `N42_EL_OWNER_RPC=127.0.0.1:<owner http
//! port>` and do not execute. Such a key reports a block as valid once the
//! owner's chain holds it, and never builds: until payload building is
//! routed to the owner, leadership has to stay with the owner's key.
//!
//! Scope: keys of one operator on one machine. A key configured this way
//! adds a signature to the owner's check, not an independent execution.

use super::{BuiltBlock, ElError, ExecutionLayer, ResolveKind};
use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionData, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadId, PayloadStatus,
    PayloadStatusEnum,
};
use std::{net::SocketAddr, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How often the owner is asked again while it does not hold a block yet.
const POLL: Duration = Duration::from_millis(5);

/// Defers every execution question to the owner's node.
pub struct DelegatedExecutionLayer {
    owner: SocketAddr,
    wait: Duration,
    /// With the frame queue: takes committed blocks' transactions out of
    /// this key's queue, which no canonical chain of its own drains.
    committed: Option<std::sync::Arc<super::committed_frames::CommittedFrames>>,
}

impl DelegatedExecutionLayer {
    /// The layer `N42_EL_OWNER_RPC` asks for, if set. A value that is not a
    /// socket address is a configuration error, reported to the caller.
    pub fn configured() -> Result<Option<Self>, ElError> {
        let Ok(owner) = std::env::var("N42_EL_OWNER_RPC") else {
            return Ok(None);
        };
        let owner = owner
            .parse()
            .map_err(|error| ElError(format!("N42_EL_OWNER_RPC={owner}: {error}")))?;
        let wait = std::env::var("N42_EL_OWNER_WAIT_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(2_000);
        Ok(Some(Self {
            owner,
            wait: Duration::from_millis(wait),
            committed: crate::frame_queue::frame_queue_enabled().then(Default::default),
        }))
    }

    /// Whether the owner's canonical chain holds `hash`.
    async fn owner_holds(&self, hash: B256) -> Result<bool, ElError> {
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"eth_getHeaderByHash","params":["{hash:?}"]}}"#
        );
        let request = format!(
            "POST / HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.owner,
            body.len(),
        );
        let failed = |error: std::io::Error| ElError(format!("execution owner: {error}"));
        let mut response = Vec::with_capacity(2048);
        // An owner that accepts the connection and then stalls must not hold
        // this key's engine calls open: one exchange gets the whole wait.
        tokio::time::timeout(self.wait, async {
            let mut stream = tokio::net::TcpStream::connect(self.owner).await?;
            stream.write_all(request.as_bytes()).await?;
            stream.read_to_end(&mut response).await
        })
        .await
        .map_err(|_| ElError(format!("execution owner: no answer in {:?}", self.wait)))?
        .map_err(failed)?;
        let response = String::from_utf8_lossy(&response);
        let Some((_, answer)) = response.split_once("\r\n\r\n") else {
            return Err(ElError("execution owner: malformed HTTP response".into()));
        };
        if answer.contains(r#""result":null"#) {
            Ok(false)
        } else if answer.contains(r#""result":{"#) {
            Ok(true)
        } else {
            Err(ElError(format!("execution owner: unexpected answer {answer:.200}")))
        }
    }
}

#[async_trait::async_trait]
impl ExecutionLayer for DelegatedExecutionLayer {
    async fn new_payload(&self, payload: ExecutionData) -> Result<PayloadStatus, ElError> {
        let hash = payload.block_hash();
        let started = std::time::Instant::now();
        // The block's transaction hashes, derived while the owner is asked.
        let imported = self.committed.is_some().then(|| {
            tokio::task::spawn_blocking(move || {
                super::committed_frames::ImportedBlock::of(&payload)
            })
        });
        loop {
            if self.owner_holds(hash).await? {
                metrics::histogram!("n42_delegated_el_wait_ms")
                    .record(started.elapsed().as_secs_f64() * 1000.0);
                if let (Some(committed), Some(imported)) = (&self.committed, imported)
                    && let Ok(block) = imported.await
                {
                    committed.imported(block);
                }
                return Ok(PayloadStatus::new(PayloadStatusEnum::Valid, Some(hash)));
            }
            if started.elapsed() >= self.wait {
                metrics::counter!("n42_delegated_el_timeout_total").increment(1);
                return Ok(PayloadStatus::from_status(PayloadStatusEnum::Syncing));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn fork_choice_updated(
        &self,
        state: ForkchoiceState,
    ) -> Result<ForkchoiceUpdated, ElError> {
        // The owner moves its own head; this key has no chain to move.
        let status = if self.owner_holds(state.head_block_hash).await? {
            // Only the finalized block is committed; the head may run ahead.
            if let Some(committed) = self.committed.clone()
                && !state.finalized_block_hash.is_zero()
            {
                let head = state.finalized_block_hash;
                tokio::task::spawn_blocking(move || {
                    committed.committed(
                        crate::frame_queue::FrameQueue::global(),
                        n42_tx_types::FrameStore::global(),
                        head,
                    )
                });
            }
            PayloadStatus::new(PayloadStatusEnum::Valid, Some(state.head_block_hash))
        } else {
            PayloadStatus::from_status(PayloadStatusEnum::Syncing)
        };
        Ok(ForkchoiceUpdated::new(status))
    }

    async fn fork_choice_updated_with_attrs(
        &self,
        _state: ForkchoiceState,
        _attrs: PayloadAttributes,
    ) -> Result<ForkchoiceUpdated, ElError> {
        Err(ElError("this key does not own the execution layer and cannot build".into()))
    }

    async fn resolve_payload(
        &self,
        _id: PayloadId,
        _kind: ResolveKind,
    ) -> Option<Result<BuiltBlock, ElError>> {
        None
    }
}

#[cfg(test)]
mod stalled_owner_tests {
    use super::*;

    /// An owner that accepts the connection and never answers ends the
    /// exchange with an error once the wait has passed.
    #[tokio::test]
    async fn an_owner_that_accepts_and_stalls_is_given_up_on() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let owner = listener.local_addr().unwrap();
        // Hold accepted connections open without reading or writing.
        let held = tokio::spawn(async move {
            let mut open = Vec::new();
            loop {
                if let Ok((stream, _)) = listener.accept().await {
                    open.push(stream);
                }
            }
        });
        let layer = DelegatedExecutionLayer {
            owner,
            wait: Duration::from_millis(100),
            committed: None,
        };
        let answer = tokio::time::timeout(Duration::from_secs(5), layer.owner_holds(B256::ZERO))
            .await
            .expect("the exchange must end on its own");
        assert!(answer.unwrap_err().0.contains("no answer"));
        held.abort();
    }
}
