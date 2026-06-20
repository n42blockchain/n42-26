# Devlog 80: Inter-block cadence profiling

Date: 2026-06-20
Branch: `feat/cadence-interblock-profile`
Base: `3f8650e docs(task): inter-block cadence profiling task for codex`

## Goal

Devlog 79 showed the 7-node high-TPS harness still has large sustained-cadence gaps. This pass adds observation-only timing probes to split the gap into:

- local commit-to-commit cadence,
- first build start after a committed parent,
- leader build start to block-data broadcast,
- follower block-data receipt to eager import accepted.

No consensus behavior, payload contents, or import behavior is changed.

## Instrumentation

Added histograms and info logs:

- `n42_inter_block_commit_ms`, logged as `N42_CADENCE: inter-block commit interval`.
- `n42_commit_to_build_start_ms`, logged as `N42_CADENCE: commit->build_start`.
  - This records only the first build start for a newly committed parent, so retry builds on the same parent do not inflate the metric.
- `n42_build_start_to_broadcast_ms`, logged as `N42_CADENCE: build_start->broadcast`.
- `n42_follower_import_ms`, logged as `N42_FOLLOWER_IMPORT: block_data->accepted`.
  - This measures block-data receipt through eager `new_payload` accepted. Final FCU remains tracked separately by existing `N42_FCU` logs.

## Build

Command:

```bash
cargo build --release --bin n42-node --bin n42-stress --bin n42-mobile-sim
```

Result: pass. Only pre-existing reth warnings were emitted.

## Test Setup

Common testnet config:

```bash
N42_TWIG=1
N42_FAST_PROPOSE=1
N42_MIN_PROPOSE_DELAY_MS=0
N42_DEFER_STATE_ROOT=1
N42_SKIP_TX_VERIFY=1
N42_MAX_TXS_PER_BLOCK=90000
N42_INJECT_HIGH_WATER=90000
N42_INGEST_TARGET_PENDING=90000
N42_POOL_MAX_TXS=300000
N42_GAS_LIMIT=2000000000
N42_DISABLE_TX_FORWARD=1
N42_INGEST_EXTENDED_ACK=1
```

Testnet:

```bash
./scripts/testnet.sh --nodes 7 --clean --no-explorer --no-tx-gen \
  --no-monitor --no-mobile-sim --block-interval 2000
```

Stress:

```bash
target/release/n42-stress \
  --rpc http://127.0.0.1:18000,...,http://127.0.0.1:18006 \
  --ingest 127.0.0.1:19900,...,127.0.0.1:19906 \
  --presign-load /tmp/n42-devlog79-5m-7rpc.bin \
  --wave 90000 --batch-size 500 --target-tps 0 --erc20-ratio 0
```

Runs:

- async finalize FCU off: `/tmp/n42-devlog80-off`, 88.8s window.
- async finalize FCU on: `/tmp/n42-devlog80-on2`, 90.0s window.
- A first async-on run (`/tmp/n42-devlog80-on`) hit a non-reproduced post-60s view stall; it is kept as an observation but not used for the A/B table.

## A/B Results

| Metric | async FCU off | async FCU on |
| --- | ---: | ---: |
| Window | 88.8s | 90.0s |
| Node waves submitted | 15 x 90k | 12 x 90k |
| Submitted tx | 1.35M | 1.08M |
| Wall submitted TPS | 15.2k | 12.0k |
| Active-wave submitted TPS | 15.8k | 14.8k |
| Stress drain p50 / p95 | 30.001s / 30.006s | 30.003s / 30.063s |
| Inter-block commit p50 / p95 | 3.313s / 31.712s | 3.046s / 31.631s |
| Commit -> first build_start p50 / p95 | 371ms / 30.028s | 250ms / 30.297s |
| Build_start -> broadcast p50 / p95 | 375ms / 2.407s | 1.785s / 2.471s |
| Follower import, tx-bearing p50 / p95 | 99ms / 360ms | 103ms / 579ms |
| Follower ready -> accepted p50 / p95 | 823ms / 1.849s | 1.029s / 2.021s |
| Finalize FCU p50 / p95 | 5ms / 44ms | 6ms / 59ms |
| Tx-bearing payload build elapsed p50 / p95 | 235ms / 1.814s | 1.408s / 1.939s |
| Tx packing p50 / p95 | 135ms / 1.038s | 970ms / 1.119s |
| EVM exec p50 / p95 | 92ms / 786ms | 722ms / 950ms |
| Tx/block p50 / p95 | 81.5k / 90k | 74.5k / 90k |
| Compact block compressed p50 / p95 | 6.5MB / 7.2MB | 6.0MB / 7.2MB |

## Interpretation

The dominant tail is not finalize-FCU. FCU p95 is 44-59ms, while inter-block commit p95 is about 31.6-31.7s and commit-to-first-build p95 is about 30s. Those 30s tails line up with the per-node sync ingest drain timeout/cadence, not with the reth finalize call.

Async finalize-FCU did not improve sustained cadence in this harness. The on run was slightly worse by wall submitted TPS, but the difference is small enough to treat as harness/load variance rather than a direct FCU effect. The key result is that enabling async finalize does not remove the long inter-block gaps.

Leader build and broadcast are still non-trivial for full blocks. Build_start->broadcast p95 is about 2.4s in both runs, and tx-bearing payload build p95 is 1.8-1.9s. That is real leader-side work, but it is not the 30s sustained-cadence tail.

Follower eager import is measurable but not the sustained bottleneck in this run. Tx-bearing follower import p95 is 360-579ms, while ready-to-accepted p95 is 1.8-2.0s. That tail is below the 30s cadence gap and should be treated as a secondary import/propagation cost.

## Anomaly Note

The first async-on run produced a useful anomaly: after about 60s and 10 waves, commits stopped after view 52, then repeated build attempts for view 57 showed commit-to-build-start growing into the 90-102s range. A rerun did not reproduce it, so it is not used as the A/B result. The new probes were still useful because they made the stall visible as a cadence/liveness gap rather than an FCU latency issue.

## Conclusion

Do not enable async finalize-FCU by default based on this data. The current high-TPS sustained ceiling is dominated by harness/pool-drain/cadence gaps and leader payload build/import tails, not finalize-FCU.

Next optimization target should be the harness/cadence path: keep per-node pools continuously fed without 30s drain windows, log pool pending at commit/build_start, and then rerun the same cadence table. Once the 30s harness tail is gone, the remaining leader payload build p95 around 2s becomes the next bottleneck to attack.
