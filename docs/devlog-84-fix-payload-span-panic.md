# Devlog 84: Fix payload-build span panic

Date: 2026-06-20
Branch: `fix/payload-build-span-panic`
Base: `c7a2fbc docs(task): fix payload-build span panic for codex (devlog-84)`
Reth baseline: `449ecfdcef n42: disable jit by default (Windows, no LLVM 22) + post-merge fixes`
Reth fix branch: `fix/payload-span-parent-lifetime`
Reth fix commit: `93a903c130 fix payload worker span parent lifetime`

## Goal

Devlog 83 found the high-TPS timeout-view headwind: full pools still burned
base timeouts while the leader either built and never broadcast, or never built.
The run also showed `tracing-subscriber` panics from reth `deferred-trie` and
`payload-convert` worker threads:

```text
tried to clone Id(...), but no span exists with that ID
```

This pass fixes the span propagation panic, adds explicit per-attempt build
outcome logs, bounds retry chains, and reruns the same 7-node continuous ingest
setup.

## Code Changes

n42:

- Removed the follower eager-import owned span crossing into the async task.
- Wrapped Engine API calls that enter reth worker pools with
  `tracing::Span::none()`:
  - `fork_choice_updated_with_attrs`
  - `resolve_payload`
  - leader/follower `new_payload`
- Added `N42_BUILD_OUTCOME` with `attempt_id`, `view`, `parent`,
  `payload_id`, `outcome`, `elapsed_ms`, and `error`.
- Added structured join handling so a payload resolve panic reports
  `outcome="join_panic"` instead of silently leaving the view to timeout.
- Changed build completion from a bare signal to `BuildCompletion`, and only
  clears `building_on_parent` when the completion still matches the guarded
  parent.
- Added a per `(parent, view)` retry budget. A failed build can schedule one
  retry; further attempts log `N42_BUILD_RETRY: exhausted` and do not create an
  unbounded 2s retry chain.

reth:

- Kept the fix deliberately narrow: worker spans now use `parent: &parent_span`
  instead of moving the parent span into blocking/rayon worker closures.
- Touched paths:
  - `payload_validator.rs` (`payload-convert`, `changeset_provider`)
  - `payload_processor/mod.rs` (`sparse-trie`)
  - `payload_processor/sparse_trie.rs` (`trie-hashing`)
  - `payload_processor/prewarm.rs` (`prewarm_tx`)
  - `state_trie_overlay.rs` (`precompute_state_trie_overlay`)

## Validation

Build:

```bash
cargo build --release --bin n42-node --bin n42-stress
```

Result: pass. Only pre-existing reth warnings were emitted.

E2E:

```bash
RUST_LOG=info E2E_SCENARIO_FILTER=1,3,4 \
  E2E_SCENARIO4_PROFILE=correctness \
  target/release/e2e-test --binary target/release/n42-node
```

Result: pass.

| Scenario | Result |
| --- | --- |
| 1 single node | 99 blocks, avg interval 4.01s, balances correct |
| 3 ERC-20 | 300/300 transfers succeeded, deployer balance correct, total supply conserved |
| 4 multi-node | 1/3/5-node subtests passed; 5-node height 15 on all nodes, avg interval 7.9s |

7-node high-TPS setup:

```bash
BASE=/tmp/n42-devlog84-span-panic/run3
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

Port preflight used `bash` `/dev/tcp` plus `lsof`:

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
RUST_LOG=info target/release/n42-stress \
  --rpc http://127.0.0.1:18000,http://127.0.0.1:18001,http://127.0.0.1:18002,http://127.0.0.1:18003,http://127.0.0.1:18004,http://127.0.0.1:18005,http://127.0.0.1:18006 \
  --ingest 127.0.0.1:19900,127.0.0.1:19901,127.0.0.1:19902,127.0.0.1:19903,127.0.0.1:19904,127.0.0.1:19905,127.0.0.1:19906 \
  --sync-ingest-mode per-node-continuous \
  --presign-load /tmp/n42-devlog79-5m-7rpc.bin \
  --wave 90000 --batch-size 500 --target-tps 0 --erc20-ratio 0 --duration 240
```

## Stress Result

| Metric | Value |
| --- | ---: |
| TCP injection duration | 240.3s |
| TCP injection sent | 2,450,312 tx |
| TCP injection errors | 0 |
| TCP injection TPS | 10,197 |
| Tx-bearing blocks in stress analysis | 26 |
| Total tx in analyzed tx-bearing blocks | 1,829,216 |
| Stress-reported overall TPS | 13,963.5 |
| Avg block TPS | 58,231.5 |
| p50 / p95 block TPS | 63,744 / 90,000 |
| Max block TPS | 90,000 |
| Max tx in one block | 90,000 |
| Avg gas utilization | 73.9% |

No span panic remained:

| Search | Matches |
| --- | ---: |
| `tried to clone Id` | 0 |
| `sharded.rs` | 0 |
| `panicked at` / `!!! PANIC` | 0 |
| `join_panic` | 0 |

Build outcomes during the stress window:

| Outcome | Count |
| --- | ---: |
| `resolved` | 25 |
| `none` | 14 |
| `timeout` | 0 |
| `err` | 0 |
| `join_panic` | 0 |

Retry budget:

| Metric | Count |
| --- | ---: |
| Retry scheduled | 12 |
| Retry exhausted | 13 |

The retry budget did its job: the same parent/view no longer loops every 2s
forever. Across the whole run logs, `outcome="none"` dropped from 204 in the
first span-fix-only attempt to 30 with the budgeted retry path.

## Timing

| Metric | n | p50 | p95 | Max |
| --- | ---: | ---: | ---: | ---: |
| Inter-block commit, stress window | 182 | 3.736s | 32.209s | 62.264s |
| Inter-block commit, pool >= 90k | 128 | 4.372s | 51.378s | 62.264s |
| Build_start -> broadcast, tx-bearing | 25 | 1.355s | 2.322s | 2.477s |
| Payload pack, tx-bearing | 25 | 702ms | 1.051s | 1.130s |
| EVM exec, tx-bearing | 25 | 407ms | 817ms | 859ms |
| Pool overhead, tx-bearing | 25 | 140ms | 408ms | 529ms |
| Tx/block, tx-bearing | 25 | 82,000 | 90,000 | 90,000 |

The successful build path is healthy but still near the 2s slot tail:
`build_start -> broadcast` p95 was 2.322s. That is not enough to explain the
62s inter-block tail by itself.

## Timeout Views

Full-pool timeout sample count: 71 samples across 12 unique views.

| Bucket | Samples | Unique views |
| --- | ---: | ---: |
| Leader build started, no broadcast | 39 | 6 |
| Leader did not build | 32 | 6 |
| Broadcast happened but follower timed out | 0 | 0 |

Representative high-load timeout views:

| View | Leader | Timeout samples | Build starts | Broadcasts | Outcomes | Retry exhausted | Dominant class |
| ---: | ---: | ---: | ---: | ---: | --- | ---: | --- |
| 166 | 5 | 5 | 2 | 0 | `none=2` | 2 | build no broadcast |
| 170 | 2 | 7 | 0 | 0 | - | 0 | leader no build |
| 173 | 5 | 5 | 0 | 0 | - | 0 | leader no build |
| 174 | 6 | 7 | 2 | 0 | `none=2` | 3 | build no broadcast |
| 177 | 2 | 5 | 0 | 0 | - | 0 | leader no build |
| 180 | 5 | 7 | 2 | 0 | `none=2` | 2 | build no broadcast |
| 181 | 6 | 7 | 2 | 0 | `none=2` | 2 | build no broadcast |
| 184 | 2 | 5 | 0 | 0 | - | 0 | leader no build |
| 186 | 4 | 5 | 0 | 0 | - | 0 | leader no build |
| 187 | 5 | 7 | 2 | 0 | `none=2` | 2 | build no broadcast |
| 188 | 6 | 6 | 2 | 0 | `none=2` | 2 | build no broadcast |
| 191 | 2 | 5 | 0 | 0 | - | 0 | leader no build |

The shape changed from "span panic kills worker task" to "FCU returns no
payload id or leader does not start build". In the build-start/no-broadcast
views, `N42_BUILD_OUTCOME` records `outcome="none"`, and the retry budget then
stops the local retry chain. In the no-build views, the leader never logs
`leader_build_start`, so the issue is earlier than payload resolution.

## Comparison With Devlog 83

| Metric | Devlog 83 | Devlog 84 |
| --- | ---: | ---: |
| Span clone panics | 41 lines | 0 |
| Payload resolve join panics | not structured | 0 |
| Full-pool timeout views | 11 | 12 |
| Build-start/no-broadcast views | 9/11 | 6/12 |
| Leader no-build views | 2/11 | 6/12 |
| Inter-block p95, pool >= 90k | 31.541s | 51.378s |
| Max inter-block, pool >= 90k | 31.894s | 62.264s |
| Build_start -> broadcast p95 | 2.136s | 2.322s |
| Tx-bearing pack p95 | 1.053s | 1.051s |
| Tx-bearing EVM p95 | 787ms | 817ms |

Important: this is not a clean TPS win. The panic is fixed, and silent retry
loops are bounded, but the run still has full-pool no-broadcast/no-build views.
Wall throughput did not rise the way the original hypothesis predicted.

## Conclusion

The span-lifetime bug is fixed. The old `tracing-subscriber` panic is gone, and
payload resolve task failures are now observable through `N42_BUILD_OUTCOME`.
The retry chain is also bounded; it no longer spawns repeated no-payload
attempts forever on the same `(parent, view)`.

The remaining throughput blocker is not the original span panic. Under full
pool, timeout views still split into:

1. leader build starts but FCU returns no `payload_id` (`outcome="none"`), then
   retry budget exhausts; and
2. the leader never starts build for that view.

Next optimization should instrument the FCU no-payload path with payload status
and reth sync/tree state, and separately trace why some leader views do not call
`leader_build_start`. Pack/EVM tail is still close to the 2s slot budget, but
the current 50-60s wall tail is control-path/build-scheduling, not EVM.
