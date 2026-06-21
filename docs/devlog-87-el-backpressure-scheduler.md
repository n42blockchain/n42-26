# Devlog 87: EL Backpressure-Aware Build Scheduler

Branch: `feat/el-backpressure-scheduler`

Base: `diagnose/el-headlag @ fddcf60`

Reth: `../reth fix/payload-span-parent-lifetime @ 93a903c130`

## Summary

Implemented a flag-gated EL backpressure scheduler prototype:

- `N42_EL_BACKPRESSURE=1` enables the new behavior; default is OFF.
- `N42_EL_BACKPRESSURE_HEAD_LAG_THRESHOLD` defaults to `2`.
- `N42_EL_BACKPRESSURE_RECHECK_MS` defaults to `250`.
- `N42_EL_BACKPRESSURE_MAX_WAIT_MS` defaults to `12000`.

When enabled, a leader samples `el.head_snapshot()` before build-FCU. If
`head_lag = expected_child_number - reth_head` is at or above the threshold, the
leader does not issue an FCU that is already known to return
`Syncing/no-payload`. It arms a short recheck timer and extends its local
pacemaker deadline while waiting for EL catch-up. Background-import failure also
enters this catch-up path after requesting sync.

The A/B result is **no-gain**. ON reduced FCU retry-budget exhaustion to zero,
but it did not reduce the network-level full-pool timeout problem. The leader's
local pacemaker extension does not stop the other validators from timing out and
advancing the view, so the wall tail got worse.

Verdict: **do not enable by default**. Keep `N42_EL_BACKPRESSURE` default OFF.

## Code Changes

- Added `ElBackpressureConfig` and `ElBackpressureWait` to the orchestrator.
- Added `N42_EL_BACKPRESSURE action=...` logs for `defer`, `caught_up`,
  `max_wait_exceeded`, `wake`, and `clear_view_changed`.
- Gated pre-build-FCU on EL head lag.
- Routed FCU `no_payload_id` / FCU error retry through the same gate when the
  flag is ON.
- On deferred background import failure, request sync and use the backpressure
  wait instead of immediately treating the leader as build-ready.
- Kept devlog-86 diagnostics (`N42_BUILD_FCU_DIAG`, `N42_BUILD_OUTCOME`,
  `N42_BUILD_SCHED`, `N42_TIMEOUT_VIEW`) intact.

No wire formats, `crates/n42-consensus/**`, or biased `select!` branch ordering
were changed.

## Setup

Build:

```bash
cargo build --release --bin n42-node --bin n42-stress
```

Shared 7-node environment:

```bash
export N42_INJECT_PORT=19900
export N42_TWIG=1 N42_FAST_PROPOSE=1 N42_MIN_PROPOSE_DELAY_MS=0 N42_DEFER_STATE_ROOT=1
export N42_SKIP_TX_VERIFY=1 N42_MAX_TXS_PER_BLOCK=90000 N42_INJECT_HIGH_WATER=90000
export N42_INGEST_TARGET_PENDING=90000 N42_POOL_MAX_TXS=300000 N42_GAS_LIMIT=2000000000
export N42_DISABLE_TX_FORWARD=1 N42_INGEST_EXTENDED_ACK=1 N42_ASYNC_FINALIZE_FCU=0
export RUST_LOG=info,n42::cl::timeout_diag=info,n42::cl::consensus_loop=info,n42::cl::exec_bridge=info,n42::cl::orchestrator=info,n42::cl::timeout=info
```

OFF:

```bash
BASE=/tmp/n42-devlog87-el-backpressure/off
export N42_EL_BACKPRESSURE=0
./scripts/testnet.sh --nodes 7 --clean --no-explorer --no-tx-gen \
  --no-monitor --no-mobile-sim --block-interval 2000 \
  --data-dir "$BASE/data"
```

ON:

```bash
BASE=/tmp/n42-devlog87-el-backpressure/on
export N42_EL_BACKPRESSURE=1
export N42_EL_BACKPRESSURE_HEAD_LAG_THRESHOLD=2
export N42_EL_BACKPRESSURE_RECHECK_MS=250
export N42_EL_BACKPRESSURE_MAX_WAIT_MS=12000
./scripts/testnet.sh --nodes 7 --clean --no-explorer --no-tx-gen \
  --no-monitor --no-mobile-sim --block-interval 2000 \
  --data-dir "$BASE/data"
```

Both runs passed the required ingest-port preflight:

```text
19900 OK
19901 OK
19902 OK
19903 OK
19904 OK
19905 OK
19906 OK
```

Stress command for both runs:

```bash
ulimit -n 65536
RUST_LOG=info target/release/n42-stress \
  --rpc http://127.0.0.1:18000,http://127.0.0.1:18001,http://127.0.0.1:18002,http://127.0.0.1:18003,http://127.0.0.1:18004,http://127.0.0.1:18005,http://127.0.0.1:18006 \
  --ingest 127.0.0.1:19900,127.0.0.1:19901,127.0.0.1:19902,127.0.0.1:19903,127.0.0.1:19904,127.0.0.1:19905,127.0.0.1:19906 \
  --sync-ingest-mode per-node-continuous \
  --presign-load /tmp/n42-devlog79-5m-7rpc.bin \
  --wave 90000 --batch-size 500 --target-tps 0 --erc20-ratio 0 --duration 240
```

## A/B Result

| Metric | OFF (`N42_EL_BACKPRESSURE=0`) | ON (`N42_EL_BACKPRESSURE=1`) | Direction |
| --- | ---: | ---: | --- |
| Stress window | 06:58:44.638Z - 07:02:44.635Z | 07:04:31.966Z - 07:08:31.963Z | same 240s |
| Injection sent / errors | 1,867,600 / 0 | 1,466,316 / 0 | worse |
| Injection TPS | 7,771 | 6,101 | worse |
| Tx-bearing blocks | 19 | 11 | worse |
| Total tx in analyzed tx-bearing blocks | 1,300,572 | 836,316 | worse |
| Stress overall TPS | 9,289.8 | 6,637.4 | worse |
| Avg block time | 7.8s | 12.6s | worse |
| p50 / p95 block TPS | 63,488 / 90,000 | 82,000 / 90,000 | mixed |
| Max block TPS | 90,000 | 90,000 | same |
| Max tx in one block | 90,000 | 90,000 | same |
| Inter-block commit p50 / p95 / max | 3.399s / 61.909s / 62.281s | 4.446s / 102.027s / 102.492s | worse |
| Unique full-pool timeout views | 11 | 10 | flat |
| Full-pool timeout records | 70 | 51 | lower, but not enough |
| FCU `Syncing/no-payload` | 14 | 9 | lower |
| Retry-budget-exhausted views | 7 | 0 | better |
| Panic matches | 0 | 0 | same |

OFF full-pool timeout views:

```text
159, 162, 163, 164, 166, 167, 169, 170, 171, 173, 174
```

ON full-pool timeout views:

```text
154, 157, 158, 160, 161, 162, 163, 164, 165, 167
```

## Head-Lag Comparison

`Syncing/no-payload` FCU lag:

| Run | n | Min | p50 | p95 | Max |
| --- | ---: | ---: | ---: | ---: | ---: |
| OFF | 14 | 4 | 4 | 6 | 6 |
| ON | 9 | 3 | 5 | 7 | 7 |

Successful `Valid/payload_id` FCU lag:

| Run | n | Min | p50 | p95 | Max |
| --- | ---: | ---: | ---: | ---: | ---: |
| OFF | 19 | 1 | 2 | 5 | 5 |
| ON | 12 | 1 | 1 | 3 | 3 |

ON did reduce the number of impossible FCUs, but the successful build count also
fell because views still timed out while the leader was waiting for EL catch-up.

## Backpressure-Specific ON Logs

Action counts:

| `N42_EL_BACKPRESSURE` action | Count |
| --- | ---: |
| `defer` | 867 |
| `max_wait_exceeded` | 12 |
| `clear_view_changed` | 9 |
| `caught_up` | 2 |

Backpressure-deferred views:

| View | Defer records |
| ---: | ---: |
| 151 | 8 |
| 153 | 12 |
| 154 | 42 |
| 157 | 37 |
| 158 | 77 |
| 160 | 40 |
| 161 | 47 |
| 162 | 39 |
| 163 | 121 |
| 164 | 120 |
| 165 | 120 |
| 167 | 108 |
| 168 | 48 |
| 169 | 48 |

The important pattern is `clear_view_changed`: views were being advanced out
from under the leader wait. Example from validator 6:

```text
view=160 head_lag=3 reth_head=154 expected_child_number=157
N42_EL_BACKPRESSURE action=defer wait_elapsed_ms=9899 wait_defer_count=40
N42_EL_BACKPRESSURE action=clear_view_changed wait_view=160 new_view=161 wait_elapsed_ms=10038
```

The leader did not spend retry budget on FCUs during that wait, but the rest of
the network still timed out the view. That converts the old failure mode
(`Syncing/no-payload` + retry exhaustion) into a different failure mode
(leader-only waiting + validator timeout). The current patch is therefore not a
sufficient fix.

## Interpretation

The devlog-86 diagnosis remains correct: EL is behind consensus under 90k-block
pressure. This prototype shows the first attempted fix is incomplete:

1. A leader-local scheduler can avoid known-bad FCUs.
2. It cannot by itself extend the view for validators that do not know the
   leader is legitimately waiting for EL catch-up.
3. Because there is no wire-level view-extension or backpressure signal, those
   validators still timeout near the base deadline and advance the view.
4. The ON run therefore lowers retry-budget exhaustion but worsens wall TPS and
   inter-block p95/max.

The next viable fix should not be another leader-only delay. It needs one of:

- EL/import throughput improvement so `head_lag` rarely reaches 2+.
- A consensus-visible, bounded leader-delay signal so validators can safely hold
  the same view while EL catches up.
- A build policy that proposes a safe fallback before validators timeout, rather
  than waiting locally until reth catches up.

## Validation

Passed:

```text
cargo fmt --check --package n42-node
git diff --check
cargo check -p n42-node -p n42-node-bin
cargo clippy -p n42-node -- -D warnings
cargo build --release --bin n42-node --bin n42-stress
```

`cargo fmt --check` for the whole workspace still fails on pre-existing
non-`n42-node` files in this worktree (`n42-bmt-core`, `n42-jmt`,
`n42-twig-core`, `n42-parallel-evm`, and others). Those files were not touched
for this task; formatting only `n42-node` is clean.

Compiler/clippy output includes only the existing paired-reth warnings from
`../reth`; `n42-node` is clean under `-D warnings`.

`Cargo.lock` was not changed.

## Bottom Line

`N42_EL_BACKPRESSURE=1` is a **no-gain** prototype in this form. It removes
retry-budget exhaustion but does not remove full-pool timeout views, and it
lowers sustained TPS from 9.29k to 6.64k in the measured 240s window.

Keep the feature default OFF and do not promote it without a network-visible
view-extension mechanism or an EL/import-side throughput fix.
