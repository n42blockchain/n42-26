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
