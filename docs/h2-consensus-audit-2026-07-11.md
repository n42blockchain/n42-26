# N42-26 HotStuff-2 Consensus ↔ State Coupling Audit

Cross-repo audit of the Rust HotStuff-2 implementation in `D:\n42\n42-26`
(`crates/n42-consensus`, `crates/n42-consensus-service`, and its EL/state seams
into `n42-node` / reth) against the class of consensus↔state coupling bugs the
Go repo (`C:\N42\N42-gov5`, `feat/eth-el-snapshot-direct` / QMDB work) hit in
live-fire testing.

Scope: read-only. No code was changed. All file:line references are into
`D:\n42\n42-26` unless noted.

## Architecture in one paragraph

The Rust node runs a Caplin-style "decoupled but in-process" consensus driver.
`ConsensusEngine` (`crates/n42-consensus`) is a pure state machine that emits
`EngineOutput`s; `ConsensusService` (`crates/n42-consensus-service/src/orchestrator`)
bridges those to the reth Engine API (`new_payload` / `forkchoiceUpdated`) via the
`ExecutionLayer` port (`el.rs`) and to libp2p via `ConsensusNetwork`.
**Crucially, execution state is owned entirely by reth's engine tree** — the
consensus service never mutates PlainState/trie directly and has no unwind path
of its own. Canonical selection is reth's fork-choice, driven only by
`forkchoiceUpdated`. Two optional sidecar state trees (JMT, Twig) are updated
asynchronously as cross-checks. Voting is **optimistic**: followers cast the R1
vote immediately after validating the *proposal envelope* (leader identity + BLS
sig + `justify_qc`), **before** the block is executed (`proposal.rs:365-380`).

This architecture makes the Go repo's unwind-atomicity bugs (#2/#5/#6/#7) largely
**structurally absent** — there is no hand-rolled three-state rollback to get
wrong. But the same optimistic-commit decoupling reintroduces the **most severe**
Go bug (#1) in a different shape, and creates one genuine new atomicity gap in the
sidecar trees.

---

## Findings

| # | Go bug | Severity | Rust status | Evidence | Fix recommendation |
|---|--------|----------|-------------|----------|--------------------|
| 1 | Vote gated on block *presence* not *execution* → unexecutable block gets QC and pins head | **CRITICAL** | **Same class present — by design (optimistic voting), and stronger: no execution gate at all** | `crates/n42-consensus/src/protocol/proposal.rs:363-380` (emit `ExecuteBlock` then `send_vote` unconditionally, comment "R1 vote … does NOT commit to block validity"); commit advances head unconditionally: `consensus_loop.rs:451-452`; execution failure only *after* commit: `consensus_loop.rs:762-786`, `handle_import_done` failure path `consensus_loop.rs:1012-1021` (only `initiate_sync`, head never rolled back) | Add an execution-outcome floor before the CommitQC is allowed to advance the local `head_block_hash`, OR gate the *self-canonicalizing* FCU on `new_payload==Valid`. See Task 1. |
| 2 | Tree / PlainState / applied-marker must roll back in one txn | **N/A (immune by structure)** for authoritative state; **partial gap** for sidecar trees | reth engine tree is the single authoritative state store; `new_payload` insertion is atomic inside reth and non-canonical blocks are discarded without a hand-rolled rollback. **But** JMT/Twig sidecar diffs are applied at commit time *before* reth confirms the block: `consensus_loop.rs:466-540` runs `apply_diff` in `spawn_blocking` immediately after `handle_block_committed`, whereas reth import/FCU happens later at `consensus_loop.rs:590`. | Defer sidecar `apply_diff` until `handle_finalize_done(finalized=true)`; on import failure the sidecar trees must not have advanced. See Task 2. |
| 3 | canonical table must have a single writer (commit path only) | **Immune / correctly designed** | Canonical = reth fork-choice; the only `forkchoiceUpdated` writers are the **post-commit** finalize path (`consensus_loop.rs:615-728`), the **sync** path on QC-verified blocks (`execution_bridge.rs:726-738`, gated by `verify_sync_block_qc` `state_mgmt.rs:501-559`), and its retry (`execution_bridge.rs:856-861`). Eager/speculative imports **deliberately** run `new_payload` only, **no FCU** (`execution_bridge.rs:511-516`, `1022-1025`; comments cite reorg/stall risk). | No change. This is exactly the lesson Go learned; keep the "eager import = new_payload only, never FCU" invariant guarded by a comment/test so it is not regressed. See Task 5 (guard test). |
| 4 | ValidateState must actually compare execution root to header root | **Verified by default; silently disablable** | Production default verifies: reth `new_payload` recomputes state root and rejects on mismatch — `patches/reth-n42-perf.patch:127-133` (`if !n42_skip_state_root() && state_root != header.state_root()`). Cache-hit path still forces `StateRootStrategy::Synchronous` and verifies (`patch:30-45`). **But** `N42_SKIP_STATE_ROOT=1` fully skips the check (`patch:93-111`) and `N42_DEFER_STATE_ROOT=1` writes `state_root=ZERO` — both currently only `warn!`, not hard-fail. | Land Phase A of `docs/rfc/production-safe-deferred-state-root.md`: hard-exit at startup when skip/defer is set without an explicit bench allow-flag + benchmark chain-id range. See Task 4. |
| 5 | Failed/discarded candidate leaves an un-reverted append in the in-memory tree → all later roots shift | **Immune (authoritative) / gap shared with #2 (sidecar)** | reth engine tree is not an append-only structure the consensus service mutates; a rejected `new_payload` leaves no residue in canonical state. Consensus-internal residue *is* cleaned on view change: `state_machine.rs:965-972` (`advance_to_view` clears vote/commit/timeout collectors, `prepare_qc`, `pending_proposal`, `imported_blocks`, equivocation trackers) and on timeout `timeout.rs:106-107`. The only residue risk is the sidecar JMT/Twig (same as #2). | Covered by Task 2. |
| 6 | committed floor must protect block *identity*, not height | **N/A (no in-consensus revert)** | There is no unwind/reorg/`ApplyUndo` code in `n42-consensus-service` (grep: only FCU-reorg *avoidance* comments). Committed monotonicity is enforced on QC view, not height, and never regresses: `round.rs:124-134` (`commit` guards `>=`), `round.rs:138-144` (`advance_view` refuses regress). Recovery is forward-only via block sync. | No change. If an in-consensus reorg path is ever added, port the Go lesson (key the floor on block hash, not number). |
| 7 |防线 must distinguish out-of-order arrival from a true discontinuity | **Handled** | Future-view messages are buffered within a window and replayed, not treated as a fault: `state_machine.rs:698-742`, `advance_to_view` replay `state_machine.rs:987-1008`. GossipSub out-of-order Decide-before-Proposal is explicitly recovered, not flagged as divergence: `proposal.rs:18-121` (`recover_late_committed_proposal`), `decision.rs:61-64`. Deferred finalization handles Decide-before-BlockData: `consensus_loop.rs:781-784`, `910-918`. Genuine epoch divergence (bitmap-length mismatch) is a distinct signal that triggers sync: `orchestrator/mod.rs:1684-1711`. | No change. Note one residual liveness risk in Task 3 (stale `pending_finalization` force-clear at `consensus_loop.rs:941-960` can drop a committed block's data and rely on sync). |
| 8 | passive (gossip/push) import must not trigger a branch switch; only consensus-driven paths may | **Immune / correctly designed** | Passive block-data arrival (`handle_block_data`, `execution_bridge.rs:357-615`) runs `new_payload` only, never FCU, and guards duplicate block numbers via an atomic (`execution_bridge.rs:487-491`, `1017-1021`). Only the commit/sync (consensus-driven) paths call FCU. | No change (shares the guard-test recommendation in Task 5). |

### Detail on Finding #1 (the important one)

The Go incident: an `import-gated` vote used "block body present" (`HasBlock` /
`insert` returning nil, which the future-queue also does) as the vote
precondition instead of "state executed on the applied lineage." Six validators
voted for a fork leader's unexecutable block, it got a QC, was committed, and the
committed head pinned to a block that can never execute — unrecoverable because
HotStuff safety forbids rolling back a committed block.

The Rust design is **more optimistic, not less**. In `process_proposal`
(`proposal.rs:244-382`) the follower:

1. verifies the proposal envelope (leader identity `256-262`, proposer BLS sig
   `264-275`, `justify_qc` aggregate sig `283-297`, HotStuff safety rule
   `is_safe_to_vote` `299-304`);
2. emits `ExecuteBlock` as a *background* hint (`363`);
3. **immediately** calls `send_vote` (`379`) — with an explicit comment that the
   R1 vote "does NOT commit to block validity" and that "Block validity (EVM
   execution) is verified during finalization … If invalid: FCU fails → view
   change recovers."

That last claim is the load-bearing assumption, and it is only half true.
Execution safety (agreement) does hold on the QC chain. But **execution
*validity* is never a precondition for the CommitQC.** If a leader (Byzantine, or
just buggy — e.g. a builder/EVM edge case) produces a block whose header the
followers accept but whose `new_payload` later fails, the sequence is:

- CommitQC forms / Decide received → `handle_block_committed` runs
  `committed_block_count += 1` (`consensus_loop.rs:353`) and
  `self.head_block_hash = block_hash` (`452`) **unconditionally**;
- `finalize_committed_block` → `background_import` → `new_payload` returns
  rejected → `handle_import_done(success=false)` (`consensus_loop.rs:1012-1021`)
  only calls `initiate_sync`; **it never rolls back `head_block_hash` or
  `committed_block_count`.**

So the next build uses the unexecuted block as parent
(`do_trigger_payload_build` reads `self.head_block_hash`,
`execution_bridge.rs:122,177-181`), and sync cannot help because the committed
block is genuinely unexecutable — the exact Go pin, reached by a different route.

Mitigations that make this *lower probability* than the Go case (but not
*absent*): the leader executes the block during `builder.finish()` and, by
default, reth verifies the state root at follower `new_payload`
(Finding #4). So an honest leader on a correct build cannot trip it. The residual
exposure is (a) a Byzantine leader, (b) an EVM/builder nondeterminism bug, or
(c) any deployment that enabled `N42_SKIP_STATE_ROOT` / `N42_DEFER_STATE_ROOT`,
where a wrong-state block sails through commit.

---

## Learnings — what the Rust side does that the Go side could adopt

1. **Single canonical writer, enforced by an explicit "import = new_payload only,
   never FCU" invariant.** The eager/speculative/passive import paths run
   `new_payload` to warm reth's engine tree but *never* touch fork-choice; only
   the committed path (and QC-verified sync) issues FCU
   (`execution_bridge.rs:511-516`, `1022-1025`; `consensus_loop.rs:1037-1050`).
   This is precisely the property the Go repo had to retrofit after the
   "canonical 三写者" incident. The Rust code makes it a first-class, commented
   invariant. Go's `internal/ethel` / QMDB leader path would benefit from the
   same crisp separation (build/import warms the tree; only 2-chain-commit
   canonicalizes).

2. **Execution owned by one authoritative store (reth engine tree) with no
   hand-rolled three-state rollback.** Because consensus never mutates
   PlainState/trie/marker itself, the whole class of "revert marker but not
   state" bugs cannot be written. Non-canonical blocks are discarded by reth's
   engine tree. Where Go keeps QMDB tree + PlainState + applied-marker as three
   separately-mutated stores that must be rolled back atomically, the Rust design
   sidesteps it by delegating fork-choice to a single component. Worth
   considering for the Go EL path: treat the trie as a discardable cache behind a
   single fork-choice authority rather than three coupled stores.

3. **Typed forward-only round state.** `RoundState` (`round.rs`) makes
   `last_committed_qc` / `locked_qc` / `current_view` monotone by construction
   (`commit` `124-134`, `advance_view` `138-144`, `may_vote_in`/`record_vote`
   `101-113` with vote-log fsync before signing `proposal.rs:469-474`). The
   crash-safety contract "record vote to durable log *before* signing" is a clean
   pattern the Go hotstuff path could mirror for its double-vote guard.

4. **Out-of-order vs. divergence are different signals with different handlers.**
   Future-view buffering + replay, Decide-before-Proposal recovery, and
   Decide-before-BlockData deferred finalization are all treated as *normal
   asynchrony*, while epoch bitmap-length mismatch is the *only* thing that
   escalates to sync. This is the clean version of the Go "乱序到达 vs 真断层"
   distinction (#7).

Caveat on borrowing #2/#3: the Rust design buys its immunity by making the state
root *not* on the consensus-commit critical path, which is exactly what
reintroduces Finding #1. The lesson to port is the *single-authority /
single-canonical-writer* structure, **paired with** an execution-validity floor
before commit (Task 1) — not the optimistic-commit-without-floor as-is.

---

## Task list for codex (each self-contained)

### Task 1 — Add an execution-validity floor so a committed block cannot pin the head (Finding #1, CRITICAL)

**Background.** Followers vote R1 before executing (optimistic voting,
`proposal.rs:363-380`). On commit, `handle_block_committed` unconditionally sets
`head_block_hash` and increments `committed_block_count`
(`consensus_loop.rs:451-452, 353`). If the committed block's `new_payload` later
fails (`handle_import_done` success=false, `consensus_loop.rs:1012-1021`), the
head is never rolled back and the next build parents off an unexecutable block —
mirroring the Go "committed head pinned to unexecutable block" incident.

**Files.**
- `crates/n42-consensus-service/src/orchestrator/consensus_loop.rs`
  (`handle_block_committed`, `finalize_committed_block`, `handle_import_done`).
- `crates/n42-consensus-service/src/orchestrator/execution_bridge.rs`
  (eager import outcomes feed `head`).

**Expected change.** Do not treat a block as the new local head until reth has
returned `Valid`/`Accepted` for it at least once (eager import, finalize FCU, or
bg import). Concretely: keep `committed_block_count` on the QC (agreement is
safe), but make `head_block_hash` advance **only** on a confirmed
execution result. On `handle_import_done(success=false)` for a *committed* block,
escalate beyond `initiate_sync`: emit a loud `error!` + a distinct metric
(`n42_committed_block_unexecutable_total`) and refuse to build on it (leave
`head_block_hash` at the last *executed* block) so the node halts producing
rather than extending an invalid chain.

**Acceptance.** New unit/integration test: commit a block whose `ExecutionLayer`
mock returns `Invalid` from `new_payload`; assert `head_block_hash` stays at the
prior executed block, the new metric increments, and no subsequent
`do_trigger_payload_build` parents off the bad hash. Existing 7-node chaos test
(`crates/n42-consensus/tests/chaos_7node.rs`) still green.

### Task 2 — Defer sidecar (JMT/Twig) state-tree updates until reth confirms the block (Findings #2/#5)

**Background.** `handle_block_committed` applies the leader-broadcast bundle diff
to the JMT and Twig sidecar trees at commit time (`consensus_loop.rs:466-540`,
`spawn_blocking apply_diff`), which runs *before* `finalize_committed_block`
(`consensus_loop.rs:590`) confirms the block imported into reth. If the import
fails, the sidecar trees have already advanced on a block reth never accepted →
sidecar/authoritative divergence (a soft echo of the Go three-state atomicity
bug).

**Files.** `crates/n42-consensus-service/src/orchestrator/consensus_loop.rs`
(`handle_block_committed` sidecar block, `handle_finalize_done`).

**Expected change.** Move the JMT/Twig `apply_diff` calls out of
`handle_block_committed` and into the `finalized == true` branch of
`handle_finalize_done` (`consensus_loop.rs:737-761`), so the sidecar trees only
advance for blocks reth confirmed `Valid`. Preserve the existing "catch up later
if block data missing" debug path.

**Acceptance.** Test: mock EL returns `Invalid`; assert `StateSink::apply_diff`
is never called for that block (spy sink). Happy-path test: `Valid` block still
updates both sidecar roots exactly once. No change to `n42_jmt_latest_root` /
`n42_twig_latest_root` semantics for successful blocks.

### Task 3 — Harden stale-`pending_finalization` handling to avoid silently dropping a committed block (Finding #7 residual)

**Background.** On view change, a `pending_finalization` more than 2 views behind
is force-cleared and `pending_block_data` is wiped, relying on sync to refetch
(`consensus_loop.rs:941-960`). If the block was committed but its data was
evicted, this can drop the only local copy and depend on a peer having it.

**Files.** `crates/n42-consensus-service/src/orchestrator/consensus_loop.rs`
(`handle_view_changed`), `state_mgmt.rs` (`committed_blocks` ring buffer).

**Expected change.** Before clearing, if the stale `pending_finalization`
corresponds to a `committed_blocks` entry whose `payload` is non-empty, re-drive
finalize from that retained payload instead of dropping to sync. Only fall back
to `initiate_sync` when no local payload exists.

**Acceptance.** Test: commit a block, force a >2-view jump with the payload still
in `committed_blocks`; assert finalize completes locally without a sync request.

### Task 4 — Fail-closed on skip/defer state-root flags in production (Finding #4)

**Background.** `N42_SKIP_STATE_ROOT` (`patches/reth-n42-perf.patch:93-133,
306-328`) and `N42_DEFER_STATE_ROOT` disable/void the root check but currently
only `warn!`. `docs/rfc/production-safe-deferred-state-root.md` Phase A specifies
hard guards; they are not yet implemented.

**Files.** `bin/n42-node/src/main.rs` (startup, per RFC lines 34-42),
`crates/n42-node/src/lib.rs:57-61` (`defer_state_root_enabled`).

**Expected change.** At startup, if skip/defer is set, `std::process::exit(1)`
unless `N42_ALLOW_BENCH_MODE=1` **and** the configured `chain_id` is inside the
reserved benchmark range; expose an `n42_deferred_state_root_enabled` gauge.

**Acceptance.** Node exits non-zero when the flag is set without the allow-flag;
starts normally in the benchmark chain-id range with the allow-flag; gauge
reflects the mode.

### Task 5 — Lock in the "import = new_payload only, never FCU" invariant with a regression test (Findings #3/#8)

**Background.** The single-canonical-writer property depends on eager/speculative/
passive import paths never issuing FCU (`execution_bridge.rs:511-516, 1022-1025`).
It is enforced only by comments today; a future refactor could silently add an
FCU and re-open the Go "canonical 三写者" class of bug.

**Files.** `crates/n42-consensus-service/src/orchestrator/execution_bridge.rs`
(tests module), `el.rs` (mock `ExecutionLayer`).

**Expected change.** Add a test with a mock `ExecutionLayer` that records every
`fork_choice_updated{,_with_attrs}` call. Drive `handle_block_data`
(passive import) and the leader eager-import path; assert **zero** attribute-less
FCU calls result from those paths, and that FCU only appears from
`finalize_committed_block` / sync.

**Acceptance.** Test green; it fails if any import path is later wired to call FCU.

---

## Notes / non-issues observed

- Vote equivocation is checked *after* BLS verification and is leader-only, so an
  unauthenticated peer cannot poison the tracker (`voting.rs:38-82, 208-224`) —
  good.
- Decide verifies the CommitQC aggregate signature including
  `validator_changes_hash` before committing (`decision.rs:50-59`), closing the
  Byzantine-changes-swap vector.
- Double-vote safety: `may_vote_in` + `record_vote` + vote-log fsync-before-sign
  (`round.rs:101-113`, `proposal.rs:469-474`, `224-227`) is crash-safe.
- QC-based view jump clamps the jump gap (`state_machine.rs:865-869`) and only
  advances `locked_qc`, so a single Byzantine far-future message cannot corrupt
  safety state.
