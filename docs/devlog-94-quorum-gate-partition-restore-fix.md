# Devlog 94: Quorum Gate Partition-Restore Liveness Fix

Date: 2026-06-26
Branch: `codex/quorum-gate-7node-validation`

## Objective

Root-cause and fix the liveness gap found by the 7-validator quorum-gate validation: after a mid-run partition healed, finality did not resume within the validation window. The leader proposal gate must remain strict: no leader build should bypass `connected_validator_peers >= engine.quorum_size() - 1`.

## Root Cause

The failure was in the leader build timer path, not in validator peer counting.

`LeaderBuildWaitMode::Scheduled` did this sequence after a view change:

1. quorum gate passed;
2. `evaluate_leader_build_wait()` cleared `leader_build_waiting_view`;
3. `schedule_payload_build()` set `next_build_at` for the next slot boundary;
4. when the slot timer fired, the timer branch called `evaluate_leader_build_wait(slot_ts)`;
5. because `leader_build_waiting_view` was already `None`, `evaluate_leader_build_wait()` returned immediately and never triggered payload building.

The old failing artifact shows the symptom:

- `.artifacts/quorum-gate-7node-20260626-030100/mid-run-partition-kill-4-5-6/result.json`
  - `height_before_partition=1`
  - `height_during_partition=1`
  - `height_after_restore=1`
- old logs had repeated `slot boundary reached, triggering payload build`, but timeout diagnostics stayed at:
  - `build_started=false`
  - `block_data_received_count=0`
  - `next_build_scheduled=false`

That means the timer was firing, but it was not re-entering the gated build path. The partition test exposed this, but the bug also affected normal post-view-change scheduled leader builds after block 1.

## Fix

Changed `crates/n42-consensus-service/src/orchestrator/mod.rs`:

- added `arm_leader_build_timer_as_direct_wait(slot_timestamp)`;
- when `next_build_at` fires, the timer now arms a `LeaderBuildWaitMode::Direct` wait if no wait is already pending;
- then it calls `evaluate_leader_build_wait()`, which still performs the quorum check before triggering `do_trigger_payload_build()`;
- added exact quorum logs with `connected_validator_peers` and `needed_quorum_peers`;
- added unit tests proving the timer re-arms a direct wait and preserves an existing scheduled wait.

This keeps the gate authoritative. The fix restores the missing re-entry into the existing gate; it does not add a proposal path around the gate.

## Investigation Answers

1. After heal, the leader's connected validator count recovers.

   New green artifact: `.artifacts/quorum-gate-7node-20260626-033250-partition-fix`.

   In `mid-run-partition-kill-4-5-6/validator-2.log`:

   - before/around partition: `view=2 ... connected_validator_peers=4 needed_quorum_peers=4`
   - after restore: `view=2 ... connected_validator_peers=6 needed_quorum_peers=4`

   So peer bookkeeping recovered; the original failure was not a permanent reconnect-count bug.

2. The view did advance during the partition.

   The old failing logs show timeout/view-change diagnostics from view 2 onward while height stayed at 1. In those views, leaders logged slot-boundary timer events, but no build started because the scheduled timer path returned without a pending wait.

3. Restarted nodes were able to rejoin and vote after the fix.

   In the green run, validators 4, 5, and 6 were killed and relaunched. The first post-heal committed block was block 2 at view 3 with `signer_count=7`, so restarted validators synced/rejoined well enough to contribute to the QC.

4. Progress after restore was driven by the re-armed leader build path.

   After restore, the leader build gate observed quorum again, built a payload, and committed:

   - `validator-3.log`: `payload built, feeding BlockReady to consensus ... view=3`
   - `validator-3.log`: `block committed! view=3 ... votes=5+5`

## Green Validation Run

Command:

```bash
ARTIFACT_ROOT="$PWD/.artifacts/quorum-gate-7node-20260626-033250-partition-fix" \
  bash -lc 'ulimit -n 10240 2>/dev/null || true; scripts/validate-quorum-gate-7node.sh'
```

Results:

```text
baseline-all-7: height=1, time_to_first_block_secs=9
staggered-0-1-2-then-rest: early_height=0, height=2, time_to_first_block_secs=34, time_after_quorum_secs=15
leader-1-alone-then-rest: early_height=0, height=1, time_to_first_block_secs=37, time_after_quorum_secs=20
mid-run-partition-kill-4-5-6: height_before_partition=1, height_during_partition=1, height_after_restore=2
single-node: height=1, time_to_first_block_secs=1
```

ConsensusEvidence checks:

```text
baseline block 1: signer_count=7, view=1
staggered block 1: signer_count=7, view=1
leader-first block 1: signer_count=7, view=1
partition block 1: signer_count=7, view=1
partition restored block 2: signer_count=7, view=3
single-node block 1: signer_count=1, view=1
```

Partition artifact details:

- `mid-run-partition-kill-4-5-6/result.json`: `{"height_before_partition":1,"height_during_partition":1,"height_after_restore":2}`
- `mid-run-partition-kill-4-5-6/evidence-block-restored.json`: block 2, view 3, `signer_count=7`
- `validator-3.log`: post-heal commit at view 3 with `votes=5+5`
- no committed block during the partition window: height stayed `1 -> 1`
- no duplicate persisted QC at the same height was observed; evidence store contains block 1, block 2, and block 3 with distinct hashes and `signer_count=7`

## Verification

Passed:

```bash
cargo fmt --package n42-consensus-service --check
cargo check -p n42-consensus-service
cargo test -p n42-consensus-service leader_build_timer -- --nocapture
cargo test -p n42-consensus-service startup_leader_gate -- --nocapture
cargo test -p n42-consensus-service quorum -- --nocapture
cargo clippy -p n42-consensus-service -- -D warnings
cargo build --release --bin n42-node
bash -n scripts/validate-quorum-gate-7node.sh
```

Note: `cargo build --release --bin n42-node` still emits an existing upstream reth dependency warning for an unused import in `reth-ethereum-payload-builder`; `clippy -p n42-consensus-service -- -D warnings` is clean.

`cargo fmt --check` for the whole workspace is not a useful signal on this branch right now: it fails on pre-existing formatting drift outside this fix scope. Package-scoped fmt for `n42-consensus-service` is clean.
