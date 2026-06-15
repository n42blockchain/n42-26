# Devlog 73: Stage 8 7-node async finalize-FCU A/B

Date: 2026-06-15
Base: `main` at `ec24a04` (`Merge pull request #4 from n42blockchain/feat/consensus-async-finalize-fcu`)
Branch: `perf/stage8-7node-async-fcu-ab`
Reth baseline: unchanged, `chore/merge-upstream-fc2cc1e` at `449ecfdce`

## Scope

This is the high-load follow-up left open by devlog 72. The goal was to decide whether
`N42_ASYNC_FINALIZE_FCU=1` materially helps a 7-validator contract-heavy local network,
whether it hurts the Case A fast path, and whether the flag should be considered for
default-on.

## Build

```bash
cargo build --release -p n42-node-bin -p n42-stress -p n42-mobile-sim
```

Result: passed. The build emitted only existing dependency warnings.

## Run shape

Both runs used a clean 7-validator local testnet, Twig enabled, mobile simulation
enabled by `testnet.sh`, 2s block interval, and a 2G block gas limit.

Flag-off testnet:

```bash
N42_TWIG=1 \
N42_MAX_TXS_PER_BLOCK=48000 \
./scripts/testnet.sh \
  --nodes 7 \
  --clean \
  --no-explorer \
  --no-tx-gen \
  --no-monitor \
  --block-interval 2000 \
  --data-dir /tmp/n42-stage8-ab/flag-off
```

Flag-on testnet:

```bash
N42_ASYNC_FINALIZE_FCU=1 \
N42_TWIG=1 \
N42_MAX_TXS_PER_BLOCK=48000 \
./scripts/testnet.sh \
  --nodes 7 \
  --clean \
  --no-explorer \
  --no-tx-gen \
  --no-monitor \
  --block-interval 2000 \
  --data-dir /tmp/n42-stage8-ab/flag-on
```

Stress command for both runs:

```bash
target/release/n42-stress \
  --rpc http://127.0.0.1:18000,http://127.0.0.1:18001,http://127.0.0.1:18002,http://127.0.0.1:18003,http://127.0.0.1:18004,http://127.0.0.1:18005,http://127.0.0.1:18006 \
  --erc20-ratio 100 \
  --target-tps 2000 \
  --duration 720 \
  --accounts 5000 \
  --batch-size 500 \
  --accounts-per-batch 100 \
  --concurrency 1024
```

## Stress throughput

| Mode | Sent | RPC/HTTP errors | Blocks | Duration | Commit rate | Effective TPS | Avg RPC latency | Last-50 p95 TPS | Max tx/block |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| flag-off | 2,530,500 | 0/0 | 362 | 726s | 0.499 blocks/s | 3,487.0 | 122.9ms | 4,750.0 | 12,500 |
| flag-on | 2,530,500 | 0/0 | 363 | 726s | 0.500 blocks/s | 3,486.5 | 125.3ms | 4,716.0 | 10,000 |

Both runs had `bp_pauses=0` and `nonce_resyncs=0`.

## Consensus and FCU metrics

The table below was extracted from all seven validator logs saved before the crash
restart. `pipeline total` is `build + import + commit` from the `view committed` log
line. `commit -> FCU log lag` is the wall-clock gap from `view committed` to the
corresponding `N42_FCU` log for the same validator and view.

| Mode | FCU elapsed p50/p95/max | Commit -> FCU log lag p50/p95/max | View elapsed p95 | Pipeline total p95 | Pipeline commit p95 | Leader view p95 | Follower view p95 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| flag-off | 2/14/50ms | 13/44/229ms | 2,143ms | 578ms | 413ms | 2,080ms | 2,153ms |
| flag-on | 2/12/37ms | 55/128/278ms | 2,147ms | 573ms | 399ms | 2,110ms | 2,155ms |

The async path naturally logs FCU completion later because the FCU call is spawned off
the select loop. The actual FCU call latency remained low in both modes: p95 was 14ms
flag-off and 12ms flag-on.

## Case A and fallback behavior

There is no explicit Case A counter in the current logs. The proxy below treats
`FCU lines - status=Syncing/Case B queues` as Case A, because `status=Syncing` leads to
the background-import Case B path.

| Mode | FCU samples | Case B queued/completed | Case A proxy | Case A proxy rate | FCU retries | Failures |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| flag-off | 2,800 | 8/8 | 2,792 | 99.71% | 0 | 0 |
| flag-on | 2,737 | 1/1 | 2,736 | 99.96% | 0 | 0 |

Failure checks were clean in both runs:

- `background import FAILED`: 0
- `async finalize completion receiver dropped`: 0
- `status=Invalid`: 0
- `fork_choice_updated failed`: 0
- `new_payload failed`: 0

This run did not show a Case A hit-rate regression from enabling async finalize-FCU.

## Mobile and crash recovery

`n42-mobile-sim` stayed healthy in both modes. The saved logs show
`receipts_match=true` before crash and after restart.

Crash recovery was checked by recording a tx-bearing latest block, killing the nodes with
`kill -9`, restarting the same data dir, and verifying the recorded hash and state root
on all seven RPC ports.

| Mode | Recorded block | Tx count | Recorded hash | Recorded state root | Restart result |
| --- | ---: | ---: | --- | --- | --- |
| flag-off | `0x187` | 1,000 | `0xea3d4a3df3e148c4b9b8ca4ceb7e3df7b39a7faf632b37dba79f3e73b9a9bfde` | `0xc55f03b3967e66a80824c8658cb4b383c8cf93210ec4bb5b73f591ac9c48a900` | all 7 ports matched at latest `0x19c` |
| flag-on | `0x187` | 81 | `0x0102b67a3d5cebde1c43dd9b278032fedee4c914133e5af7d13837962d5acc52` | `0x9b3fc1fbabd5b3e1b5e2b42ec9192630994de83482dcf5a85ee078b2953a285f` | all 7 ports matched at latest `0x196` |

## Conclusion

Correctness: passed. The async path handled 7-validator contract-heavy load, mobile
verification, Case B fallback, and crash recovery without FCU failures, receiver drops,
or background-import failures.

Performance: no material win in this profile. Throughput was effectively identical
(3,487.0 TPS flag-off vs 3,486.5 TPS flag-on), commit rate stayed at about one block per
2 seconds, and pipeline p95 was essentially unchanged. FCU latency was measurable but
small relative to the 2s slot: p95 was 12-14ms, so this workload does not put enough
pressure on finalize-FCU for async offloading to move end-to-end throughput.

Recommendation: keep `N42_ASYNC_FINALIZE_FCU` default off. The flag is useful as a
correctness-proven experimental path for environments where finalize-FCU becomes
hundreds of milliseconds or blocks the select loop, but this 7-node contract-heavy run
does not justify default-on.
