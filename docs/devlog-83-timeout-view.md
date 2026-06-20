# Devlog 83: Timeout-view diagnosis

Date: 2026-06-20
Branch: `feat/timeout-view-investigation`
Base: `0ce8840 docs(task): timeout-view investigation for codex (devlog-83)`
Reth baseline: `449ecfdcef n42: disable jit by default (Windows, no LLVM 22) + post-merge fixes`

## Goal

Devlog 82 removed the 30s ingest drain artifact and left a narrower question:
why do some views still burn the `~10s` base timeout while all pools are full at
90k pending?

This pass is observation-only. It adds per-view timeout diagnostics and reruns
the same 7-node continuous-ingest high-TPS setup to separate three candidates:

1. leader build overruns the 2s slot and block data arrives late;
2. one specific leader/node stalls;
3. view-change recovery adds the long tail after timeout.

## Instrumentation

Added structured `N42_TIMEOUT_VIEW` logs:

- `leader_build_start`: view, leader index, local node index, parent, pool
  depth, and whether the previous view had timed out.
- `leader_ready`: payload ready for broadcast.
- `leader_broadcast_complete`: encoded size, send/gossip/broadcast time, and
  build-start-to-broadcast time.
- `block_data_received`: follower receipt time, duplicate status, leader-ready
  timestamp delta, and whether receipt was after a local timeout.
- `timeout_triggered`: local timeout context, pool depth, last commit age,
  current phase, pending block/execution queues, build-start status, and
  background import state.
- `view_change_after_timeout`: timeout-to-view-change elapsed, old/new leaders,
  pool depth, and whether block data had arrived.

The probes do not change consensus behavior, payload contents, or import
behavior.

## Guardrails

I used clean paired worktrees:

- n42 worktree: `/Users/jieliu/Documents/n42/codex-timeout-view-devlog83/n42-26`
- reth worktree: `/Users/jieliu/Documents/n42/codex-timeout-view-devlog83/reth`
- `git -C ../reth status --short`: clean
- `git -C ../reth log -1 --oneline`: `449ecfdcef n42: disable jit by default (Windows, no LLVM 22) + post-merge fixes`

Build:

```bash
cargo build --release --bin n42-node --bin n42-stress
```

Result: pass. Only pre-existing reth warnings were emitted.

Port preflight used bash `/dev/tcp`, and all seven ingest ports were listening:

```text
19900 OK
19901 OK
19902 OK
19903 OK
19904 OK
19905 OK
19906 OK
```

## Test Setup

Testnet:

```bash
BASE=/tmp/n42-timeout-view-devlog83/run1
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

Stress:

```bash
BASE=/tmp/n42-timeout-view-devlog83/run1
ulimit -n 65536
export N42_INGEST_EXTENDED_ACK=1

RUST_LOG=info target/release/n42-stress \
  --rpc http://127.0.0.1:18000,http://127.0.0.1:18001,http://127.0.0.1:18002,http://127.0.0.1:18003,http://127.0.0.1:18004,http://127.0.0.1:18005,http://127.0.0.1:18006 \
  --ingest 127.0.0.1:19900,127.0.0.1:19901,127.0.0.1:19902,127.0.0.1:19903,127.0.0.1:19904,127.0.0.1:19905,127.0.0.1:19906 \
  --sync-ingest-mode per-node-continuous \
  --presign-load /tmp/n42-devlog79-5m-7rpc.bin \
  --wave 90000 --batch-size 500 --target-tps 0 --erc20-ratio 0 --duration 240
```

Stress window: `2026-06-20T18:11:22.447Z` through
`2026-06-20T18:15:22.449Z`.

## Stress Result

| Metric | Value |
| --- | ---: |
| TCP injection duration | 240.2s |
| TCP injection sent | 2,139,768 tx |
| TCP injection errors | 0 |
| TCP injection TPS | 8,907 |
| Tx-bearing blocks in stress analysis | 20 |
| Total tx in analyzed tx-bearing blocks | 1,421,544 |
| Stress-reported active-block overall TPS | 74,818 |
| Avg block TPS | 74,818 |
| p50 / p95 block TPS | 74,240 / 90,000 |
| Max block TPS | 90,000 |
| Max tx in one block | 90,000 |
| Avg gas utilization | 74.6% |

Successful tx-bearing payload path during the stress window:

| Metric | n | p50 | p95 | Max |
| --- | ---: | ---: | ---: | ---: |
| Inter-block commit, stress window | 147 | 3.455s | 13.267s | 31.894s |
| Inter-block commit, pool >= 90k | 105 | 3.735s | 31.541s | 31.894s |
| Build_start -> broadcast, stress window | 20 | 1.385s | 2.136s | 2.334s |
| Payload pack, tx-bearing | 20 | 874ms | 1.053s | 1.073s |
| EVM exec, tx-bearing | 20 | 630ms | 787ms | 860ms |
| Tx/block, tx-bearing | 20 | 77,440 | 90,000 | 90,000 |

The run produced enough full-pool timeout samples, and also reproduced the
leader build pressure from devlog 82.

## Run Anomaly

All validators logged runtime panics from tracing span bookkeeping:

```text
tracing-subscriber-0.3.23/src/registry/sharded.rs:306:32:
tried to clone Id(...), but no span exists with that ID
```

Counts by validator: `v0=3`, `v1=6`, `v2=4`, `v3=5`, `v4=8`, `v5=7`,
`v6=8`. Thread labels were mostly `deferred-trie` and `payload-convert`
(`deferred-trie=29`, `payload-convert=7`, unlabelled panic lines `=5`).

Because the timeout samples below show leader build attempts that never reach
`leader_ready` or broadcast, these panics are not just noise. They are a strong
suspect for the no-broadcast path and should be fixed or made fail-fast before
using this run to tune timeout policy.

## Timeout Samples

The table includes high-load timeout views with local timeout logs, minimum
local `pool_pending >= 90000`, and view `>=176`.

| view | leader | build attempts | first build->timeout ms | retry span ms | timeout nodes | pool min | block_data recv | ready | broadcast | view-change min/p50/max ms | cause |
| ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: | --- | --- |
| 176 | 1 | 5 | 9019 | 8012 | 0,2,3,5,6 | 90000 | 0 (0 pre-timeout) | 0 | 0 | 10/289/705 | leader_build_no_broadcast |
| 183 | 1 | 0 | - | - | 0,1,2,3,5 | 90000 | 0 (0 pre-timeout) | 0 | 0 | 26/154/164 | leader_no_build |
| 186 | 4 | 5 | 9704 | 8093 | 0,2,3,5,6 | 90000 | 0 (0 pre-timeout) | 0 | 0 | 2/37/211 | leader_build_no_broadcast |
| 188 | 6 | 5 | 9561 | 8008 | 0,1,2,3,4,5,6 | 90000 | 0 (0 pre-timeout) | 0 | 0 | 13/88/435 | leader_build_no_broadcast |
| 190 | 1 | 5 | 9757 | 8058 | 0,1,2,4,5,6 | 90000 | 0 (0 pre-timeout) | 0 | 0 | 8/50/257 | leader_build_no_broadcast |
| 192 | 3 | 0 | - | - | 0,1,2,3,5 | 90000 | 0 (0 pre-timeout) | 0 | 0 | 63/386/404 | leader_no_build |
| 193 | 4 | 10 | 10000 | 18039 | 0,1,2,3,4,5,6 | 90000 | 0 (0 pre-timeout) | 0 | 0 | 21/52/10058 | leader_build_no_broadcast |
| 195 | 6 | 6 | 9921 | 10014 | 0,2,3,5,6 | 90000 | 0 (0 pre-timeout) | 0 | 0 | 17/32/169 | leader_build_no_broadcast |
| 196 | 0 | 10 | 10020 | 18084 | 0,1,2,3,4,5 | 90000 | 0 (0 pre-timeout) | 0 | 0 | 11/108/10058 | leader_build_no_broadcast |
| 197 | 1 | 15 | 20000 | 28116 | 0,1,3,4,5,6 | 90000 | 0 (0 pre-timeout) | 0 | 0 | 47/5048/10065 | leader_build_no_broadcast |
| 198 | 2 | 16 | 30001 | 30061 | 0,1,2,3,4,5,6 | 90000 | 0 (0 pre-timeout) | 0 | 0 | 47/60/76 | leader_build_no_broadcast |

Summary:

| Bucket | Count | Share | Evidence |
| --- | ---: | ---: | --- |
| Leader build started, but no ready/broadcast | 9/11 | 81.8% | repeated `leader_build_start`; zero `leader_ready`, zero `leader_broadcast_complete`, zero follower `block_data_received` |
| Leader did not start build | 2/11 | 18.2% | views 183 and 192 had no local leader build log |
| Block arrived late after timeout | 0/11 | 0% | no timeout sample had any follower block data for that view |
| Single-node leader stall | 0/11 | 0% | leaders were spread: 1=>4, 4=>2, 6=>2, 0/2/3=>1 each |

View-change timing across the sample set: `n=64`, p50 `72ms`, p95 `10045ms`,
max `10065ms`.

## Candidate Analysis

### Build overran slot

This is not the primary shape of the timeout samples.

Successful tx-bearing builds are close to the 2s slot budget:
`build_start -> broadcast` p50/p95/max was `1.385s / 2.136s / 2.334s`.
That means build pressure is real and still worth optimizing.

But the 11 timeout views do not show "block data arrived after timeout". They
show no block data at all for the skipped view, and the leader side never logs
`leader_ready` or `leader_broadcast_complete`. The dominant failure is therefore
not a slightly late broadcast; it is a build/resolve/convert path that starts
and then does not emit a block for that view.

### Specific node stall

This is not a single bad node. Timeout leaders were distributed across six
leader indices:

```text
leader 1: 4 samples
leader 4: 2 samples
leader 6: 2 samples
leader 0: 1 sample
leader 2: 1 sample
leader 3: 1 sample
leader 5: 0 samples
```

The data does support a broader "leader-side build task stall/failure" bucket,
but not a hardware or per-node leader problem.

### Slow view-change

View-change is a secondary amplifier, not the first cause.

For many nodes, timeout-to-view-change is fast: p50 is `72ms`. However, the p95
is still `~10s`, and after consecutive skipped views some nodes only catch up on
their next base timeout. That explains why the wall cadence can stretch to
`30s+` once the chain enters a no-broadcast cascade.

So the next fix should not start by shortening the base timeout. The first fix
is to make the leader build/resolve path either emit a block or emit an explicit
failure quickly. Once no-broadcast views are eliminated, timeout tuning can be
re-evaluated with cleaner data.

## Conclusion

The `~10s` full-pool gaps are primarily leader-side no-broadcast views:

- 9/11 samples: leader build attempts happened repeatedly, but never reached
  `leader_ready`, never broadcast, and followers received no block data.
- 2/11 samples: the leader did not start building at all.
- 0/11 samples: block data was merely late.
- 0/11 samples: isolated to a single leader node.

The strongest next target is the spawned payload resolve / conversion / deferred
trie path. In this run, those same areas emitted tracing span panics, and the
timeout table shows exactly the symptom a killed or stuck build task would
produce: repeated build starts with no payload completion and no broadcast.

## Recommended Next Work

1. Fix or disable the tracing span propagation bug in spawned payload/deferred
   trie/convert tasks. Search for owned parent spans crossing task boundaries
   and use borrowed spans or remove parent propagation where necessary.
2. Add an explicit per-attempt build outcome log around the payload resolve task:
   `resolved`, `timeout`, `none`, `err`, `join_panic`, elapsed ms, view,
   parent, payload id, and attempt id. The current logs prove "started" and
   "not emitted", but they do not yet record the exact failure mode.
3. Prevent silent retry cascades on the same parent/view lineage. If a build is
   still in flight, either avoid starting another resolve every 2s or cancel the
   previous attempt and log that cancellation.
4. After the no-broadcast path is fixed, rerun devlog 83. If successful
   build-start-to-broadcast remains above the 2s slot, then attack
   pack/finish/compress or introduce adaptive timeout policy with clean data.
