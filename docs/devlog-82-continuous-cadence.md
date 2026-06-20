# Devlog 82: Continuous-ingest cadence rerun

Date: 2026-06-20
Branch: `feat/cadence-continuous-rerun`
Base: `923c7e1 docs(task): continuous-ingest cadence rerun for codex (devlog-82)`
Reth baseline: `449ecfdcef n42: disable jit by default (Windows, no LLVM 22) + post-merge fixes`

## Goal

Devlog 80 showed a `~31s` inter-block p95 tail, but that run used the old
sync-ingest wave harness whose 30s pool-drain window can look like a chain
cadence stall. This rerun uses `--sync-ingest-mode per-node-continuous` so the
seven node pools stay fed while the chain is committing blocks.

The questions to answer are:

1. How far does `n42_inter_block_commit_ms` drop after removing the 30s drain
   window?
2. Does leader payload build around `~2s` become the real bottleneck?
3. Do `pool_pending_at_commit` / `pool_pending_at_build_start` prove the leader
   is fed when it builds?

## Guardrails

I used a clean paired worktree instead of the dirty local `../reth` tree:

- n42 worktree: `/Users/jieliu/Documents/n42/codex-cadence-continuous-rerun/n42-26`
- reth worktree: `/Users/jieliu/Documents/n42/codex-cadence-continuous-rerun/reth`
- `git -C ../reth status --short`: clean
- `git -C ../reth log -1 --oneline`: `449ecfdcef n42: disable jit by default (Windows, no LLVM 22) + post-merge fixes`

Build:

```bash
cargo build --release --bin n42-node --bin n42-stress
```

Result: pass.

One preflight gotcha: the `/dev/tcp` port check must run under `bash`. Running
it under zsh produced false `REFUSED` results even though the node processes were
listening.

Correct preflight:

```bash
bash -lc 'for p in 19900 19901 19902 19903 19904 19905 19906; do
  (exec 3<>/dev/tcp/127.0.0.1/$p) 2>/dev/null && echo "$p OK" || echo "$p REFUSED"
done'
```

Result:

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
BASE=/tmp/n42-cadence-continuous-rerun
export N42_INJECT_PORT=19900
export N42_TWIG=1 N42_FAST_PROPOSE=1 N42_MIN_PROPOSE_DELAY_MS=0 N42_DEFER_STATE_ROOT=1
export N42_SKIP_TX_VERIFY=1 N42_MAX_TXS_PER_BLOCK=90000 N42_INJECT_HIGH_WATER=90000
export N42_INGEST_TARGET_PENDING=90000 N42_POOL_MAX_TXS=300000 N42_GAS_LIMIT=2000000000
export N42_DISABLE_TX_FORWARD=1 N42_INGEST_EXTENDED_ACK=1 N42_ASYNC_FINALIZE_FCU=0

./scripts/testnet.sh --nodes 7 --clean --no-explorer --no-tx-gen \
  --no-monitor --no-mobile-sim --block-interval 2000 \
  --data-dir "$BASE/data"
```

Stress:

```bash
BASE=/tmp/n42-cadence-continuous-rerun
ulimit -n 65536
export N42_INGEST_EXTENDED_ACK=1

RUST_LOG=info target/release/n42-stress \
  --rpc http://127.0.0.1:18000,http://127.0.0.1:18001,http://127.0.0.1:18002,http://127.0.0.1:18003,http://127.0.0.1:18004,http://127.0.0.1:18005,http://127.0.0.1:18006 \
  --ingest 127.0.0.1:19900,127.0.0.1:19901,127.0.0.1:19902,127.0.0.1:19903,127.0.0.1:19904,127.0.0.1:19905,127.0.0.1:19906 \
  --sync-ingest-mode per-node-continuous \
  --presign-load /tmp/n42-devlog79-5m-7rpc.bin \
  --wave 90000 --batch-size 500 --target-tps 0 --erc20-ratio 0 --duration 90
```

The first stress attempt failed before useful traffic with `Too many open files`.
The rerun above used `ulimit -n 65536` and completed without injection errors.

Main analysis window: `2026-06-20T10:20:02.037Z` through
`2026-06-20T10:21:32.040Z`. Later post-load/drain blocks are excluded from the
primary cadence table.

## Stress Result

| Metric | Value |
| --- | ---: |
| TCP injection duration | 90.3s |
| TCP injection sent | 1,844,004 tx |
| TCP injection errors | 0 |
| TCP injection TPS | 20,417 |
| Tx-bearing blocks in stress analysis | 19 |
| Total tx in analyzed tx-bearing blocks | 1,218,000 |
| Wall on-chain TPS over 90.3s | 13,489 |
| Stress-reported active-block overall TPS | 67,667 |
| Avg block TPS | 67,639 |
| p50 block TPS | 65,280 |
| p95 / max block TPS | 90,000 / 90,000 |
| Max tx in one block | 90,000 |
| Avg gas utilization | 67.3% |

The harness now produces repeated full 90k blocks. The remaining wall TPS gap is
from commit cadence and timeout gaps, not from empty pool drain windows.

## Cadence Table

| Metric | n | p50 | p95 | Max | Notes |
| --- | ---: | ---: | ---: | ---: | --- |
| Inter-block commit | 133 | 3.378s | 12.373s | 13.810s | all validators, tx-bearing window |
| Commit -> first build_start | 21 | 217ms | 9.996s | 10.077s | first build after parent commit |
| Pool pending at commit | 133 | 90,000 | 90,000 | 90,000 | avg 85,838; min 44,000 during warmup |
| Pool pending at build_start | 21 | 90,000 | 90,000 | 90,000 | avg 83,738; min 500 during warmup |
| Payload pack, tx-bearing | 19 | 1.000s | 1.253s | 1.253s | `N42_PAYLOAD_PACK` |
| EVM exec inside pack | 19 | 613ms | 946ms | 946ms | simple transfers, serial execution |
| Pool overhead inside pack | 19 | 265ms | 604ms | 604ms | pulling/sorting/iteration side |
| Tx-bearing payload built elapsed | 38 | 1.541s | 1.954s | 1.954s | duplicate build lines included |
| Finish total | 19 | 539ms | 953ms | 953ms | `N42_FINISH_BREAKDOWN` |
| Finish assemble_block | 19 | 535ms | 948ms | 948ms | state root deferred |
| Build_start -> broadcast | 19 | 1.894s | 2.636s | 2.636s | leader local build + finish + encode/compress |
| Leader payload serialize | 19 | 17ms | 97ms | 97ms | tx-bearing blocks |
| Leader compression | 19 | 155ms | 597ms | 597ms | compact block compression |
| Compact block compressed size | 19 | 5.1MB | 7.2MB | 7.2MB | raw p50/p95: 14.9MB / 21.1MB |
| Follower import, tx-bearing | 63 | 112ms | 439ms | 542ms | block_data -> eager import accepted |
| Follower new_payload elapsed | 63 | 38ms | 76ms | 150ms | cache-hit import path |

Representative single-validator inter-block rows from validator 5:

```text
view=248 ms=610   pool=44000
view=249 ms=1334  pool=76500
view=250 ms=2881  pool=87500
view=251 ms=3042  pool=78804
view=252 ms=2286  pool=86804
view=253 ms=2988  pool=90000
view=254 ms=3645  pool=90000
view=255 ms=3501  pool=90000
view=256 ms=4013  pool=90000
view=257 ms=3505  pool=90000
view=258 ms=4217  pool=84648
view=259 ms=2511  pool=90000
view=261 ms=12373 pool=90000
view=262 ms=3379  pool=90000
view=263 ms=3863  pool=90000
view=264 ms=2942  pool=90000
view=266 ms=12062 pool=90000
view=268 ms=13810 pool=90000
view=269 ms=1841  pool=90000
```

## Answers

### 1. Inter-block commit after removing 30s drain

The 30s drain artifact is gone. Devlog 80 had inter-block commit p95 around
`31.7s`; this continuous run drops to:

- all-validator p50 / p95: `3.378s / 12.373s`
- validator-5 representative p50 / p95-max: `3.379s / 13.810s`

This is a large improvement, but it is not yet an ideal 2s cadence. The remaining
tail lines up with pacemaker/view timeout gaps, not pool drain. The log window
contains skipped/timeout views around 260, 265, and 267 with the pool still at
90k pending.

### 2. Does leader build around 2s become the real bottleneck?

Yes, leader build is now a real bottleneck candidate:

- tx-bearing payload built elapsed p50 / p95: `1.541s / 1.954s`
- build_start -> broadcast p50 / p95: `1.894s / 2.636s`
- payload pack p50 / p95: `1.000s / 1.253s`
- EVM exec p50 / p95: `613ms / 946ms`
- finish total p50 / p95: `539ms / 953ms`

That said, leader build alone does not explain the `12-14s` inter-block p95
tail. The next bottleneck split is:

1. leader build/finish/compact encoding around the 2s slot budget,
2. view-timeout/cadence gaps that occasionally add about 10s while the pool is
   still full.

### 3. Was the leader fed?

Yes. The pool-depth probes prove the leader was fed after warmup:

- `pool_pending_at_commit` p50 / p95: `90,000 / 90,000`
- `pool_pending_at_build_start` p50 / p95: `90,000 / 90,000`
- build-start rows after warmup are almost all at 90k pending

The continuous harness removed the empty-pool drain window. When cadence stalls
now, it is not because the leader has no transactions to build.

## Interpretation

Continuous ingest fixes the harness artifact from devlog 80. The old `~31s`
p95 was primarily the sync-ingest drain window. This run keeps all seven ingest
ports open and shows full 90k blocks, full leader pools, and no injection errors.

The real chain-side work now shows up. The leader needs roughly `1.9-2.6s` from
build start to broadcast for tx-bearing blocks. Inside that, serial transfer EVM
is below 1s p95, while packing/pool overhead, block assembly, and compact block
compression together consume the rest.

Follower import is not the primary limiter in this run. Tx-bearing follower
import p95 is `439ms`, and follower `new_payload` p95 is `76ms`.

The long sustained-cadence tail is now a liveness/cadence problem rather than an
ingest problem. Timeout views still happen with `pool_pending=90000`; fixing
that path is required before build-only optimizations can translate cleanly into
wall sustained TPS.

## Next Targets

1. Investigate the timeout/skipped-view path around views 260, 265, and 267:
   why does the chain wait for the 10s base timeout while pools are full?
2. Split `build_start -> broadcast` further into payload pack, finish/assemble,
   serialize, compress, and direct push, then reduce the largest serial chunks.
3. Keep `per-node-continuous` as the default harness for cadence tests; the old
   windowed sync ingest mode is useful only as a negative control.
