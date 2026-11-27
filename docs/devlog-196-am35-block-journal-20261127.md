# devlog-196: AM35 block journal — a block is durable before its commit vote (2026-11-27)

Branch `fix/profile1-am35-block-journal`, based on `origin/main` b64f650. Basis: the whole-set power-loss
analysis `docs-drafts/whole-set-power-loss-20261127.md` and AM35 in the mixed production standard (n42-rs
`docs/MIXED_PRODUCTION_STANDARD.md`, "a commit vote means a durable block", option A = consensus-dir block
journal).

## 1. Problem

On Profile 1 the R2 (commit) vote is released by `BlockImportedWithParent`, i.e. by `new_payload` Valid
without a forkchoice update. Before the R2 signature only `vote_log.bin` and `locked_qc.bin` are fsynced.
reth persists a block only after the finalize FCU, asynchronously. A whole-set SIGKILL or power cut between
the (f+1)-th R2 vote for B and the first member's reth `save_blocks` of B leaves every R2 voter with a durable
lock on B while no execution layer holds B. After restart the leader builds on `locked_qc.block_hash` (B),
reth answers Syncing, nobody can serve B, and the set halts for good. The leader's own R2 self-vote
(`voting.rs`) had no gate at all.

## 2. Design decisions

- **Option A (CL journal), not option B (await reth persistence).** Option B needs a pre-commit FCU to B
  (uncommitted blocks become canonical, RPC `latest` and unwinds change) and costs one or two `save_blocks`
  per view (15-20 ms empty, 0.1-0.5 s full). The journal costs one append + `fdatasync` (measured below at
  ~0.45 ms for a 1200-tx block) and changes nothing in reth.
- **Gate inside the vote log, called by the engine.** `VoteLogWriter` gained
  `ensure_block_durable(view, block_hash) -> BlockDurability`. Both R2 paths call it immediately before
  `record_lock` (follower `process_prepare_qc`, leader `cast_leader_commit_self_vote`), so every R2 vote on
  every line goes through it; the engine stays pure and unit tests inject the sink. Order on the R2 path is
  now: journal append + `fdatasync` → `locked_qc.bin` (tmp + fdatasync + rename + dir fsync) →
  `vote_log.bin[8..16]` fdatasync → sign → emit.
- **Hold, don't fail, when data is missing.** `Unavailable { missing }` holds the PrepareQC (follower) or the
  self-vote (leader) for the view; the orchestrator retries with the new `ConsensusEvent::BlockDataStaged`
  whenever block data is staged while an R2 vote is held. A held vote waiting for Profile 1's own execution
  is not retried by data (only by the import), so no extra QC verification per staged block.
- **Ancestors by hash.** `ensure_durable(B)` walks parents from B until it reaches an entry already durable
  in the journal (whose own ancestry was checked when it was appended) or a block the EL has persisted, where
  "persisted" means `number <= persisted_number` and the persisted canonical hash at that height equals the
  block's hash. Missing ancestors are appended in the same write (oldest first) under one `fdatasync`. A
  block at or below the persisted height that is not the persisted canonical block there is refused.
- **Only blocks reth has not persisted are written** (`n42_block_journal_skipped_persisted_total`).
- **Persisted = synced EL commit.** The node refuses to start with `--db.sync-mode` other than `Durable`
  while the journal is on; Profile 1 refuses `N42_BLOCK_JOURNAL=0` (AM35 rule 5). Other lines may turn the
  journal off with a warning.
- **Separate file, not folded into the lock.** The journal is fdatasynced immediately before the lock
  write rather than in the same fsync. Folding the lock into the journal would save one fsync (~0.35 ms on
  the P5520) but changes the durable-lock format other tools read; left as a follow-up.

Alternatives considered: one file per block (needs a directory fsync per append), and journalling at block
arrival instead of at R2 (writes blocks that never get a vote; the R2 point is what AM35 requires).

## 3. Implementation

| Piece | File |
|---|---|
| `BlockDurability`, `VoteLogWriter::ensure_block_durable` (default Durable) | `crates/n42-consensus/src/vote_log.rs` |
| Follower R2 gate, `on_block_data_staged` | `crates/n42-consensus/src/protocol/proposal.rs` |
| Leader self-vote extracted to `cast_leader_commit_self_vote`, gated | `crates/n42-consensus/src/protocol/voting.rs` |
| `ConsensusEvent::BlockDataStaged`, `held_self_commit_vote`, `held_commit_vote_block()`, `set_vote_log`, `ViewTiming::r2_persist_us` (printed as `r2_persist=` in the view timing summary) | `crates/n42-consensus/src/protocol/state_machine.rs` |
| Journal: framing, torn-tail scan, staging, ancestor walk, append + fdatasync, pruning, compaction, `PersistedChain`, `FnPersistedChain`, lineage registry | `crates/n42-consensus-service/src/block_journal.rs` |
| `FileVoteLog::with_block_journal`, `ensure_block_durable` impl | `crates/n42-consensus-service/src/persistence.rs` |
| Restart replay | `crates/n42-consensus-service/src/orchestrator/journal_replay.rs` |
| Staging hooks, `with_block_journal`, `retry_held_commit_vote`, `block_data_lineage`, `block_data_from_execution` | `crates/n42-consensus-service/src/orchestrator/mod.rs` |
| Lineage notes (eager import, leader build), sync-import staging, compaction after execution-valid head | `orchestrator/consensus_loop.rs`, `orchestrator/execution_bridge.rs` |
| Wiring, `open_block_journal`, reth persisted head adapter, sync-mode check | `bin/n42-node/src/main.rs` |

**File format** (`<datadir>/blocks.journal`): frames of
`"N42J" | version 1 | 3 reserved | len u32 LE | blake3(payload) | payload`, payload = bincode
`JournalRecord { block_hash, parent_hash, number, view, native_header, block_data }`, where `block_data` is
the bincode `BlockDataBroadcast` the block-data path carries (payload zstd or binary-v1) and `native_header`
is the Gov5/Profile-1 header RLP known at append time (needed to re-admit the block). `open` truncates at the
first short, oversized, unknown-version or checksum-failing frame; the append that frame belonged to never
returned, so no vote rests on it.

**Staging** (memory, FIFO of 256, `Arc` shared with the existing caches, no copy):
`remember_recent_block_data` (follower block data, Gov5 copies converted to block data, leader payload
feedback), sync import (`import_and_notify`), authenticated Gov5 catch-up (`stage_execution`: the payload is
encoded only if an R2 vote needs it). Rejected (forged) block data is unstaged with the existing rejection
set. Parent/number come from a bounded lineage registry fed by the eager import, the leader build, sync and
catch-up; otherwise the payload is decoded once (memoized).

**Pruning**: entries at or below reth's persisted height leave the index on every append (in memory) and in
`maybe_compact`, which runs after each execution-valid head advance (off the vote path): the file is truncated
to zero when nothing is live, rewritten (tmp + fsync + rename + dir fsync) when dead bytes exceed
`max(live, 64 MiB)`. A live size above 4 GiB warns (reth persistence far behind). There is no hard cap that
drops unpersisted entries: dropping one would reopen the hole.

**reth persisted head** (`main.rs`): `provider.canonical_in_memory_state().get_persisted_num_hash()`
(set after the persistence task's commit returns), at least the startup best block; hashes from
`provider.block_hash(n)`.

**Restart replay** (`ConsensusService::run`, before `initialize_startup_schedule`, i.e. before the first
build, vote or served block): records in height order; skip `number <= EL head`; require the parent to be the
EL head or a replayed block; Profile 1 `admit_known` with the journalled header (other lines remember the
native header); decode, check the hash, `new_payload` (HISTORICAL_SEQUENTIAL); on Valid cache the block data,
record the eager-import completion and run `finish_eager_import_completion` (H2 `BlockImportedWithParent`,
committed-FCU re-drive). Forkchoice moves only to `last_committed_qc.block_hash` if it was replayed, then
`advance_execution_validated_head` + `mark_catchup_commits_finalized`. Uncommitted replayed blocks are
republished (Profile 1/H2: Gov5 RLP via `broadcast_gov5_block_reliable`, which also puts them in the by-hash
serve cache; native: block data announce) so peers that did not vote can fetch them.

**Metrics**: `n42_block_journal_append_ms`, `n42_block_journal_fsync_ms` (histograms),
`n42_block_journal_appended_{blocks,bytes}_total`, `n42_block_journal_unavailable_total`,
`n42_block_journal_skipped_persisted_total`, `n42_block_journal_replayed_blocks_total`,
`n42_block_journal_live_{bytes,blocks}`; per view `r2_persist=` in the existing view-timing summary.
Qualification abort point `N42_QUALIFICATION_ABORT_AT=block_journal_synced` (after the journal sync, before
the lock).

## 4. Lines covered

The R2 gate is in the shared engine and `FileVoteLog`, and `main.rs` is the only voting entry point, so the
Profile 1 / H2 mainline, the native 0x50 line and the ETH-default profile all journal by default. Only
Profile 1 forbids turning it off. Observers do not vote and open no journal. On the native line R2 is not
held for execution, so a PrepareQC can overtake the block data: the vote is now held until the data is staged
(previously it was sent without the block). Native blocks are large (95k txs ≈ 15 MB) and reth persistence
lags under native load; there the journal can grow to GBs and each append costs a few ms
(~2 ms per 1.3 MB measured); `N42_BLOCK_JOURNAL=0` exists for measurement runs on that line.

## 5. Cost (microbench, `/data` = Intel SSDPF2KX076TZ NVMe, xfs; release build; live rounds were running on
the machine)

`N42_JOURNAL_BENCH_DIR=/data/... cargo test --release -p n42-consensus-service block_journal_append_bench -- --ignored --nocapture`,
200 appends each, block = incompressible 110-byte txs, zstd as on the wire:

| Block | Frame | append+fdatasync p50 / p90 / p99 | fdatasync p50 / p99 |
|---|---|---|---|
| 1200 tx | 133.7 KB | 0.45 / 0.49-0.75 / 0.92-1.9 ms | 0.35-0.37 / 0.8-1.8 ms |
| 12000 tx | 1.34 MB | 2.16 / 2.88 / 3.51 ms | 0.37 / 1.24 ms |

For comparison the existing lock write (`record_lock`: tmp + fdatasync + rename + dir fsync) measured p50
0.35 ms, p99 1.1-2.7 ms in the same run. So AM35 roughly doubles the durable work on the R2 path for a
1200-tx block, adding ~0.45 ms per view.

## 6. Tests

- Journal unit tests (`block_journal::tests`): append/read back after reopen, unstaged block or ancestor →
  Unavailable and nothing written, persisted block skipped + fork ancestor refused by hash, torn frame payload
  and torn header truncated (and appends continue), checksum mismatch stops the scan, pruning by persisted
  head down to an empty file, compaction rewrite, forged staged copy refused and replaced.
- Engine ordering (`state_machine::tests`): `OrderingVoteLog` owns the engine's output receiver and drains
  it at every durable call, so an R2 vote leaving before the journal sync would show up first.
  `test_follower_r2_never_leaves_before_block_journal_fsync` (held while unavailable; then
  `block:durable → lock → commit → out:commit_vote`), `test_leader_r2_self_vote_is_gated_on_block_durability`
  (PrepareQC goes out, self R2 waits; no lock/R2 record until durable). Existing lock-order tests now include
  the block step.
- Profile 1 (`tests/profile1_v6.rs` `am35_commit_vote_waits_for_the_block_journal_after_the_execution`): with
  gov5's vectors, executed but not durable → no commit vote; staged → `block → lock → commit`.
- Orchestrator (`orchestrator/tests/am35_block_journal.rs`):
  `whole_set_kill_between_prepare_qc_and_commit_qc_replays_the_locked_block` — live phase with a real
  `FileVoteLog` + journal: PrepareQC before the data holds R2 (no lock on disk), data arrival journals B, then
  lock and R2; "kill" with no CommitQC and no snapshot; restart with an EL that never saw B: replay
  `new_payload`s B, no FCU to B, B's data cached, and the leader's build uses parent B.
  `replay_moves_forkchoice_only_to_the_committed_block` — two journalled blocks, the first committed: both
  re-imported, FCU only to the committed one.
- `cargo clippy -p n42-consensus -p n42-consensus-service -p n42-node-bin --all-targets -- -D warnings`
  clean; `cargo test -p n42-consensus -p n42-consensus-service -p n42-node-bin` all pass.

## 7. Status

- [x] Journal with crash-safe framing, torn-tail tolerance, pruning and compaction
- [x] R2 gate on every path (follower, held release, leader self-vote), ancestors by hash
- [x] Restart replay before build/vote/serve; FCU only to committed; republish uncommitted blocks
- [x] Profile 1 cannot disable it; non-durable reth sync modes refused
- [x] Metrics and per-view timing; microbench
- [ ] Whole-set kill e2e gate (below)

## 8. Next steps

1. AM35 gate, local 4-node Profile 1 (then mixed) set under load, each run: kill -9 every node, restart,
   require heads equal and progress within 2 min, `n42_block_journal_replayed_blocks_total` and the
   "AM35: block journal replayed" log as evidence.
   - 5× timed right after a commit: trigger on the leader's `BlockCommitted` / Decide log line.
   - 5× between PrepareQC and CommitQC: trigger the kill on the first follower log
     `R2 block made durable` (debug) or the `n42::cl::block_journal` append, before the view's Decide; or
     slow the leader's Decide with a debug delay. The windows are 0.02-0.5 s, so timing must be triggered,
     not random.
   - Negative control: same kill with `N42_BLOCK_JOURNAL=0` on a non-Profile-1 genesis should reproduce the
     halt (reth answering Syncing for the locked hash).
2. Optional: fold the lock into the journal frame to share one fsync.
3. The by-hash serve cache after replay holds only republished blocks; a direct journal lookup in the
   network's by-hash handler would cover peers that ask later.

## 9. Review round 1 (2026-11-27)

An external review of the wip commit `b3265ad` raised three blockers and five minor points. All are fixed
on `fix/profile1-am35-block-journal`.

### 9.1 Blocker 1: the snapshot could store a non-durable lock

`process_prepare_qc` (and the leader's `try_form_prepare_qc`) move the in-memory lock to B as soon as the
PrepareQC is verified. That happens before B is durable. While R2 is held, any `save_consensus_state` call
could write that lock into `consensus_state.json`, and on restart `higher_lock` would adopt it.

**Decision.** The in-memory lock keeps moving early. The engine now tracks a separate
`durable_locked_qc`, and the snapshot stores only that one.
- `durable_locked_qc` advances only in `persist_before_commit_vote`, after `ensure_block_durable` and
  `record_lock` have both succeeded.
- A recovered engine starts with `durable_locked_qc` equal to the recovered lock. That lock was durable
  before the restart: it came from either the snapshot or `locked_qc.bin`.

**Why not defer the in-memory lock.** We rejected deferring `update_locked_qc` until after the journal
write.
- The in-memory lock is the safety rule for R1 voting (`safe_node`). A lower lock only makes a node more
  permissive, so deferring it would widen what a node may vote for in the meantime.
- The lock also feeds the leader's parent choice and the TC high-QC.
- Losing a lock this node never commit-voted is safe across a restart. The locks that make a CommitQC
  safe are the ones held by its R2 voters, and every R2 voter writes its lock before signing.

**Shared R2 prologue.** The follower R2, the held-release R2 and the leader self-vote now all call one
function, `ConsensusEngine::persist_before_commit_vote`. It runs these steps in order:
1. Make the voted block durable (`ensure_block_durable`). If the current lock names a different block,
   make that block durable too.
2. `record_lock`.
3. Advance `durable_locked_qc`.
4. `record_commit_vote`.
5. Record `r2_persist_us`.

### 9.2 Blocker 2: pruning and replay compared heights, not hashes

- `BlockJournal::prune` drops an entry in two cases:
  - The persisted canonical block at the entry's height has the entry's hash.
  - The entry is a **dead fork**: its height is at or below both the persisted tip and the finalized
    height (`note_finalized`, fed from `advance_execution_validated_head`), and the persisted block at
    that height has a different hash.

  A fork entry above the finalized height survives an early persisted sibling.
- `ensure_durable` no longer returns `Err` for a staged ancestor that sits at a persisted height under a
  different hash. Such an ancestor is journalled like any other unpersisted block, and the walk continues
  down to the common ancestor. Previously this case dropped the vote.
- Replay skips a record only when `journal.is_persisted(hash, number)` is true. A record whose parent is a
  persisted block (matched by hash) is replayed even below the EL head, so a fork sibling comes back.
- The lineage registry was a process-global map, and parallel tests polluted each other through it
  (`unstage_drops_forged_copy` was flaky). It is now owned by each journal (`note_lineage`).

### 9.3 Blocker 3: replay failures were ignored

`replay_block_journal` now returns `Result<usize, ReplayFailure>`.
- **Required blocks.** These are the locked block, the committed block, and their journalled ancestors.
  Each one must end up either persisted (by hash) or replayed.
- **Retries.** `new_payload` is retried up to 5 times with backoff starting at 250 ms and doubling each
  time, on an EL error or a non-final status (SYNCING/ACCEPTED). INVALID is final and is not retried.
- **Fail closed.** If a required block is still missing after the retries, `run()` panics with
  `AM35: journalled <role> block <hash> (height n) could not be replayed into the execution layer: <reason>`.
  The orchestrator is a critical task, so the node stops before it serves, votes or builds.
- **Other failures.** A failed block that nothing rests on is logged and skipped.

### 9.4 Minor points

1. **Held R2 retry.** A held R2 is retried on a 200 ms tick in the event loop
   (`HELD_COMMIT_VOTE_RETRY`, armed only while a vote is held). It is also retried after replay, in
   addition to the existing retry on staged data.
2. **Off-loop journal writes.** The R2 path still calls `ensure_durable` inline, and that call remains the
   gate. In addition, `record_eager_import_completion` now journals the just-validated block on
   `spawn_blocking`. On Profile 1, R2 waits for the node's own execution, so the background append has
   normally finished, and the inline call is a lookup under the journal mutex. If the background write is
   still in flight, the inline call waits on the mutex for the rest of it. The mutex orders the two
   writers, and the vote is still signed only after `ensure_durable` returns Durable.

   Measured inline costs (section 5):
   - 1200-tx block: append + fdatasync about 0.45 ms p50.
   - 12000-tx block: about 2.2 ms p50.

   A 30 MiB block costs roughly two copies plus blake3 plus fdatasync, an estimated 20-40 ms. That is the
   case the background append keeps off the loop.
3. **Sync-mode check.** The durable-sync-mode check compares the enum directly:
   `mode == reth_db::mdbx::SyncMode::Durable`. `n42-node-bin` now depends on `reth-db` with the `mdbx`
   feature.
4. **Replay through `run()`.** Two tests drive `run()` itself:
   - `run_replays_the_journal_before_the_first_build`: the mock EL's ordered call log shows
     `new_payload(B)` before `build(parent = B)`.
   - `run_stops_before_the_event_loop_when_the_locked_block_cannot_be_replayed`: `run()` panics with the
     AM35 message, and nothing is built.
5. **Stale comment.** The comment at the eager import site said "new_payload + fcu". It now says the eager
   import runs `new_payload` only.

### 9.5 Stable log line for the E9 kill tool

On every R2 path, right after the journal fsync and `record_lock`, and immediately before the R2
signature, the node logs:

```text
INFO n42::am35: commit vote released after durable lock view=<u64> block_hash=0x<64 hex> role="follower"|"leader"
```

- The line is emitted only from `persist_before_commit_vote`. That covers the follower direct path, the
  follower released after holding, and the leader self-vote.
- `block_hash` uses `Display`, which prints the full hash. `role` uses `Debug`, which prints it quoted.
- The text is fixed because the mixed-gate kill tool (E9) regex-matches it.
- The `N42_COMMIT_STATE: Committed` line in `consensus_loop` is unchanged.

### 9.6 New tests

- `whole_set_kill_between_prepare_qc_and_commit_qc_replays_the_locked_block` now also checks that while
  R2 is held, the in-memory lock is B but `build_snapshot().locked_qc` is still genesis. After the R2, the
  snapshot lock is B.
- `persisted_block_is_not_journalled_and_ancestors_match_by_hash`: a fork parent is journalled together
  with its child. Pruning keeps it while the height is not finalized and drops it after
  `note_finalized`.
- New replay tests:
  - `replay_retries_a_transient_execution_layer_failure`
  - `replay_fails_closed_when_the_locked_block_cannot_be_replayed`
  - `replay_matches_persisted_blocks_by_hash_not_height`
- New `run()` tests: the two listed in 9.4 item 4.

Results:
- n42-consensus: 296 lib tests plus integration tests, all pass.
- n42-consensus-service: 380 lib tests pass (3 consecutive runs) and `profile1_v6` passes 20.
- n42-node-bin: tests pass.
- clippy `-D warnings` is clean on all three crates.

### 9.7 E2E runbook (local 4-node Profile 1; not run here because live rounds own the box)

See the AM35 report for the exact commands. In summary:
- At least 5 whole-set `kill -9` after a commit, triggered on `N42_COMMIT_STATE: Committed`.
- At least 5 whole-set `kill -9` between PrepareQC and CommitQC, triggered by
  `N42_QUALIFICATION_ABORT_AT=block_journal_synced` on all four nodes, or by the `n42::am35` line above.
- After each restart, every node must log `AM35: block journal replayed` with `locked_replayed=true`
  (when the lock was above the EL head). The chain must commit a higher block within 60 s, and the nodes
  must agree on the committed hashes.
