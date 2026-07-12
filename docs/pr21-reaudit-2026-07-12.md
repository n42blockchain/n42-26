# Deep Re-Audit: PR #21 "gate committed head on execution validity" (n42-26)

- **Repo**: `D:\n42\n42-26` (Rust, reth-based EL + HotStuff-2 CL)
- **Object**: PR #21, commit `09bfdb7`, merged at `b20d3b5`; re-audited against **current `origin/main` (`f246b35`)**, which additionally contains PR #22 (`68cbf97`, sidecar staging hangs off `advance_execution_validated_head`).
- **Scope**: read-only deep review, second pass after the initial code-review-level acceptance. Cross-checked against the Go-side sibling fix (`C:\N42\N42-gov5` `ef5781d5`, `BlockChain.HasAppliedBlock` / `sync.Service.BlockApplied`).
- **Date**: 2026-07-12

All line references below are to `origin/main` unless marked `09bfdb7`.
Key files:
- `crates/n42-consensus-service/src/orchestrator/consensus_loop.rs` (CL)
- `crates/n42-consensus-service/src/orchestrator/execution_bridge.rs` (EB)
- `crates/n42-consensus-service/src/orchestrator/state_mgmt.rs` (SM)
- `crates/n42-consensus-service/src/orchestrator/mod.rs` (MOD)
- `bin/n42-node/src/main.rs` (NODE)

## Verdict

The core mechanism is sound in steady state: every `advance_execution_validated_head` call site corresponds to a genuine EL confirmation, the view-monotonic guard is correct within one process lifetime, and PR #22's sidecar-diff staging is correctly coupled to the single advance funnel. The substantive residual risk is concentrated in **restart**: the guard state (`execution_validated_head_view`, `committed_blocks` ring) is process-local while the consensus view stream and reth's canonical head are persistent, and three distinct degradations flow from that asymmetry (F1, F2). One spec-level gate weakness (F3) and several low-severity robustness items round out the list.

## Findings

| # | Severity | Title | Evidence | Recommendation |
|---|----------|-------|----------|----------------|
| F1 | **Medium** | Restart resets `execution_validated_head_view` to 0, disabling both guards for the first confirmation; a sync-imported old block can then regress `head_block_hash`, and the FCU side effect fires **before** the guard in all cases | MOD:1090 (`execution_validated_head_view: 0` in `with_execution_layer`); CL:1037-1062 (stale + same-view guards, both bypassed at 0); EB:726-740 (`handle_valid_import`: FCU issued at 731-738, `advance` only afterwards at 740); SM:370-432 (`handle_sync_response`: no validation that response blocks fall in the requested view range; ring-based skip at 417-420 useless post-restart because the ring is empty); SM:84-95 (`build_snapshot` persists `last_committed_qc` but NOT the validated head) | Persist `(execution_validated_head_view, head_block_hash)` in the consensus snapshot (v4) and restore it; validate `handle_sync_response` block views against the in-flight request range; move the `view <= execution_validated_head_view` skip in front of `import_and_notify`'s FCU (see Task T2) |
| F2 | **Medium** | Post-restart execution catch-up computes `sync_from = execution_validated_head_view = 0` → requests views `1..=1+cap`. On a long chain peers' rings (default 10 000) cannot serve it → repeated useless catch-up, committed blocks never execute, head wedged (liveness). On a young chain (< ring size) peers CAN serve it → ancient blocks re-imported with `already_committed == false` (ring empty after restart) → **`committed_block_count` inflated by one per replayed block** and reth FCU'd backwards block-by-block | CL:1139-1148 (`sync_from = self.execution_validated_head_view` when committed); SM:211-240 (`initiate_execution_catchup_sync`, `from_view: local_view + 1`); SM:464-470 (`committed_block_count += 1` + `prev_randao_cache` overwrite for every not-in-ring sync block); NODE:806-825 (restart head = reth canonical head — the correct anchor exists but is not used for the view floor) | When `execution_validated_head_view == 0`, derive the catch-up floor from the restored `last_committed_qc.view` (snapshot) or the consensus view of the reth canonical head, never from 0; never increment `committed_block_count` for a sync block whose view ≤ the restored committed view (see Task T3) |
| F3 | **Low/Medium** | `PayloadStatusEnum::Accepted` is treated as execution validity everywhere the gate feeds from. Per the Engine API spec, `ACCEPTED` means the payload was stored for a side chain **without being executed**. If reth ever returns it for a committed block (plausible mid-recovery when the block does not extend the current canonical head), the head advances and PR #22 flushes the staged sidecar diff for an unexecuted block — exactly the three-state divergence class the gate was built to prevent | EB:522, 685, 852; CL:590, 622, 679, 857 (all `Valid \| Accepted`); CL:1064-1069 (advance → sidecar flush) | Gate `advance_execution_validated_head` (and `record_eager_execution_validated`) on `Valid` only, or additionally require `latest_valid_hash == block_hash`. Keep `Accepted` as "retry FCU later", not "validated" (see Task T4) |
| F4 | **Low** | `eager_execution_validated` cap-32 `pop_front` can evict the entry of a validated block whose commit is imminent under speculative view churn (32 newer speculative validations). Consequence is bounded: the finalize-FCU path (CL:711) or bg-import path (CL:1095) re-confirms, costing one extra EL round trip — no liveness loss found on any path walked (Case A/B/C all recover). Entries for never-committed blocks linger until displaced (bounded, no expiry) | CL:1017-1026 (`record_eager_execution_validated`, `MAX_EAGER_VALIDATED_BLOCKS = 32`, FIFO eviction); removal sites CL:454-461, 651-657, 1200-1206 | Acceptable as-is. Optionally store `(hash, view)` and evict lowest-view first; add the eviction-rescue regression test (T5). Not a follow-up blocker |
| F5 | **Low** | `handle_import_done` classifies "committed" by scanning the `committed_blocks` ring (`view && hash`). The ring is `N42_SYNC_BUFFER_SIZE` (default 10 000) but operator-configurable; with a small value, a slow import of an evicted committed block is misclassified as speculative → wrong metric, `sync_from = view` instead of validated head, plain `initiate_sync` instead of catch-up fan-out. No head corruption (head untouched on the failure path either way) | CL:1102-1105 (ring scan); SM:19-24 (env-configurable size, no floor) | Clamp `N42_SYNC_BUFFER_SIZE` to a sane floor (e.g. ≥ 256), or fall back to `view <= last_commit_view` as a secondary committed signal |
| F6 | **Info** | Same-view conflict refusal (`view == head_view && hash != head`) requires a CommitQC safety violation (>f Byzantine) to trigger; the sync path independently refuses conflicting QC-verified blocks at a committed view (SM:398-408). It cannot wedge the head: the next view's confirmation (`view+1 > head_view`) advances normally. The `!= 0` exemption only matters for the first advance after restart, where it is needed (genesis/restart seeding) | CL:1049-1062; SM:398-408 | No change needed |
| F7 | **Info** | View monotonicity holds across epochs and TC jumps within a process: `RoundState::advance_view` refuses `new_view <= current_view`, epochs are keyed on view ranges and never reset the counter, and sync blocks bind `view` to the QC (`commit_qc.view != sync_block.view` rejected). The only "legal regression" of the guard's reference clock is process restart — folded into F1 | `crates/n42-consensus/src/protocol/round.rs:137-144`; SM:550-558 | No change needed |
| F8 | **Info** | PR #21 changed the (informational) ZK scheduler input semantics: `parent_hash: self.head_block_hash` at commit time is now the true parent on the slow path but the block itself on the eager path (head already advanced at CL:454-461 before CL:519). Pre-PR#21 it was always the block itself. Field is not consumed for verification today | CL:516-524 | Capture `parent_hash` before the eager-match advance if the field ever becomes load-bearing |

### Question-by-question mapping

1. **Eager set completeness** → F4. Eviction of a true confirmation is recoverable on every path (finalize FCU retry, Case B bg import, stale-pending-finalization local redrive at CL:922-985). No liveness hole found; test gap remains.
2. **View semantics of the advance guard** → F7 (in-process: safe) + F1 (restart: the real gap — the guard's clock resets while the chain's clock doesn't).
3. **Same-view conflict** → F6. Correctly conservative; cannot wedge.
4. **`handle_import_done` committed classification** → F5. Ring eviction degrades diagnostics/sync-origin only, not the head invariant.
5. **PR #22 interaction** → verified correct. All six advance call sites ("eager import before commit" CL:460, "eager import during finalize" CL:658, "finalize fcu" CL:711, "background import" CL:1095, "eager import after commit" CL:1199, "sync import"/"sync import retry" EB:740/862) fire only after an EL `new_payload`/FCU acceptance. The commit-time ordering race (advance runs before the diff is staged) is explicitly closed by the immediate flush at CL:491-493. Range-flush `..=view` (CL:1299-1318) is sound because commit views form a single chain, so a later confirmation implies ancestor execution. The `Invalid` path drops the staged diff (CL:1116-1122); the `Syncing` path preserves it for the catch-up confirmation. The one caveat is F3: `Accepted` feeding the funnel would flush a diff for an unexecuted block.
6. **Test coverage gaps** → see list below.
7. **Go comparison / restart** → the restart head **value** is safe: `head_block_hash` is seeded from reth's canonical head (NODE:806-825), and reth only canonizes executed blocks — reth's own persistence plays the role of Go's applied-lineage query, so restart is *not* a silent unvalidated head adoption. What is lost across restart is the **guard state** (`execution_validated_head_view` → 0, `committed_blocks` ring → empty), producing F1 and F2. Go's `HasAppliedBlock` (blockchain.go:2420, canonical-prefix + applied-marker lineage walk) has no such asymmetry because the query itself is persistent.

## Missing test cases

1. **Eager eviction rescue**: record 33 speculative eager validations, commit the evicted (first) one, assert the head still advances via the finalize-FCU path and `n42_eager_import_rescued_total`/no-op cost is bounded.
2. **Restart regression guard** (captures F1, currently fails): construct a service with `head_block_hash = H_new` (restored from "reth"), `execution_validated_head_view = 0`; feed a QC-verified sync import at an older view; assert `head_block_hash` does not regress. (Requires the F1 fix to pass.)
3. **Post-restart catch-up floor** (captures F2): committed block at high view returns `Syncing` with `execution_validated_head_view == 0`; assert the catch-up request's `from_view` is derived from the restored committed view, not 1. Note the existing test `committed_syncing_payload_requests_missing_execution_ancestors` (MOD:2513) asserts `from_view == 1` in a view-1 context and therefore does not exercise this edge.
4. **Sync replay of ancient canonical blocks** (captures F2/F1 side effects): empty ring + sync response containing old already-canonical blocks; assert `committed_block_count` unchanged and no backward FCU issued.
5. **`Accepted` is not validity** (captures F3): mock EL returning `Accepted` from `new_payload`/FCU; assert head does not advance and no sidecar diff is flushed (design decision required first).
6. **Ring-eviction misclassification** (F5): small `max_committed_blocks`, evict a committed block, fail its late import; assert catch-up (not plain sync) semantics or the chosen fallback.
7. **Same-view conflict after restart seeding** (F6 edge): first advance at view V seeds the head; second conflicting advance at V is refused; advance at V+1 recovers.

## Follow-up fix tasks (self-contained)

### T1 — Persist the execution-validated head across restarts
Bump `ConsensusSnapshot` to version 4 with `execution_validated_head_view: u64` and `execution_validated_head_hash: B256` (SM:72-96, `persistence` module). On restore in NODE startup: if the snapshot hash equals the reth canonical head, seed both fields; if reth's head is behind the snapshot (crash between commit FCU and snapshot), seed the view of the reth head by walking `committed_blocks`-equivalent metadata or fall back to `snapshot.view - 1` so the pending block's confirmation at `snapshot.view` still passes the `>=` check (the same-view guard refuses hash changes only when `head_view != 0` **and** hashes differ — verify the seeded pair keeps the legitimate confirmation admissible; add a unit test for the crash-between-commit-and-FCU replay).
**Tests**: missing tests 2 and 7 above; plus snapshot v3→v4 upgrade round-trip.

### T2 — Validate sync responses against the request and order the skip before the FCU
In `handle_sync_response` (SM:370): (a) track the in-flight request range and drop blocks outside `[from_view, to_view]`; (b) hoist the `view <= execution_validated_head_view` skip so it also guards `import_and_notify` (EB:647) — today the backward FCU at EB:731-738 fires before the advance guard can refuse anything; (c) never increment `committed_block_count` (SM:464) for a block whose view ≤ the max of (restored committed view, execution-validated head view).
**Tests**: missing test 4; a fan-out duplicate-response test asserting idempotency of `committed_block_count` and no second FCU.

### T3 — Catch-up floor must not be 0 after restart
In `handle_import_done` (CL:1139-1148) and `initiate_execution_catchup_sync` (SM:211): when `execution_validated_head_view == 0`, use `self.last_commit_qc.map(|qc| qc.view)` (restored via `with_recovered_commit_qc`, NODE:1508-1512) or the restored `committed_block_count`-derived view as the floor.
**Tests**: missing test 3.

### T4 — Treat `Accepted` as "retry", not "validated"
Split the `Valid | Accepted` matches feeding `record_eager_execution_validated` / `advance_execution_validated_head` (EB:522, 852; CL:590, 622, 679, 857): only `Valid` (optionally `Valid` + `latest_valid_hash == block_hash`) may advance; `Accepted` should behave like `Syncing` (queue/retry). First confirm empirically whether the pinned reth ever returns `Accepted` on these paths; if provably unreachable, a `debug_assert!` + metric is an acceptable minimum.
**Tests**: missing test 5.

### T5 — Regression tests only (no code change)
Missing tests 1 and 6.

## Priority

T2 > T1 > T3 (all restart/sync-edge; T2 also closes the only mid-run side-effect-before-guard window) > T4 (spec hygiene, likely rare in practice) > T5 (coverage). None of these invalidates PR #21's merge: steady-state behavior matches the design and the Go-side precedent; the follow-ups harden the restart boundary where the in-memory guard diverges from Go's persistent applied-lineage query.
