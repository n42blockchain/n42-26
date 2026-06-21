# Task (codex): EL head-lag visibility before any scheduler change (devlog-86)

Base: latest `origin/main`, with the diagnose branch
`diagnose/fcu-nopayload-noleaderbuild` as the parent for code (it carries the
N42_BUILD_SCHED / N42_BUILD_FCU_DIAG diagnostics + reth span containment).

Reth baseline: `../reth` @ `449ecfdce` (revm 40.0.3 / alloy-evm 0.36.0 /
reth-primitives-traits 0.4.1). Reth span fix lives in the paired fork branch
`fix/payload-span-parent-lifetime @ 93a903c130`; keep using it for 0-panic runs.

## Why this task (and why measurement-only)

devlog-85 narrowed the 50-90s wall-tail to **EL/import readiness under high-load
cadence**, in two shapes:

- **A**: build FCU returns `Syncing` + no `payload_id` (reth says the forkchoice
  head is not buildable yet).
- **B**: node becomes leader while background import is still in flight, so the
  view-change build is deferred and recovery fires too late.

devlog-85 explicitly could NOT measure the actual reth canonical-head lag,
because the current `ExecutionLayer` seam only wraps `ConsensusEngineHandle` +
`PayloadBuilderHandle` and exposes neither the provider nor the engine-tree
head. **Do not change consensus/scheduler behavior yet.** First make the lag
visible and quantify it. This is deliberately a measurement-only pass — we fix
the scheduler only after the data confirms (and sizes) the lag.

## Deliverable 1 — narrow EL head-lag debug hook

Add ONE narrow read-only method to the `ExecutionLayer` seam to expose reth's
canonical / engine-tree head **number** (and hash if cheap) at FCU-response
time. Keep it additive and behind the seam:

- Prefer the engine-tree / canonical-in-memory head that the engine actually
  uses to decide buildability, not just the DB-latest header, so the number
  lines up with why FCU returns `Syncing`.
- If both are cheaply available, log both (`reth_canonical_head`,
  `reth_engine_tree_head`); otherwise pick the one that explains `Syncing`.
- Method must be read-only and must not perturb the hot path (no extra await on
  the per-vote path; sample only at the build-FCU attempt).

Wire it so every `N42_BUILD_FCU_DIAG` record (and the `outcome="none"` /
`outcome="err"` build-outcome logs) gains:

- `reth_head` (number),
- `expected_child_number`,
- `head_lag = expected_child_number - reth_head`.

## Deliverable 2 — rerun + quantify

Rerun the exact devlog-85 7-node continuous-ingest workload (same env, same
preflight, reth on `fix/payload-span-parent-lifetime @ 93a903c130`):

- 7 nodes, `N42_INJECT_PORT=19900`, port preflight 19900..19906 (do NOT skip —
  this was the devlog-81 discard root cause).
- `--sync-ingest-mode per-node-continuous`, `--wave 90000 --batch-size 500
  --target-tps 0 --erc20-ratio 0 --duration 240`, 90k pool.

Produce in `docs/devlog-86-el-headlag.md`:

1. Distribution of `head_lag` at every `Syncing/no-payload` build FCU
   (min/p50/p95/max, and how it evolves across a full-pool timeout view).
2. For the **B** (leader-no-build / `view_change_bg_import_pending`) views: the
   `bg_import_queue_len` and how long after the deferral the import actually
   drained vs. when the view changed — i.e. quantify "too late by how many ms".
3. A clear verdict: is A driven by a real multi-block head lag (EL genuinely
   behind), or by a transient 1-block boundary race? Numbers, not adjectives.
4. ONLY a recommendation section for the scheduler fix (backpressure-aware
   build), sized by the measured lag. Do not implement the scheduler change in
   this task.

## Guardrails

- **No Cargo.lock drift.** If you add a dep, add it to the crate Cargo.toml and
  regenerate Cargo.lock against clean `../reth` @ `449ecfdce` only. Reset
  Cargo.lock to origin/main's and let `cargo check` re-add only legit deps.
  Verify `git diff origin/main -- Cargo.lock` shows only intended additions.
- Do not downgrade revm/alloy/reth-* pins. Do not touch `crates/n42-consensus/**`.
- `cargo check -p n42-node` must pass on baseline reth `449ecfdce` (the worktree
  must sit alongside `../reth` so the path dep resolves).
- Keep all new output behind `RUST_LOG` targets already used
  (`n42::cl::exec_bridge` etc.); no new always-on log spam.

## Acceptance

Branch pushed (e.g. `diagnose/el-headlag`), devlog-86 written, no Cargo.lock
drift, compiles clean, and the head-lag numbers are concrete enough to decide
whether the next task is (a) a backpressure-aware build scheduler or (b) an
import/catch-up speedup. Report the branch name + the one-line verdict back.
