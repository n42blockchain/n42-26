# Devlog 86: EL head-lag visibility

Date: 2026-06-21
Branch: `diagnose/el-headlag`
Base parent: `591c4a9 diagnose fcu no-payload build stalls`
Task source: `b0dcf51 docs(task): add EL head-lag visibility task for codex`
Reth baseline: `449ecfdcef n42: disable jit by default (Windows, no LLVM 22) + post-merge fixes`
Reth fix branch used for measurement: `fix/payload-span-parent-lifetime @ 93a903c130`

## Goal

Devlog 85 split the remaining 50-90s wall-tail into two EL/import readiness
shapes:

- A: leader build starts, but build FCU returns `Syncing` and no `payload_id`.
- B: a node becomes leader while background import is in flight, so the
  view-change build is deferred and timeout recovery arrives too late.

This pass adds a narrow read-only EL head snapshot hook and reruns the same
7-node continuous-ingest workload. It does not change consensus scheduling.

## Instrumentation

Added `ExecutionLayer::head_snapshot()` with an optional `ElHeadSnapshot`:

- `canonical_number` / `canonical_hash`;
- optional `engine_tree_number` / `engine_tree_hash`.

The in-process reth handles currently expose the provider head cheaply through
`BlockchainProvider::chain_info()`. The private engine-tree head is not exposed
by the handles used in the n42 seam, so this run logs:

```text
has_reth_head=true
reth_head=<provider best canonical number>
reth_head_hash=<provider best hash>
reth_engine_tree_head=None
reth_engine_tree_hash=None
```

Every `N42_BUILD_FCU_DIAG` and `N42_BUILD_OUTCOME outcome="none"/"err"` now
includes:

```text
consensus_parent_number
expected_child_number
reth_head
head_lag = expected_child_number - reth_head
parent_lag = consensus_parent_number - reth_head
```

For B-class visibility, added observation-only `N42_BG_IMPORT_DEFER_DIAG`
records:

- `action=defer` when a leader view skips build because background import is in
  flight;
- `action=drained` when the import queue drains;
- `action=failed` when the background import fails and sync is requested;
- `action=view_changed_before_drain` if the view changes before the marker is
  cleared.

The `action=failed` clear was added after the run below exposed the failure
case. The run data still has the original `background import FAILED` line, so
the B timeline is fully reconstructable; future runs will have the explicit
`N42_BG_IMPORT_DEFER_DIAG action=failed` line as well.

## Setup

Build:

```bash
cargo build --release --bin n42-node --bin n42-stress
```

7-node environment:

```bash
BASE=/tmp/n42-devlog86-el-headlag/run2
export N42_INJECT_PORT=19900
export N42_TWIG=1 N42_FAST_PROPOSE=1 N42_MIN_PROPOSE_DELAY_MS=0 N42_DEFER_STATE_ROOT=1
export N42_SKIP_TX_VERIFY=1 N42_MAX_TXS_PER_BLOCK=90000 N42_INJECT_HIGH_WATER=90000
export N42_INGEST_TARGET_PENDING=90000 N42_POOL_MAX_TXS=300000 N42_GAS_LIMIT=2000000000
export N42_DISABLE_TX_FORWARD=1 N42_INGEST_EXTENDED_ACK=1 N42_ASYNC_FINALIZE_FCU=0
export RUST_LOG=info,n42::cl::timeout_diag=info,n42::cl::consensus_loop=info,n42::cl::exec_bridge=info,n42::cl::orchestrator=info,n42::cl::timeout=info

./scripts/testnet.sh --nodes 7 --clean --no-explorer --no-tx-gen \
  --no-monitor --no-mobile-sim --block-interval 2000 \
  --data-dir "$BASE/data"
```

Port preflight note: an initial `/dev/tcp` probe immediately after startup
reported `19900 REFUSED`; that run was discarded before stress traffic. For the
accepted run, `lsof` showed listeners and `nc -z` passed on every ingest port:

```text
19900 OK
19901 OK
19902 OK
19903 OK
19904 OK
19905 OK
19906 OK
```

Stress:

```bash
ulimit -n 65536
RUST_LOG=info target/release/n42-stress \
  --rpc http://127.0.0.1:18000,http://127.0.0.1:18001,http://127.0.0.1:18002,http://127.0.0.1:18003,http://127.0.0.1:18004,http://127.0.0.1:18005,http://127.0.0.1:18006 \
  --ingest 127.0.0.1:19900,127.0.0.1:19901,127.0.0.1:19902,127.0.0.1:19903,127.0.0.1:19904,127.0.0.1:19905,127.0.0.1:19906 \
  --sync-ingest-mode per-node-continuous \
  --presign-load /tmp/n42-devlog79-5m-7rpc.bin \
  --wave 90000 --batch-size 500 --target-tps 0 --erc20-ratio 0 --duration 240
```

Stress window:

```text
2026-06-21T03:27:56.913738Z .. 2026-06-21T03:31:56.911608Z
```

No span panic reproduced with the reth fix:

| Search | Matches |
| --- | ---: |
| `tried to clone Id` / `sharded.rs` / `panicked at` / `!!! PANIC` / `join_panic` | 0 |

## Run summary

| Metric | Value |
| --- | ---: |
| TCP injection duration | 240.3s |
| TCP injection sent | 1,607,176 tx |
| TCP injection errors | 0 |
| TCP injection TPS | 6,688 |
| Tx-bearing blocks in stress analysis | 12 |
| Total tx in analyzed tx-bearing blocks | 707,176 |
| Stress-reported tx-bearing TPS | 64,288.7 |
| p50 / p95 block TPS | 65,792 / 90,000 |
| Max block TPS | 90,000 |
| Max tx in one block | 90,000 |
| Avg gas utilization | 61.9% |

Build FCU responses during the stress window:

| FCU status | Payload id | Count |
| --- | --- | ---: |
| `Valid` | yes | 14 |
| `Syncing` | no | 20 |

Build outcomes:

| Outcome | Count |
| --- | ---: |
| `resolved` | 15 |
| `none` with `fcu_status=Syncing` | 20 |

## A: Syncing/no-payload head lag

`Syncing/no-payload` samples:

| Metric | n | Min | p50 | p95 | Max |
| --- | ---: | ---: | ---: | ---: | ---: |
| `head_lag` | 20 | 2 | 4 | 6 | 6 |
| `parent_lag` | 20 | 1 | 3 | 5 | 5 |

Distribution:

| `head_lag` | Count |
| ---: | ---: |
| 2 | 3 |
| 3 | 2 |
| 4 | 7 |
| 5 | 6 |
| 6 | 2 |

For comparison, successful `Valid/payload_id` FCUs were mostly current:

| Metric | n | Min | p50 | p95 | Max |
| --- | ---: | ---: | ---: | ---: | ---: |
| `head_lag` | 14 | 1 | 1 | 3 | 3 |
| `parent_lag` | 14 | 0 | 0 | 2 | 2 |

Per-view `Syncing/no-payload` series:

| View | Leader node | Attempts | `reth_head` | `expected_child_number` | `head_lag` | Timestamps |
| ---: | ---: | ---: | --- | --- | --- | --- |
| 223 | 6 | 1 | 220 | 222 | 2 | 03:28:14.780 |
| 227 | 3 | 1 | 224 | 228 | 4 | 03:28:29.409 |
| 228 | 4 | 2 | 225, 225 | 229, 229 | 4, 4 | 03:28:34.832, 03:28:36.834 |
| 230 | 6 | 2 | 226, 226 | 231, 231 | 5, 5 | 03:28:46.182, 03:28:48.187 |
| 231 | 0 | 2 | 225, 225 | 229, 229 | 4, 4 | 03:28:56.697, 03:28:58.749 |
| 232 | 1 | 2 | 226, 226 | 228, 228 | 2, 2 | 03:29:17.201, 03:29:19.203 |
| 233 | 2 | 2 | 226, 226 | 230, 230 | 4, 4 | 03:29:47.218, 03:29:49.261 |
| 234 | 3 | 2 | 226, 226 | 231, 231 | 5, 5 | 03:30:17.256, 03:30:19.259 |
| 235 | 4 | 2 | 225, 225 | 231, 231 | 6, 6 | 03:30:47.340, 03:30:49.350 |
| 238 | 0 | 2 | 225, 225 | 230, 230 | 5, 5 | 03:31:29.542, 03:31:31.607 |
| 239 | 1 | 2 | 226, 226 | 229, 229 | 3, 3 | 03:31:50.043, 03:31:52.102 |

The repeated retry pattern matters: inside a single view the second FCU retry
usually sees the same `reth_head` and the same `head_lag`. For example view 235
stays at `reth_head=225` while the leader needs child 231 on both attempts,
roughly two seconds apart. That means the no-payload path is not a transient
1-block boundary race.

## Full-pool timeout classification

All 50 timeout-triggered records in the stress window had
`pool_pending=90000`. Unique full-pool timeout views:

```text
228, 230, 231, 232, 233, 234, 235, 237, 238
```

| View | Leader | Build starts | FCU shape | Retry exhausted | B skip | Class |
| ---: | ---: | ---: | --- | ---: | ---: | --- |
| 228 | 4 | 2 | `Syncing/no-payload=2`, `head_lag=4` | 2 | 0 | build started, no broadcast |
| 230 | 6 | 2 | `Syncing/no-payload=2`, `head_lag=5` | 2 | 0 | build started, no broadcast |
| 231 | 0 | 2 | `Syncing/no-payload=2`, `head_lag=4` | 2 | 0 | build started, no broadcast |
| 232 | 1 | 2 | `Syncing/no-payload=2`, `head_lag=2` | 2 | 0 | build started, no broadcast |
| 233 | 2 | 2 | `Syncing/no-payload=2`, `head_lag=4` | 2 | 0 | build started, no broadcast |
| 234 | 3 | 2 | `Syncing/no-payload=2`, `head_lag=5` | 2 | 0 | build started, no broadcast |
| 235 | 4 | 2 | `Syncing/no-payload=2`, `head_lag=6` | 2 | 0 | build started, no broadcast |
| 237 | 6 | 0 | none | 0 | 1 | leader no-build |
| 238 | 0 | 2 | `Syncing/no-payload=2`, `head_lag=5` | 2 | 0 | build started, no broadcast |

The dominant full-pool timeout class is therefore A: the leader did start
build, but reth was multiple blocks behind and returned `Syncing` until retry
budget exhaustion.

## B: leader no-build timing

Only one unique full-pool timeout view was B-class in this run: view 237 on
node 6.

Relevant timeline:

| Time | Event | Details |
| --- | --- | --- |
| 03:31:19.048 | `N42_BUILD_SCHED action=defer reason=finalize_case_b_bg_import` | node 6 is leader for view 237; parent hash `0x98d8608c...`; no build guard |
| 03:31:19.048 | `spawning background import` | import view 236 block `0x98d8608c...` |
| 03:31:19.053 | `N42_BUILD_SCHED action=skip reason=view_change_bg_import_pending` | `bg_import_in_flight=true`, `bg_import_queue_len=0`, `bg_import_hashes_len=1`, `building_on_parent=None` |
| 03:31:19.053 | `N42_BG_IMPORT_DEFER_DIAG action=defer` | marker for deferred leader view 237 |
| 03:31:19.475 | `bg import: new_payload rejected status=Syncing` | reth still not ready for the block |
| 03:31:19.480 | `background import FAILED, requesting sync` | failure 427ms after defer marker; sync requested for local view 236 -> target 237 |
| 03:31:19.571 | `received sync response blocks=0 peer_committed_view=236` | sync did not advance this node to the needed head |
| 03:31:29.461 | `timeout_triggered view=237` | `pool_pending=90000`, `build_started=false`, `bg_import_in_flight=false` |
| 03:31:29.557 | `view_change_after_timeout 237 -> 238` | 10.504s after defer marker |

Run interpretation:

- The original B deferral was real: view 237 skipped the view-change build
  because one background import was in flight.
- It did not drain late. It failed quickly: 427ms after the defer marker,
  `new_payload` returned `Syncing` and the node requested sync.
- The expensive part is recovery after the failed import: the node was still
  not build-ready when view 237 timed out 10.504s after the marker.
- The run was captured before the explicit `action=failed` marker was added,
  so two later `view_changed_before_drain` lines at 10.504s and 30.998s are
  diagnostic-marker artifacts. The code now clears the marker on failure and
  will log `N42_BG_IMPORT_DEFER_DIAG action=failed` instead.

## Verdict

A is driven by real multi-block EL head lag. On `Syncing/no-payload` FCUs,
`head_lag` is never 0 or 1; it is 2-6 blocks, with p50 4 and p95 6. Successful
FCUs mostly sit at `head_lag=1` / `parent_lag=0`, so the lag field separates
healthy build attempts from no-payload attempts.

B is the same EL/import readiness family, not a stuck scheduler guard:
`building_on_parent=None` and the build skip reason is explicit
`view_change_bg_import_pending`. In this run the blocked background import
failed in 427ms because reth returned `Syncing`, then sync recovery did not
advance fast enough before the 10.5s view timeout.

## Recommendation

The next task should be a backpressure-aware build scheduler plus import/sync
recovery handling, sized for a 2-6 block EL lag under 90k-block pressure:

1. Treat build-FCU `Syncing/no-payload` as an EL backpressure signal tied to the
   measured `head_lag`, rather than spending the normal view on repeated FCU
   attempts that cannot produce a payload.
2. When a leader view is deferred by background import, distinguish `drained`
   from `failed`. A failed import should enter an explicit catch-up path and
   avoid pretending the leader can build on that parent until reth head reaches
   it.
3. Add a deeper reth engine-tree/private-head hook if the next patch needs to
   separate provider canonical lag from engine-tree pending-state lag. The
   current provider head was enough to prove multi-block lag, but it does not
   expose the exact internal tree head used by FCU buildability.

## Validation

Completed before this devlog:

```text
cargo fmt --package n42-node --package n42-node-bin --check
git diff --check
cargo check -p n42-node
cargo check -p n42-node-bin
cargo check -p n42-node with ../reth at 449ecfdcef
cargo build --release --bin n42-node --bin n42-stress
```

All commands passed. The only compiler output was the pre-existing reth warning
set already seen in earlier devlogs.

## Bottom line

The residual high-TPS wall-tail is no longer a vague cadence issue. In this
run, full-pool timeout views are dominated by leaders trying to build while EL
is 2-6 blocks behind, causing `Syncing/no-payload` and retry exhaustion. The
B-class no-build path is also EL readiness: background import hits `Syncing`,
fails quickly, and sync recovery is too slow for the view.
