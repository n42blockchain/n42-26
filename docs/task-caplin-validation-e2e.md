# Task (codex): validate Caplin EL-seam refactor on real machine + complete E2E

Branch under test: **`feat/caplin-cl-stage3-6`** (pushed, 16 commits ahead of `main`). It is
a **behavior-identical** ports-and-adapters refactor (devlog-88 stages 3-6, devlog-89 stages
7-9) that extracts `crates/n42-consensus-service` (hard-reth-free) + `crates/n42-el-rpc` +
`bin/n42-consensus-standalone`. All unit tests + `cargo check --all-targets` are green; the
ONLY thing missing before it can merge to `main` is **real-machine validation** — that's this
task. Reth baseline: `../reth` @ `449ecfdce` (revm 40.0.3 / alloy-evm 0.36.0). Confirm
`git -C ../reth log -1 --oneline` before building.

Goal: prove the refactor did NOT regress runtime behavior vs `main`, then complete the two
remaining real-machine items (stage-8 A/B, stage-9 cross-process). Produce
`docs/devlog-90-caplin-validation.md` with the numbers + a clear merge verdict.

## Part 1 — pre-merge E2E (the gate; do this first)

Build BOTH baselines from a clean checkout (same `../reth`):

```bash
# baseline = main
git checkout main && cargo build --release -p n42-node-bin -p e2e-test
# (save target/release/n42-node as n42-node.main, e2e-test as e2e-test.main)
# candidate = the branch
git checkout feat/caplin-cl-stage3-6 && cargo build --release -p n42-node-bin -p e2e-test
```

Run E2E scenarios **1 (single-node), 3 (ERC-20 contract), 4 (multi-node consensus,
correctness profile)** on BOTH builds:

```bash
E2E_SCENARIO_FILTER=1,3,4 target/release/e2e-test --binary target/release/n42-node
```

(Windows: must be a release build; multi-node goes through `e2e-test`, NOT `testnet.sh`.)

**Compare candidate vs main** — the refactor passes the gate ONLY if, for scenarios 1/3/4:
1. All three scenarios pass on the candidate (no consensus stall, no fork, blocks finalize).
2. These are statistically unchanged vs main (within noise):
   - block production cadence (commit interval p50/p95),
   - `n42_eager_import_hits_total` / eager-import behavior (Case A hit rate),
   - `n42_fcu_latency_ms`,
   - blob sidecar propagation (if scenario exercises 4844; else note N/A),
   - mobile-reward (EIP-4895 withdrawals) + staking withdrawals appear correctly at epoch
     boundaries (scenario 4 / a reward scenario — confirm the `WithdrawalSource` port
     produces the same withdrawals as main),
   - state-tree roots (jmt/twig) match across nodes and equal main's for the same workload.
3. No new panics / `tracing` span panics / errors in logs that aren't in main.

If any of these regress, STOP and report the exact diff + the suspect stage/port (the
devlogs map each port to its commit). Do NOT merge.

## Part 2 — stage-8 A/B (`N42_ASYNC_FINALIZE_FCU`)

Per `docs/task-async-finalize-fcu-ab.md`: on the candidate branch, run a 7-node sustained
workload (the devlog-82/86 continuous-ingest setup — `export N42_INJECT_PORT=19900`, port
preflight 19900..19906, do NOT skip it) twice: `N42_ASYNC_FINALIZE_FCU=0` then `=1`. Report
inter-block commit p50/p95/max, `n42_fcu_latency_ms`, and whether the devlog-69 in-loop-FCU
stall disappears with the flag on, AND that finalization correctness holds (every committed
block finalizes; no `finalize_done_rx` backlog at depth 256). If ON is a clear win with no
correctness regression, recommend flipping the default to `true` (separate commit); else keep
it opt-in and record the numbers.

## Part 3 — stage-9 cross-process (assess + complete; do not block the merge on it)

`bin/n42-consensus-standalone`'s `main` is a dev skeleton that constructs but does not
`run()` the service against a live Engine API (swarm bring-up + config loading are TODO).
- Complete the minimal wiring needed to actually run standalone consensus against a reth
  Engine API endpoint (auth RPC, typically `:8551` + a JWT hex secret). The EL target can be
  a plain reth node or an `n42-node` reth instance exposing Engine API.
- Run `n42-consensus-standalone --engine-url http://127.0.0.1:8551 --jwt-secret <path>` and
  verify it drives `engine_newPayloadV4` / `forkchoiceUpdatedV3` / `getPayloadV4` against the
  live EL (capture a few successful calls + a built/imported block).
- Fill `blob_tx_hashes` in `resolve_payload` (currently stubbed empty) IF you want standalone
  blob rebroadcast; otherwise document the limitation.
- If a live Engine-API endpoint isn't readily available in the environment, document the
  exact blocker and what's needed — this part may remain partially deferred; Parts 1-2 are
  the priority.

## Guardrails
- Do NOT change the refactor's behavior to make a test pass — if a test fails it's either a
  real regression (report it) or a test/env issue (fix the test/env, explain). The whole
  point is behavior-identical.
- No Cargo.lock drift beyond intended; no revm/alloy/reth-* downgrades; don't touch
  `crates/n42-consensus` or wire formats.
- Commits on `feat/caplin-cl-stage3-6`. Commit template:
  `GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" -m "..."` — NO "Claude" / "Co-Authored-By: Claude" strings anywhere.

## Acceptance / report back
`docs/devlog-90-caplin-validation.md` with: Part-1 candidate-vs-main comparison table + a
one-line **MERGE / NO-MERGE** verdict; Part-2 A/B numbers + default recommendation; Part-3
status (ran / blocked + why). Push the branch. Report the verdict + branch state.
