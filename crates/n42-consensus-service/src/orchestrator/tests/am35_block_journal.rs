//! AM35 through the orchestrator: an R2 vote's block is journalled before
//! the vote, and a whole-set kill between the PrepareQC and the CommitQC
//! (lock durable, no CommitQC, no EL copy of the block) is survived by the
//! restart replay: the EL gets the locked block back and the leader builds
//! on it.

use super::*;
use crate::block_journal::{BLOCK_JOURNAL_FILE, BlockJournal, FnPersistedChain};
use crate::persistence::{FileVoteLog, load_durable_lock};
use n42_consensus::ConsensusEvent;
use n42_primitives::consensus::PrepareQC;

/// The EL's persisted chain in these tests: only `head` at height 0.
fn persisted_at_genesis(head: B256) -> Arc<dyn crate::block_journal::PersistedChain> {
    Arc::new(FnPersistedChain::new(
        || Some(0),
        move |number| (number == 0).then_some(head),
    ))
}

fn open_journal(dir: &std::path::Path, head: B256) -> Arc<BlockJournal> {
    let (journal, _) = BlockJournal::open(dir.join(BLOCK_JOURNAL_FILE)).expect("journal opens");
    journal.set_persisted_chain(persisted_at_genesis(head));
    Arc::new(journal)
}

fn prepare_qc_for(view: u64, block_hash: B256, signers: &[u8]) -> PrepareQC {
    let message = n42_consensus::protocol::quorum::signing_message(view, &block_hash);
    let signatures: Vec<_> = signers
        .iter()
        .map(|i| test_key(*i).sign(&message))
        .collect();
    let refs: Vec<_> = signatures.iter().collect();
    let mut signer_bits = QuorumCertificate::genesis().signers;
    signer_bits.resize(4, false);
    for i in signers {
        signer_bits.set(*i as usize, true);
    }
    PrepareQC {
        view,
        block_hash,
        qc: QuorumCertificate {
            view,
            block_hash,
            aggregate_signature: n42_primitives::bls::AggregateSignature::aggregate(&refs)
                .expect("aggregate"),
            signers: signer_bits,
        },
    }
}

/// Phase 1 is the live view up to the kill: a follower receives B's data and
/// the PrepareQC for B, journals B, writes its lock and sends its R2 vote.
/// No CommitQC arrives and nothing else is saved: the whole set dies there.
/// Phase 2 restarts with an execution layer that never saw B.
#[tokio::test]
async fn whole_set_kill_between_prepare_qc_and_commit_qc_replays_the_locked_block() {
    let dir = tempfile::tempdir().unwrap();
    let head = B256::repeat_byte(0x10);
    let locked = B256::repeat_byte(0xB1);
    let vote_log_path = dir.path().join("vote_log.bin");

    // ---- Phase 1: live, up to the kill ----
    {
        let journal = open_journal(dir.path(), head);
        let (mut engine, output_rx) = make_test_engine_with_validator_count(4, 0);
        engine.set_vote_log(Arc::new(
            FileVoteLog::open(vote_log_path.clone())
                .unwrap()
                .with_block_journal(Arc::clone(&journal)),
        ));
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(64);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx)
            .with_block_journal(Arc::clone(&journal));
        orch.head_block_hash = head;

        // The PrepareQC overtakes B's data: the R2 vote is held, nothing is
        // locked on disk and no vote leaves.
        orch.engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::PrepareQC(
                prepare_qc_for(1, locked, &[1, 2, 3]),
            )))
            .unwrap();
        let early: Vec<_> = std::iter::from_fn(|| orch.output_rx.try_recv().ok()).collect();
        assert!(
            !early.iter().any(|o| matches!(
                o,
                EngineOutput::SendToValidator(_, ConsensusMessage::CommitVote(_))
            )),
            "no R2 vote before the block is durable"
        );
        assert!(load_durable_lock(&vote_log_path).unwrap().is_none());
        assert!(!journal.is_durable(&locked));
        // The in-memory lock already names B, but a snapshot taken now (any
        // of the save_consensus_state call sites) must not: B is on no disk.
        assert_eq!(orch.engine.locked_qc().block_hash, locked);
        assert_eq!(
            orch.build_snapshot().locked_qc.block_hash,
            B256::ZERO,
            "the snapshot stores only the durable lock"
        );

        // B's data arrives: it is staged, journalled and synced, and only then
        // are the lock and the R2 vote written and sent.
        orch.handle_block_data(test_block_data(head, locked, 1))
            .await;
        assert!(journal.is_durable(&locked), "B is in the journal");
        let outputs: Vec<_> = std::iter::from_fn(|| orch.output_rx.try_recv().ok()).collect();
        assert!(
            outputs.iter().any(|o| matches!(
                o,
                EngineOutput::SendToValidator(_, ConsensusMessage::CommitVote(cv))
                    if cv.block_hash == locked && cv.view == 1
            )),
            "the R2 vote leaves once B is durable"
        );
        assert_eq!(
            load_durable_lock(&vote_log_path)
                .unwrap()
                .map(|qc| qc.block_hash),
            Some(locked)
        );
        assert_eq!(orch.build_snapshot().locked_qc.block_hash, locked);
        // Kill: the process ends here. No CommitQC, no snapshot, and the
        // execution layer never persisted B.
    }

    // ---- Phase 2: restart ----
    let lock = load_durable_lock(&vote_log_path)
        .unwrap()
        .expect("durable lock on B");
    assert_eq!(lock.block_hash, locked);
    let journal = open_journal(dir.path(), head);
    assert!(
        journal.is_durable(&locked),
        "B survived the kill in the journal"
    );
    let (engine, output_rx) = make_test_engine_locked_on(lock.block_hash);
    let (network, _cmd_rx, _prx) = make_test_network();
    let (_net_event_tx, net_event_rx) = mpsc::channel(64);
    let mock_el = Arc::new(MockExecutionLayer::new(
        PayloadStatusEnum::Valid,
        PayloadStatusEnum::Valid,
    ));
    let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx)
        .with_block_journal(Arc::clone(&journal));
    orch.el = Some(mock_el.clone());
    orch.head_block_hash = head;
    orch.head_block_number = 0;

    assert_eq!(orch.replay_block_journal().await.unwrap(), 1);
    assert_eq!(
        mock_el.new_payload_hashes(),
        vec![locked],
        "B handed back to the EL"
    );
    assert!(
        !mock_el.fcu_heads().contains(&locked),
        "B is only locked, not committed: no forkchoice to it"
    );
    assert!(
        orch.pending_block_data.contains_key(&locked),
        "B's data is held for serving and finalization"
    );

    // The leader builds on the locked block, which the EL now has.
    orch.do_trigger_payload_build(None).await;
    assert_eq!(mock_el.build_parents(), vec![locked]);
}

/// A replayed block the consensus state already committed is made
/// canonical: forkchoice moves to it, and no further.
#[tokio::test]
async fn replay_moves_forkchoice_only_to_the_committed_block() {
    let dir = tempfile::tempdir().unwrap();
    let head = B256::repeat_byte(0x10);
    let committed = B256::repeat_byte(0xC1);
    let locked_child = B256::repeat_byte(0xC2);
    {
        let journal = open_journal(dir.path(), head);
        journal.stage_data(
            committed,
            1,
            Arc::new(test_block_data_at_number(head, committed, 1, 1)),
        );
        journal.stage_data(
            locked_child,
            2,
            Arc::new(test_block_data_at_number(committed, locked_child, 2, 2)),
        );
        assert!(matches!(
            journal.ensure_durable(locked_child).unwrap(),
            crate::block_journal::Durability::Durable(stats) if stats.blocks == 2
        ));
    }

    let journal = open_journal(dir.path(), head);
    let sk = test_key(0x21);
    let validator_set = ValidatorSet::new(
        &[ValidatorInfo {
            address: Address::with_last_byte(1),
            bls_public_key: sk.public_key(),
            p2p_peer_id: None,
        }],
        0,
    );
    let (output_tx, output_rx) = mpsc::channel(1024);
    let mut locked_qc = QuorumCertificate::genesis();
    locked_qc.view = 2;
    locked_qc.block_hash = locked_child;
    let mut committed_qc = QuorumCertificate::genesis();
    committed_qc.view = 1;
    committed_qc.block_hash = committed;
    let engine = ConsensusEngine::with_recovered_state(
        0,
        sk,
        EpochManager::new(validator_set),
        60_000,
        120_000,
        output_tx,
        3,
        locked_qc,
        committed_qc,
        0,
        0,
    );
    let (network, _cmd_rx, _prx) = make_test_network();
    let (_net_event_tx, net_event_rx) = mpsc::channel(64);
    let mock_el = Arc::new(MockExecutionLayer::new(
        PayloadStatusEnum::Valid,
        PayloadStatusEnum::Valid,
    ));
    let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx)
        .with_block_journal(journal);
    orch.el = Some(mock_el.clone());
    orch.head_block_hash = head;
    orch.head_block_number = 0;

    assert_eq!(orch.replay_block_journal().await.unwrap(), 2);
    assert_eq!(mock_el.new_payload_hashes(), vec![committed, locked_child]);
    assert_eq!(
        mock_el.fcu_heads(),
        vec![committed],
        "forkchoice to the committed block only"
    );
    assert_eq!(orch.head_block_hash, committed);
}

/// Journals `blocks` (parent, hash, number, view) with the EL persisted at
/// `persisted`, as a node does before its R2 votes.
fn journal_blocks(dir: &std::path::Path, persisted: Vec<B256>, blocks: &[(B256, B256, u64, u64)]) {
    let (journal, _) = BlockJournal::open(dir.join(BLOCK_JOURNAL_FILE)).unwrap();
    journal.set_persisted_chain(persisted_chain(persisted));
    for (parent, hash, number, view) in blocks.iter().copied() {
        journal.stage_data(
            hash,
            view,
            Arc::new(test_block_data_at_number(parent, hash, view, number)),
        );
    }
    let tip = blocks.last().expect("blocks").1;
    assert!(matches!(
        journal.ensure_durable(tip).unwrap(),
        crate::block_journal::Durability::Durable(_)
    ));
}

fn persisted_chain(chain: Vec<B256>) -> Arc<dyn crate::block_journal::PersistedChain> {
    Arc::new(FnPersistedChain::new(
        {
            let len = chain.len() as u64;
            move || len.checked_sub(1)
        },
        move |number| chain.get(number as usize).copied(),
    ))
}

/// A restarted orchestrator locked on `locked` with the EL at `head`.
fn restarted(
    dir: &std::path::Path,
    persisted: Vec<B256>,
    locked: B256,
    mock_el: Arc<MockExecutionLayer>,
) -> ConsensusService {
    let (journal, _) = BlockJournal::open(dir.join(BLOCK_JOURNAL_FILE)).unwrap();
    let head = *persisted.last().expect("persisted head");
    let head_number = persisted.len() as u64 - 1;
    journal.set_persisted_chain(persisted_chain(persisted));
    let (engine, output_rx) = make_test_engine_locked_on(locked);
    let (network, _cmd_rx, _prx) = make_test_network();
    let (_net_event_tx, net_event_rx) = mpsc::channel(64);
    let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx)
        .with_block_journal(Arc::new(journal));
    orch.el = Some(mock_el);
    orch.head_block_hash = head;
    orch.head_block_number = head_number;
    orch
}

/// A transient EL answer (SYNCING) is retried; the locked block replays.
#[tokio::test]
async fn replay_retries_a_transient_execution_layer_failure() {
    let dir = tempfile::tempdir().unwrap();
    let head = B256::repeat_byte(0x10);
    let locked = B256::repeat_byte(0xB1);
    journal_blocks(dir.path(), vec![head], &[(head, locked, 1, 1)]);
    let mock_el = Arc::new(MockExecutionLayer::with_new_payload_statuses(
        [
            PayloadStatusEnum::Syncing,
            PayloadStatusEnum::Syncing,
            PayloadStatusEnum::Valid,
        ],
        PayloadStatusEnum::Valid,
    ));
    let mut orch = restarted(dir.path(), vec![head], locked, mock_el.clone());
    assert_eq!(orch.replay_block_journal().await.unwrap(), 1);
    assert_eq!(mock_el.new_payload_hashes(), vec![locked, locked, locked]);
}

/// Blocker 3: the locked block (through its journalled parent) cannot be
/// replayed: the replay fails closed instead of letting the node vote or
/// build without it. An unrelated journalled block failing is tolerated.
#[tokio::test]
async fn replay_fails_closed_when_the_locked_block_cannot_be_replayed() {
    let dir = tempfile::tempdir().unwrap();
    let head = B256::repeat_byte(0x10);
    let parent = B256::repeat_byte(0xA1);
    let locked = B256::repeat_byte(0xB2);
    journal_blocks(
        dir.path(),
        vec![head],
        &[(head, parent, 1, 1), (parent, locked, 2, 2)],
    );
    // The parent is INVALID (final, not retried): the locked child cannot
    // follow it.
    let mock_el = Arc::new(MockExecutionLayer::with_new_payload_statuses(
        [PayloadStatusEnum::Invalid {
            validation_error: "test".to_string(),
        }],
        PayloadStatusEnum::Valid,
    ));
    let mut orch = restarted(dir.path(), vec![head], locked, mock_el.clone());
    let failure = orch.replay_block_journal().await.unwrap_err();
    assert_eq!(
        (failure.hash, failure.number, failure.role),
        (parent, 1, "locked")
    );
    assert_eq!(
        mock_el.new_payload_hashes(),
        vec![parent],
        "INVALID is not retried"
    );
    assert!(failure.to_string().contains("could not be replayed"));

    // The same failure on a journalled block nothing rests on is logged and
    // skipped: the lock is on the EL head.
    let mock_el = Arc::new(MockExecutionLayer::with_new_payload_statuses(
        [PayloadStatusEnum::Invalid {
            validation_error: "test".to_string(),
        }],
        PayloadStatusEnum::Valid,
    ));
    let mut orch = restarted(dir.path(), vec![head], head, mock_el);
    assert_eq!(orch.replay_block_journal().await.unwrap(), 0);
}

/// Blocker 2: replay matches the EL by hash. A journalled block at a height
/// reth persisted under another hash (a fork sibling) is replayed, not
/// skipped; the block reth persisted itself is skipped.
#[tokio::test]
async fn replay_matches_persisted_blocks_by_hash_not_height() {
    let dir = tempfile::tempdir().unwrap();
    let head = B256::repeat_byte(0x10);
    let persisted_one = B256::repeat_byte(0x11);
    let sibling = B256::repeat_byte(0x12);
    // Journalled while only `head` was persisted.
    journal_blocks(dir.path(), vec![head], &[(head, persisted_one, 1, 1)]);
    journal_blocks(dir.path(), vec![head], &[(head, sibling, 1, 2)]);
    // reth then persisted `persisted_one` at height 1; the lock is on the
    // sibling (not finalized: no commit at height 1).
    let mock_el = Arc::new(MockExecutionLayer::new(
        PayloadStatusEnum::Valid,
        PayloadStatusEnum::Valid,
    ));
    let mut orch = restarted(
        dir.path(),
        vec![head, persisted_one],
        sibling,
        mock_el.clone(),
    );
    assert_eq!(orch.replay_block_journal().await.unwrap(), 1);
    assert_eq!(mock_el.new_payload_hashes(), vec![sibling]);
}

/// Minor 4: through `run()`, the real entry point. The journalled locked
/// block reaches the EL before the leader's first build (the startup
/// schedule and the event loop come after the replay).
#[tokio::test]
async fn run_replays_the_journal_before_the_first_build() {
    let dir = tempfile::tempdir().unwrap();
    let head = B256::repeat_byte(0x10);
    let locked = B256::repeat_byte(0xB1);
    journal_blocks(dir.path(), vec![head], &[(head, locked, 1, 1)]);
    let mock_el = Arc::new(MockExecutionLayer::new(
        PayloadStatusEnum::Valid,
        PayloadStatusEnum::Valid,
    ));
    let orch = restarted(dir.path(), vec![head], locked, mock_el.clone());
    let task = tokio::spawn(orch.run());
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !mock_el.calls().iter().any(|(kind, _)| *kind == "build") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the leader never built: {:?}",
            mock_el.calls()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    task.abort();
    let calls = mock_el.calls();
    assert_eq!(
        &calls[..2],
        &[("new_payload", locked), ("build", locked)],
        "replay first, then the build on the replayed locked block"
    );
}

/// Through `run()`: a locked block that cannot be replayed stops the node
/// (the orchestrator task panics, which ends the node as a critical task)
/// before anything is built.
#[tokio::test]
async fn run_stops_before_the_event_loop_when_the_locked_block_cannot_be_replayed() {
    let dir = tempfile::tempdir().unwrap();
    let head = B256::repeat_byte(0x10);
    let locked = B256::repeat_byte(0xB1);
    journal_blocks(dir.path(), vec![head], &[(head, locked, 1, 1)]);
    let mock_el = Arc::new(MockExecutionLayer::with_new_payload_statuses(
        [PayloadStatusEnum::Invalid {
            validation_error: "test".to_string(),
        }],
        PayloadStatusEnum::Valid,
    ));
    let orch = restarted(dir.path(), vec![head], locked, mock_el.clone());
    let joined = tokio::time::timeout(std::time::Duration::from_secs(10), tokio::spawn(orch.run()))
        .await
        .expect("run() stops instead of entering the event loop");
    let panic = joined.expect_err("run() panics").into_panic();
    let message = panic.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(
        message.contains("AM35") && message.contains("locked"),
        "{message}"
    );
    assert!(mock_el.build_parents().is_empty(), "nothing was built");
}
