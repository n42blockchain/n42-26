use crate::mobile_bridge::AttestationEvent;
use n42_jmt::{EvidenceStore, MobileEvidence};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, warn};

const DEFAULT_MOBILE_EVIDENCE_WRITE_ATTEMPTS: usize = 5;
const DEFAULT_MOBILE_EVIDENCE_WRITE_RETRY_DELAY: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy)]
pub struct MobileEvidenceWriteConfig {
    pub max_attempts: usize,
    pub retry_delay: Duration,
}

impl Default for MobileEvidenceWriteConfig {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MOBILE_EVIDENCE_WRITE_ATTEMPTS,
            retry_delay: DEFAULT_MOBILE_EVIDENCE_WRITE_RETRY_DELAY,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobileEvidenceWriteOutcome {
    Written,
    AlreadyPresent,
    MissingConsensusEvidence,
    BlockHashMismatch,
    InvalidEvent,
    GetFailed,
    PutFailed,
    JoinFailed,
}

/// Spawns the bounded, async write-back path for a completed mobile aggregate.
///
/// The mobile bitfield is indexed by the `AttestationStore` verifier registry's
/// registration order. This helper only merges it into the already-persisted
/// committee `ConsensusEvidence` row after verifying the block hash matches.
pub fn spawn_mobile_evidence_writeback(
    evidence_store: Arc<EvidenceStore>,
    event: AttestationEvent,
) {
    spawn_mobile_evidence_writeback_with_config(
        evidence_store,
        event,
        MobileEvidenceWriteConfig::default(),
    );
}

pub fn spawn_mobile_evidence_writeback_with_config(
    evidence_store: Arc<EvidenceStore>,
    event: AttestationEvent,
    config: MobileEvidenceWriteConfig,
) {
    std::mem::drop(tokio::spawn(async move {
        let _ = write_mobile_evidence_with_retry(evidence_store, event, config).await;
    }));
}

pub async fn run_mobile_evidence_writeback(
    rx: mpsc::Receiver<AttestationEvent>,
    evidence_store: Arc<EvidenceStore>,
) {
    run_mobile_evidence_writeback_with_config(
        rx,
        evidence_store,
        MobileEvidenceWriteConfig::default(),
    )
    .await;
}

pub async fn run_mobile_evidence_writeback_with_config(
    mut rx: mpsc::Receiver<AttestationEvent>,
    evidence_store: Arc<EvidenceStore>,
    config: MobileEvidenceWriteConfig,
) {
    while let Some(event) = rx.recv().await {
        spawn_mobile_evidence_writeback_with_config(Arc::clone(&evidence_store), event, config);
    }
}

pub async fn write_mobile_evidence_with_retry(
    evidence_store: Arc<EvidenceStore>,
    event: AttestationEvent,
    config: MobileEvidenceWriteConfig,
) -> MobileEvidenceWriteOutcome {
    let max_attempts = config.max_attempts.max(1);

    for attempt in 1..=max_attempts {
        let store = Arc::clone(&evidence_store);
        let event = event.clone();
        let outcome =
            match tokio::task::spawn_blocking(move || write_mobile_evidence_once(store, event))
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    warn!(
                        target: "n42::cl::evidence",
                        error = %error,
                        "mobile evidence write-back task failed"
                    );
                    return MobileEvidenceWriteOutcome::JoinFailed;
                }
            };

        if outcome != MobileEvidenceWriteOutcome::MissingConsensusEvidence {
            return outcome;
        }

        if attempt < max_attempts {
            tokio::time::sleep(config.retry_delay).await;
        }
    }

    debug!(
        target: "n42::cl::evidence",
        block_number = event.block_number,
        %event.block_hash,
        attempts = max_attempts,
        "consensus evidence row absent after bounded mobile write-back retry; dropping mobile evidence"
    );
    MobileEvidenceWriteOutcome::MissingConsensusEvidence
}

fn write_mobile_evidence_once(
    evidence_store: Arc<EvidenceStore>,
    event: AttestationEvent,
) -> MobileEvidenceWriteOutcome {
    let set_bits: u32 = event
        .packed_participants
        .iter()
        .map(|byte| byte.count_ones())
        .sum();
    if event.valid_count != u32::from(event.participant_count)
        || set_bits != u32::from(event.participant_count)
        || event.packed_participants.last() == Some(&0)
    {
        warn!(
            target: "n42::cl::evidence",
            block_number = event.block_number,
            %event.block_hash,
            valid_count = event.valid_count,
            participant_count = event.participant_count,
            set_bits,
            bitfield_bytes = event.packed_participants.len(),
            "mobile evidence event has a non-canonical participant bitfield; dropping"
        );
        return MobileEvidenceWriteOutcome::InvalidEvent;
    }

    match evidence_store.get(event.block_number) {
        Ok(Some(mut evidence)) => {
            if evidence.block_hash != event.block_hash.0 {
                warn!(
                    target: "n42::cl::evidence",
                    block_number = event.block_number,
                    event_block_hash = %event.block_hash,
                    stored_block_hash = %alloy_primitives::B256::from(evidence.block_hash),
                    "mobile evidence block hash mismatch; dropping"
                );
                return MobileEvidenceWriteOutcome::BlockHashMismatch;
            }

            if evidence.mobile.is_some() {
                return MobileEvidenceWriteOutcome::AlreadyPresent;
            }

            evidence.mobile = Some(MobileEvidence {
                receipts_root: event.receipts_root,
                aggregate_signature: event.aggregate_signature,
                participant_count: event.participant_count,
                packed_participants: event.packed_participants,
                created_at_ms: event.created_at_ms,
            });

            match evidence_store.put(event.block_number, &evidence) {
                Ok(()) => MobileEvidenceWriteOutcome::Written,
                Err(error) => {
                    warn!(
                        target: "n42::cl::evidence",
                        block_number = event.block_number,
                        %event.block_hash,
                        error = %error,
                        "mobile evidence write-back failed"
                    );
                    MobileEvidenceWriteOutcome::PutFailed
                }
            }
        }
        Ok(None) => MobileEvidenceWriteOutcome::MissingConsensusEvidence,
        Err(error) => {
            warn!(
                target: "n42::cl::evidence",
                block_number = event.block_number,
                %event.block_hash,
                error = %error,
                "mobile evidence get failed"
            );
            MobileEvidenceWriteOutcome::GetFailed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use n42_jmt::ConsensusEvidence;

    fn sample_consensus_evidence(block_hash: B256) -> ConsensusEvidence {
        ConsensusEvidence {
            view: 7,
            block_hash: block_hash.0,
            aggregate_signature: [0xAA; 96],
            signer_count: 3,
            packed_signers: vec![0b0000_0111],
            mobile: None,
        }
    }

    fn sample_event(block_hash: B256, block_number: u64) -> AttestationEvent {
        AttestationEvent {
            block_hash,
            block_number,
            receipts_root: [0xBB; 32],
            aggregate_signature: [0xCC; 96],
            participant_count: 21,
            packed_participants: vec![0xFF, 0xFF, 0x1F],
            created_at_ms: 1_700_000_000_000,
            valid_count: 21,
        }
    }

    fn open_store() -> (tempfile::TempDir, Arc<EvidenceStore>) {
        let dir = tempfile::tempdir().unwrap();
        let env = n42_jmt::open_jmt_env(dir.path()).unwrap();
        let store = EvidenceStore::open(env).unwrap();
        (dir, Arc::new(store))
    }

    #[tokio::test]
    async fn writeback_sets_mobile_and_is_idempotent() {
        let (_dir, store) = open_store();
        let block_number = 42;
        let block_hash = B256::from([0x11; 32]);
        store
            .put(block_number, &sample_consensus_evidence(block_hash))
            .unwrap();

        let event = sample_event(block_hash, block_number);
        let config = MobileEvidenceWriteConfig {
            max_attempts: 1,
            retry_delay: Duration::from_millis(1),
        };

        let outcome =
            write_mobile_evidence_with_retry(Arc::clone(&store), event.clone(), config).await;
        assert_eq!(outcome, MobileEvidenceWriteOutcome::Written);

        let loaded = store.get(block_number).unwrap().unwrap();
        let mobile = loaded.mobile.expect("mobile evidence should be set");
        assert_eq!(mobile.receipts_root, event.receipts_root);
        assert_eq!(mobile.aggregate_signature, event.aggregate_signature);
        assert_eq!(mobile.participant_count, event.participant_count);
        assert_eq!(mobile.packed_participants, event.packed_participants);
        assert_eq!(mobile.created_at_ms, event.created_at_ms);

        let outcome = write_mobile_evidence_with_retry(store, event, config).await;
        assert_eq!(outcome, MobileEvidenceWriteOutcome::AlreadyPresent);
    }

    #[tokio::test]
    async fn writeback_preserves_sparse_high_registry_index() {
        let (_dir, store) = open_store();
        let block_number = 43;
        let block_hash = B256::from([0x12; 32]);
        store
            .put(block_number, &sample_consensus_evidence(block_hash))
            .unwrap();

        let mut event = sample_event(block_hash, block_number);
        event.participant_count = 1;
        event.valid_count = 1;
        event.packed_participants = vec![0u8; 1_250];
        event.packed_participants[9_999 / 8] |= 1 << (9_999 % 8);

        let outcome = write_mobile_evidence_with_retry(
            Arc::clone(&store),
            event.clone(),
            MobileEvidenceWriteConfig {
                max_attempts: 1,
                retry_delay: Duration::from_millis(1),
            },
        )
        .await;
        assert_eq!(outcome, MobileEvidenceWriteOutcome::Written);

        let mobile = store.get(block_number).unwrap().unwrap().mobile.unwrap();
        assert_eq!(mobile.packed_participants, event.packed_participants);
        assert_ne!(mobile.packed_participants[9_999 / 8], 0);
    }

    #[tokio::test]
    async fn writeback_rejects_participant_count_bitfield_mismatch() {
        let (_dir, store) = open_store();
        let block_number = 44;
        let block_hash = B256::from([0x13; 32]);
        store
            .put(block_number, &sample_consensus_evidence(block_hash))
            .unwrap();

        let mut event = sample_event(block_hash, block_number);
        event.packed_participants[2] = 0x0F; // 20 set bits, count still claims 21.
        let outcome = write_mobile_evidence_with_retry(
            Arc::clone(&store),
            event,
            MobileEvidenceWriteConfig {
                max_attempts: 1,
                retry_delay: Duration::from_millis(1),
            },
        )
        .await;
        assert_eq!(outcome, MobileEvidenceWriteOutcome::InvalidEvent);
        assert!(store.get(block_number).unwrap().unwrap().mobile.is_none());
    }

    #[tokio::test]
    async fn writeback_retries_absent_block_then_drops() {
        let (_dir, store) = open_store();
        let event = sample_event(B256::from([0x22; 32]), 99);
        let config = MobileEvidenceWriteConfig {
            max_attempts: 2,
            retry_delay: Duration::from_millis(1),
        };

        let outcome = write_mobile_evidence_with_retry(store, event, config).await;
        assert_eq!(
            outcome,
            MobileEvidenceWriteOutcome::MissingConsensusEvidence
        );
    }

    #[tokio::test]
    async fn writeback_consumer_processes_events() {
        let (_dir, store) = open_store();
        let block_number = 77;
        let block_hash = B256::from([0x33; 32]);
        store
            .put(block_number, &sample_consensus_evidence(block_hash))
            .unwrap();

        let (tx, rx) = mpsc::channel(4);
        let config = MobileEvidenceWriteConfig {
            max_attempts: 1,
            retry_delay: Duration::from_millis(1),
        };
        let consumer_store = Arc::clone(&store);
        let consumer = tokio::spawn(async move {
            run_mobile_evidence_writeback_with_config(rx, consumer_store, config).await;
        });

        tx.send(sample_event(block_hash, block_number))
            .await
            .unwrap();
        drop(tx);

        for _ in 0..20 {
            if store.get(block_number).unwrap().unwrap().mobile.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(store.get(block_number).unwrap().unwrap().mobile.is_some());
        consumer.await.unwrap();
    }
}
