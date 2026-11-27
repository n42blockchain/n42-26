# devlog-198: restart with reth's persisted head above the consensus snapshot (2026-11-27)

## 1. Problem (E9 kill 10, n42-26 v3 on 72facd3)

- Before the kill, v3's snapshot saves were being refused by the AM35 durable-lock bug (fixed separately in PR #57). The last good snapshot named block 91, while reth (persistence threshold 0) had already persisted 92–94.
- At restart, `main.rs` saw reth's head (94) differ from the snapshot's validated head (91) and rewound the consensus head to their highest common ancestor, 91. That path was written for a crash on an uncommitted side proposal.
- Catch-up then re-executed 92 (new_payload Valid) and sent an FCU to 92. That is an ancestor of reth's persisted head, and reth refused it: "too deep reorg". Catch-up stopped.
- Blocks 92–95 never passed the Profile-1 header gate. From then on every child was refused with "parent … has not been admitted"; v3 committed only by Decide, and its EL stayed at 95. Consensus safety was unaffected.

## 2. Design decision

- **Rejected: keep reth's head at start-up whenever it extends the snapshot head.** The blocks above the snapshot may include uncommitted proposals that the rewind exists to recover from.
- **Chosen: rewind as before, but tell catch-up which blocks reth already holds as persisted canonical** (hash → number, from `provider.block_hash(n)` for n in recovered+1..=reth head, capped at 4,096 blocks).
  - Catch-up runs new_payload on such a block, which proves the parent chain, but skips the FCU. It then does the same bookkeeping as after a Valid FCU: execution-validated head, head number, lifecycle finalization, deferred finalization, and `BlockImportedWithParent`.
  - The first block that differs from reth's chain still takes the FCU, so a real side branch is handled as a sibling reorg, exactly as before.

## 3. Implementation

- `ConsensusService::persisted_canonical_suffix` and the `with_persisted_canonical_suffix` builder.
- In `import_h2_v4_catchup_block`, the block's own hash and number must both match before the FCU is skipped; each entry is used once.
- The success bookkeeping moved into `finish_h2_v4_catchup_block`, shared by the FCU path and the skip path.
- In `main.rs`, the suffix is collected inside the existing recovery loop and handed over with a start-up info line.

## 4. Tests and status

- `catchup_skips_fcu_for_reth_persisted_canonical_block`: no FCU; the head advances; the entry is consumed.
- `catchup_keeps_fcu_for_block_off_reth_persisted_chain`: same height, different hash; the FCU is kept.
- Clippy is clean and all 2,201 workspace tests pass.

## 5. Next

- Gate it with a whole-set or single-node kill in which the snapshot deliberately lags reth, for example by injecting a snapshot-save failure.
