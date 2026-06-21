# Task (codex): EL-backpressure-aware build scheduler (devlog-87) — the fix

Base: `origin/main`, code parent `diagnose/el-headlag @ fddcf60` (it carries the
`ElHeadSnapshot` head-lag hook + N42_BUILD_SCHED / N42_BUILD_FCU_DIAG /
N42_BG_IMPORT_DEFER_DIAG diagnostics + reth span containment).

Reth baseline: `../reth` @ `449ecfdce`. For 0-panic runs keep reth on the paired
fork `fix/payload-span-parent-lifetime @ 93a903c130`.

## Why this task (measurement is done — now we fix)

devlog-86 quantified the high-TPS wall-tail precisely:

- **A (dominant)**: leader build FCU returns `Syncing/no-payload` because reth
  is **2-6 blocks behind** (`head_lag` p50=4, p95=6). Successful FCUs sit at
  `head_lag<=1`. The leader then burns its `(parent,view)` retry budget on FCUs
  that *cannot* produce a payload, never broadcasts, and the view times out
  (~10s) into a cascade. This is real multi-block EL lag, not a 1-block race
  (same `reth_head` across both retries within a view).
- **B**: a leader view is deferred because background import is in flight; that
  import then **fails in ~427ms** (`new_payload` → `Syncing`), and sync recovery
  does not advance the node before the 10.5s view timeout.

Root cause: under 90k-tx/block pressure, reth's import/canonical-head throughput
trails the consensus commit cadence, so consensus keeps trying to build ahead of
where the EL actually is. **The fix is to make the leader build scheduler
EL-backpressure-aware**: stop spending whole views building on a head reth can't
serve yet, and handle failed background imports with an explicit catch-up path.

## Hard requirement: gated + A/B measured (measure-first discipline)

History on this repo: 3 optimizations were implemented and then **deleted** when
data disproved their value. So:

- Put the new behavior **behind an env flag** (e.g. `N42_EL_BACKPRESSURE=1`),
  **default OFF**. Flag OFF must be byte-for-byte the current behavior.
- Deliver an **A/B** in `docs/devlog-87-el-backpressure-scheduler.md`: same
  7-node continuous-ingest workload as devlog-86, flag OFF vs flag ON, reth on
  `93a903c130` both runs.
- The change is only kept if ON beats OFF on the win condition below. If it does
  not, report that honestly and leave the flag default OFF (do not silently
  keep a non-win).

## Behavior to implement (flag ON)

1. **Backpressure gate on build-FCU.** Before issuing the build-trigger FCU as
   leader, sample `el.head_snapshot()` and compute
   `head_lag = expected_child_number - reth_head`.
   - `head_lag <= 1`: build normally (today's path).
   - `head_lag >= 2` (tune the threshold; expose as env, default 2): **do not**
     issue the FCU you already know returns `Syncing`. Instead defer the build
     and arm a one-shot "EL caught up" wake that re-checks on the next
     import-done signal and/or a short timer, triggering the build only once
     `head_lag` drops below the threshold.
   - While the leader is legitimately waiting for EL catch-up, **extend / reset
     the view pacemaker** (an EL-catch-up extension) so the view does not fall
     into the base ~10s timeout and cascade. Bound the extension (cap total
     wait) so a genuinely stuck EL still surfaces a timeout rather than hanging.
2. **Failed-import catch-up (B).** When a deferred-leader background import
   fails (`new_payload` → `Syncing`, the `N42_BG_IMPORT_DEFER_DIAG
   action=failed` case codex just added), enter an explicit catch-up: request
   sync and **do not** treat the leader as build-ready for that parent until
   `reth_head` actually reaches it. Avoid the current "fail in 427ms then sit
   idle until the 10.5s timeout" gap.
3. Keep emitting the devlog-86 diagnostics so the A/B is comparable
   (`head_lag`, full-pool timeout views, build outcomes).

Do **not** change the biased `select!` branch ordering, wire formats, or
`crates/n42-consensus/**`. This is orchestrator-side scheduling only.

## Win condition (what "better" means, ON vs OFF)

ON must reduce wasted full-pool timeout views and the wall-tail **without**
materially lowering throughput:

- Fewer unique full-pool timeout views in the stress window (target: the A-class
  `Syncing/no-payload` timeout views largely eliminated).
- Lower inter-block commit p95 / max (devlog-85/86 saw p95 up to ~94s; target a
  large drop).
- Sustained tx-bearing TPS **>=** the OFF run (peak block TPS may stay ~90k; the
  gain is steadier sustained throughput + far fewer 10s stalls), and
  `head_lag` at build time should trend toward `<=1`.

Report all three for both runs in a single comparison table. State the verdict
in one line: gain / no-gain, and the kept default.

## Guardrails

- **No Cargo.lock drift.** New dep → add to crate Cargo.toml, regenerate against
  clean `../reth` @ `449ecfdce`; verify `git diff origin/main -- Cargo.lock`
  shows only intended additions. Do not downgrade revm/alloy/reth-* pins.
- `cargo fmt --check`, `git diff --check`, `cargo check -p n42-node -p
  n42-node-bin` must pass (worktree alongside `../reth`).
- `cargo clippy -p n42-node -- -D warnings` on the touched files.
- Flag-OFF path unchanged; EL-`None` / observer paths unaffected (the trait
  default `head_snapshot()` returns `None`, so guard for that).

## Acceptance

Branch pushed (e.g. `feat/el-backpressure-scheduler`), devlog-87 with the OFF-vs-
ON A/B table + one-line verdict, no Cargo.lock drift, compiles + clippy clean.
Report branch name + verdict (gain/no-gain + kept default) back.
