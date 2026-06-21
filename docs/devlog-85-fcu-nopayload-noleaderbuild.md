# Devlog 85: FCU no-payload and leader no-build diagnosis

Date: 2026-06-20
Branch: `diagnose/fcu-nopayload-noleaderbuild`
Base: `9a7ab29 docs(task): diagnose FCU no-payload / leader no-build`
Parent n42 fix: `43c5e33 fix payload build span panic`
Reth baseline: `449ecfdcef n42: disable jit by default (Windows, no LLVM 22) + post-merge fixes`
Reth fix branch: `fix/payload-span-parent-lifetime`
Reth fix commit: `93a903c130 fix payload worker span parent lifetime`

## Goal

Devlog 84 removed the `tracing-subscriber` span panic, but full-pool timeout
views still split into two buckets:

- build started, but `fork_choice_updated_with_attrs` returned no
  `payload_id`; and
- the leader never entered `leader_build_start` for that view.

This pass adds enough per-view scheduling and FCU status diagnostics to separate
the two causes, then reruns the same 7-node continuous ingest workload. It also
checks whether the n42-side span containment alone fixes the panic on baseline
reth, or whether the reth worker-span fix is required.

## Instrumentation

Added `N42_BUILD_SCHED` at every path that decides whether a leader build is
triggered, scheduled, skipped, or deferred. Each record includes:

- leader identity (`is_current_leader`, `leader_idx`, `my_index`);
- parent context (`parent`, `consensus_parent_number`,
  `expected_child_number`);
- build guard state (`building_on_parent`, `guard_matches_head`,
  `build_trigger_age_ms`);
- scheduler state (`next_build_scheduled`, `next_slot_timestamp`,
  `speculative_build_hash`);
- import/finalize queues (`bg_import_in_flight`, `bg_import_queue_len`,
  `bg_import_hashes_len`, `eager_import_done_pending`,
  `finalize_done_pending`);
- local pending data (`pending_block_data_len`, `pending_executions_len`,
  `pending_finalization`, `syncing_queue_len`);
- retry state (`retry_key`, `retry_count`).

Added `N42_BUILD_FCU_DIAG` for each successful
`fork_choice_updated_with_attrs` response. It logs the FCU payload status,
`latest_valid_hash`, payload id presence, elapsed time, and the same local
queue/guard context. `N42_BUILD_OUTCOME outcome="none"` and
`outcome="err"` now include the same context.

The current `ExecutionLayer` seam only wraps reth's
`ConsensusEngineHandle` and `PayloadBuilderHandle`; it does not expose the
provider or engine-tree canonical head. This run therefore logs n42's expected
parent number at each FCU attempt and the FCU status returned by reth, but it
does not directly log the reth canonical head lag. That needs a narrow follow-up
provider/engine-tree diagnostic hook.

## Setup

Build:

```bash
cargo build --release --bin n42-node --bin n42-stress
```

7-node environment:

```bash
BASE=/tmp/n42-devlog85-fcu-nopayload/run-fix-reth
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

Port preflight passed for all ingest ports before traffic:

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

## Run A: n42 diagnostics + reth span fix

Reth: `fix/payload-span-parent-lifetime @ 93a903c130`.

Stress summary:

| Metric | Value |
| --- | ---: |
| TCP injection duration | 240.3s |
| TCP injection sent | 1,676,080 tx |
| TCP injection errors | 0 |
| TCP injection TPS | 6,976 |
| Tx-bearing blocks in stress analysis | 13 |
| Total tx in analyzed tx-bearing blocks | 777,392 |
| Stress-reported tx-bearing TPS | 64,782.7 |
| p50 / p95 block TPS | 70,656 / 90,000 |
| Max block TPS | 90,000 |
| Max tx in one block | 90,000 |
| Avg gas utilization | 62.8% |

No span panic remained with the reth fix:

| Search | Matches |
| --- | ---: |
| `tried to clone Id` | 0 |
| `sharded.rs` | 0 |
| `panicked at` / `!!! PANIC` | 0 |
| `join_panic` | 0 |

Build FCU status during the stress window:

| FCU status | Payload id | Count |
| --- | --- | ---: |
| `Valid` | yes | 15 |
| `Syncing` | no | 18 |

Build outcomes:

| Outcome | Count |
| --- | ---: |
| `resolved` | 15 |
| `none` | 18 |

Scheduling reasons:

| Action | Reason | Count |
| --- | --- | ---: |
| `defer` | `finalize_case_b_bg_import` | 37 |
| `defer` | `finalize_case_c_missing_data` | 21 |
| `schedule` | `bg_import_done_leader` | 2 |
| `schedule` | `finalize_case_a_leader` | 8 |
| `schedule` | `retry_budget` | 12 |
| `schedule` | `timeout_recovery` | 9 |
| `schedule` | `view_change_leader` | 21 |
| `skip` | `guard_existing_inflight_parent` | 8 |
| `skip` | `retry_budget_exhausted` | 15 |
| `skip` | `timer_not_leader` | 2 |
| `skip` | `view_change_bg_import_pending` | 4 |
| `skip` | `view_change_not_leader` | 150 |
| `trigger` | `fast_propose_immediate` | 31 |
| `trigger` | `timer_retry` | 10 |

Timing:

| Metric | n | p50 | p95 | Max |
| --- | ---: | ---: | ---: | ---: |
| Inter-block commit | 112 | 3.412s | 93.946s | 94.778s |

Full-pool timeout views:

| View | Leader | Build starts | Broadcasts | FCU status/outcome | Retry exhausted | Class |
| ---: | ---: | ---: | ---: | --- | ---: | --- |
| 208 | 5 | 2 | 0 | `Syncing/no-payload=2` | 2 | build started, no broadcast |
| 210 | 0 | 2 | 0 | `Syncing/no-payload=2` | 2 | build started, no broadcast |
| 211 | 1 | 2 | 0 | `Syncing/no-payload=2` | 2 | build started, no broadcast |
| 212 | 2 | 2 | 0 | `Syncing/no-payload=2` | 2 | build started, no broadcast |
| 213 | 3 | 2 | 0 | `Syncing/no-payload=2` | 2 | build started, no broadcast |
| 215 | 5 | 0 | 0 | - | 0 | leader no build |
| 217 | 0 | 0 | 0 | - | 0 | leader no build |
| 218 | 1 | 2 | 0 | `Syncing/no-payload=2` | 2 | build started, no broadcast |
| 219 | 2 | 2 | 0 | `Syncing/no-payload=2` | 2 | build started, no broadcast |

Classification:

| Class | Unique views |
| --- | ---: |
| Build started, no broadcast | 7 |
| Leader no build | 2 |

## A: FCU no-payload

Every no-payload build FCU in Run A returned:

```text
status=Syncing
has_payload_id=false
latest_valid_hash=None
```

This is not a generic missing-payload-id path. It is specifically reth saying
the forkchoice head is not buildable yet.

Visible n42 queue state at the failing FCU attempts did not show a persistent
local backlog as the immediate cause. Most no-payload samples had
`bg_import_in_flight=false`, `bg_import_queue_len=0`,
`eager_import_done_pending=0`, and `finalize_done_pending=0` when the build FCU
returned. Some samples had pending block data or a `syncing_queue_len=1`, but
that was not consistent across the no-payload set.

The time series still points to EL catch-up pressure: the same view range has
many `finalize_case_b_bg_import` / `finalize_case_c_missing_data` deferrals,
and retries only repeat `Syncing` until the `(parent, view)` retry budget is
exhausted. The immediate failure is inside the EL/engine-tree acceptance state,
not in the payload builder resolve path.

Direct reth canonical head lag was not available in this run because the n42 EL
seam does not expose provider or engine-tree state. The next diagnostic patch
should add one narrow field beside FCU status: reth canonical/engine-tree head
number at FCU response time, then compute `expected_child_number - reth_head`.

## B: leader no-build

The leader no-build samples were not caused by a stuck `building_on_parent`
guard.

Views 215 and 217 both show the leader skipping the view-change build because
background import was still in flight:

```text
action=skip
reason=view_change_bg_import_pending
bg_import_in_flight=true
bg_import_hashes_len=1
building_on_parent=None
```

The timeout recovery path later schedules a retry, but it is too late: the view
usually changes before that timer can start a build. In these samples the guard
is already clear, so the devlog-84 completion guard is not the reason the leader
misses the view.

`guard_existing_inflight_parent` still appears in the run, but not as the tail
root. In the retry-exhausted/no-payload samples `building_on_parent` is also
cleared. Retry exhaustion is therefore a symptom after repeated `Syncing` FCUs,
not the original scheduling cause.

Leader no-build cause distribution for the full-pool timeout set:

| Cause | Unique views | Evidence |
| --- | ---: | --- |
| Background import pending at leader view-change | 2 | `view_change_bg_import_pending`, `bg_import_in_flight=true` |
| Stuck `building_on_parent` guard | 0 | guard was `None` in no-build samples |
| Retry budget exhausted before first build | 0 | retry exhaustion belongs to no-payload views |
| No scheduler trigger observed | 0 | explicit skip reason was logged |

## Run B: n42 diagnostics on baseline reth

Reth was temporarily checked out to baseline `449ecfdcef` and rebuilt with the
same n42 diagnostics. This tests whether n42's `.instrument(Span::none())`
containment alone is enough.

Stress summary:

| Metric | Value |
| --- | ---: |
| TCP injection duration | 240.3s |
| TCP injection sent | 2,004,892 tx |
| TCP injection errors | 0 |
| TCP injection TPS | 8,343 |
| Tx-bearing blocks in stress analysis | 14 |
| Total tx in analyzed tx-bearing blocks | 943,828 |
| Stress-reported tx-bearing TPS | 72,602.2 |
| p50 / p95 block TPS | 79,104 / 90,000 |
| Max block TPS | 90,000 |
| Max tx in one block | 90,000 |
| Avg gas utilization | 70.8% |

Baseline reth still panicked:

| Search | Matches in stress window |
| --- | ---: |
| `tried to clone Id` / `sharded.rs` / `panicked at` / `!!! PANIC` | 106 |

Representative threads:

```text
!!! PANIC in thread 'payload-convert' !!!
!!! PANIC in thread 'deferred-trie' !!!
tried to clone Id(...), but no span exists with that ID
```

Conclusion: n42-only span containment is not sufficient on baseline reth. The
reth `parent: &parent_span` worker-span lifetime fix is required for the
0-panic result.

The scheduling/FCU shape was otherwise similar:

| FCU status | Payload id | Count |
| --- | --- | ---: |
| `Valid` | yes | 18 |
| `Syncing` | no | 20 |

| Outcome | Count |
| --- | ---: |
| `resolved` | 18 |
| `none` | 20 |

Full-pool timeout views: 10 unique views. Classification:

| Class | Unique views |
| --- | ---: |
| Build started, no broadcast | 8 |
| Leader no build | 2 |

The two leader no-build views again mapped to
`view_change_bg_import_pending`. This confirms the residual bottleneck is not
created by the reth span fix; baseline reth simply adds the old panic back on
top.

After this comparison, reth was restored to
`fix/payload-span-parent-lifetime @ 93a903c130`.

## Are A and B the same root?

They are the same family of problem, but not the exact same local guard bug.

- A is a build-start path where reth returns `Syncing` and withholds
  `payload_id`. The leader then burns its bounded retry budget on the same
  `(parent, view)` and never broadcasts.
- B is a scheduling path where the node becomes leader while background import
  is still in flight, so `handle_view_changed` deliberately defers build. The
  background import/catch-up does not complete early enough for that view, and
  timeout recovery schedules too late.

Both point to EL/import catch-up being behind the consensus cadence under
90k-block pressure. Neither points to a stuck `building_on_parent` guard.

## Recommendations

1. Add a real reth head-lag diagnostic before changing consensus behavior:
   expose provider/engine-tree canonical head number through a narrow EL debug
   method or reth Engine API debug event, and log it next to
   `N42_BUILD_FCU_DIAG`.
2. Treat `Syncing` no-payload as explicit EL backpressure. Do not keep burning
   a full view on repeated FCU attempts for the same parent without an EL
   catch-up signal or import completion signal.
3. Tighten the leader `view_change_bg_import_pending` path. If the current node
   is leader and defers due to background import, `handle_import_done` must start
   the build immediately when the queue drains, and the pacemaker may need a
   small EL-catch-up extension instead of falling into the base timeout.
4. Keep the reth span fix in the paired fork. Baseline reth still reproduces the
   span panic with 106 matching panic lines under the same workload.

## Bottom line

The remaining 50-90s wall-tail is not EVM execution and not a stuck
`building_on_parent` guard. It is EL/import readiness under high-load cadence:
reth returns `Syncing` for build FCUs in some views, and other leader views are
deferred because background import is still in flight. The next useful patch is
head-lag visibility plus a backpressure-aware build scheduler, not another
payload execution optimization.
