# Devlog 97: LockedQC-authoritative leader build parent

Date: 2026-07-18  
Task: `docs/codex-task-sync-from-gov5-2026H1.md`, S3

## Outcome

The S3 audit confirmed that proposal construction and payload construction had
different parent authorities. `ConsensusEngine::on_block_ready` correctly put
the current `locked_qc` in `Proposal.justify_qc`, but the orchestrator asked
reth to build on its locally execution-validated `head_block_hash`. During a
view change those hashes can disagree, allowing a block built as a child of B
to be proposed with a QC for A.

Leader payload construction now uses `locked_qc.block_hash` as the only parent
after genesis. If reth does not yet have that block, its existing
`Syncing`/no-payload response defers the proposal and schedules a retry; the
leader never falls back to another local fork.

## Audit matrix

| Surface | Before | Result |
|---|---|---|
| Proposal justification | `on_block_ready` cloned `locked_qc` | Sound; unchanged |
| Payload-build FCU head | Local execution `head_block_hash` | Fixed: non-genesis builds use `locked_qc.block_hash` |
| Divergent FCU safe/finalized | Local sibling could be advertised as A's ancestor | Fixed: when hashes differ, no new safe/finalized assertion is sent |
| Resolved payload parent | Trusted the payload-builder response | Fixed: compare `execution_data.parent_hash()` with the captured LockedQC parent |
| View change during async build | Old block hash was fed to the engine at its new current view | Fixed: every result carries captured `(view, parent_hash)` and stale results are discarded |
| Async completion guard | Old task could clear a newer task's build guard | Fixed: completion can clear only its own captured context |
| Follower voting mode | Optimistic R1 vote before EL import | Intentionally unchanged by the task redline |

The reth Engine API consistency rule is important in the A/B sibling case:
safe and finalized hashes, when non-zero, must be ancestors of the requested
head. Therefore the divergent build request sends `head=A` with zero
safe/finalized fields. This does not declare A finalized; it merely avoids
making a false claim about sibling B. Reth either finds/imports A and builds on
it, or returns `Syncing` so the leader defers.

## Async race closure

The audit found a second bug in the same path. Payload resolution previously
returned only a block hash. If the pacemaker changed view before resolution,
the orchestrator passed that old hash to `BlockReady` under the engine's new
view, and the engine wrapped it in the new `locked_qc`. A
`PayloadBuildContext { view, parent_hash }` now travels through build start,
resolution, parent validation, completion, and `BlockReady`. The event loop
recomputes the required current context before allowing proposal construction.

The following metrics expose fail-closed behavior:

- `n42_locked_qc_parent_unavailable_total`: a non-genesis LockedQC contains a
  zero block hash;
- `n42_payload_parent_mismatch_total`: the payload builder returned a child of
  a parent other than the requested LockedQC block;
- `n42_stale_payload_builds_total`: the resolved `(view, parent)` is no longer
  the current required context.

## Compatibility and rollout

This is a consensus-behavior break, even though no proposal field, signing
domain, or bincode layout changed. An old leader can still construct a block on
the wrong local branch, and the existing optimistic follower path deliberately
does not execute/import-gate R1 voting. Mixed old/new validators therefore do
not provide the new invariant for turns led by an old node.

`CONSENSUS_PROTOCOL_VERSION` is bumped from 3 to 4 so mixed deployments reject
each other instead of silently running different leader rules. All validators
must be stopped and upgraded together. There is no active mainnet, so no fork
activation gate is required.

## Verification

- S3 orchestrator regressions: passed
  - LockedQC(A) + local same-height sibling B builds on A;
  - sibling-parent payload result emits no Proposal and schedules retry;
  - accepted result emits a Proposal whose `justify_qc.block_hash` is A;
  - an old completion cannot clear a new-view build guard.
- `cargo test -p n42-consensus --test integration_test`: 67 passed (all ten
  current modules; the task book's seven-module baseline has since expanded).
- `cargo test -p n42-consensus --test chaos_7node`: 12 passed.
- `cargo test -p n42-consensus-service --lib`: 124 passed.

The workspace checks and real Scenario 4 result are recorded before merge.
