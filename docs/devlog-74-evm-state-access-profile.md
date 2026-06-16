# Devlog 74: 7-node EVM/state-access profile

Date: 2026-06-16
Base: `main` at `dca9391` (`Merge pull request #5 from n42blockchain/perf/stage8-7node-async-fcu-ab`)
Branch: `perf/evm-state-access-profile-7node`
Reth baseline: unchanged, `chore/merge-upstream-fc2cc1e` at `449ecfdce`

## Scope

Devlog 73 showed that Stage 8 async finalize-FCU is not the throughput bottleneck in
the 7-validator contract-heavy profile: finalize-FCU p95 was only 12-14ms. This follow-up
measures the execution side instead: leader payload packing/EVM time, reth slow-block
execution/new-payload timing, state-provider cache behavior, state-root/Twig cost, and
the tx-count vs gas-limit knee.

## Build

```bash
cargo build --release --bin n42-node --bin n42-stress --bin n42-mobile-sim
```

Result: passed. The build emitted only existing dependency warnings.

## Testnet shape

All runs used a clean 7-validator local testnet, Twig enabled, mobile simulation enabled
by `testnet.sh`, no explorer, no built-in tx generator, and a 2s block interval.

```bash
N42_TWIG=1 \
N42_MAX_TXS_PER_BLOCK=<cap> \
N42_GAS_LIMIT=<gas_limit> \
N42_EXTRA_NODE_FLAGS="--engine.state-provider-metrics --engine.slow-block-threshold 0" \
./scripts/testnet.sh \
  --nodes 7 \
  --clean \
  --no-explorer \
  --no-tx-gen \
  --no-monitor \
  --block-interval 2000 \
  --data-dir /tmp/n42-evm-state-profile/<run>
```

`N42_EXTRA_NODE_FLAGS` is a small testnet-script pass-through added for this profile. It
has no effect when unset; here it exposed reth state-provider metrics and forced
slow-block timing logs for every block.

Stress used `--erc20-ratio 100` with high submission pressure:

```bash
target/release/n42-stress \
  --rpc http://127.0.0.1:18000,http://127.0.0.1:18001,http://127.0.0.1:18002,http://127.0.0.1:18003,http://127.0.0.1:18004,http://127.0.0.1:18005,http://127.0.0.1:18006 \
  --erc20-ratio 100 \
  --target-tps <8000..20000> \
  --duration <240..480> \
  --accounts 5000 \
  --batch-size 1000 \
  --accounts-per-batch 200 \
  --concurrency <2048..4096> \
  --max-pool <250000..350000> \
  --resume-pool <125000..175000>
```

## Run matrix

| Run | Gas limit | Tx cap | Stress result | Last-50 view | Max tx/block | Full-block gas | Final state |
| --- | ---: | ---: | --- | --- | ---: | ---: | --- |
| `g2-baseline` | 2G | 48,000 | 6,723,303 sent, 159 stress blocks, 483s | avg block 2.4s, overall 7,370.9 TPS, p95 block TPS 24,000 | 48,000 | 1.363B, 68.2% | all 7 RPC heads converged at `0xc2` |
| `g3-72k-hi` | 3G | 72,000 | 4,782,712 sent, 55 stress blocks, 241s | avg block 4.7s, overall 10,420.3 TPS, p95 block TPS 18,000 | 72,000 | 2.044B, 68.1% | all 7 RPC heads converged at `0x44` |
| `g4-96k` | 4G | 96,000 | stopped early at 240s; block stayed at 38 from about 120s onward | not a steady-state run | 96,000 observed in builder logs | 2.724-2.727B, about 68.1% | heads diverged from `0x26` to `0x2f` after pressure |

The full-block gas ratio is the important sweep result: 48k, 72k, and 96k full blocks all
use only about 68% of their respective gas limit. The profile is not gas-bound at these
caps. Raising only the gas limit does not buy more throughput; increasing tx/block does,
but 72k already stretches slots and 96k crosses the stable local-network boundary.

## Leader payload packing

`N42_PAYLOAD_PACK` is the best leader-side decomposition for this run. It captures the
transaction packing loop, including `evm_exec_ms`, `pool_overhead_ms`, `iter_ms`, and
small consensus/other buckets. Rows below are tx-bearing payloads; near-full means at
least 95% of that run's tx cap.

| Run | Rows | Pack p50/p95 | EVM p50/p95 | Pool overhead p50/p95 | Near-full pack p50/p95 | Near-full EVM p50/p95 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 2G/48k | 148 | 251/1,054ms | 179/868ms | 47/296ms | 419/933ms | 313/709ms |
| 3G/72k | 61 | 957/1,069ms | 700/882ms | 169/359ms | 807/957ms | 613/747ms |
| 4G/96k | 38 | 1,002/1,145ms | 693/939ms | 202/351ms | 377/952ms | 275/690ms |

The payload-builder metric `reth_n42_payload_build_ms` was also present. Aggregated
across all seven metric endpoints, average build time was 432.5ms for 2G/48k, 905.8ms
for 3G/72k, and 794.4ms for the unstable 4G/96k run. The expected
`n42_execution_block_ms` metric was not emitted on this path; this profile uses reth
builder/slow-block instrumentation instead.

## Reth execution/new-payload side

`Slow block` logs were deduplicated by block hash. Many follower rows are legitimate
compact-cache fast paths with `execution_ms=0`, so the table also isolates rows where
reth actually executed work (`execution_ms > 0`).

| Run | Unique tx-bearing blocks | execution_ms p50/p95/max | Positive-exec rows | Positive-exec p50/p95/max | Storage read slots p50/p95 | Storage write slots p50/p95 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 2G/48k | 148 | 380/2,087/2,435ms | 107 | 1,115/2,131/2,435ms | 1,428/2,362 | 1,428/2,628 |
| 3G/72k | 61 | 1,537/3,656/4,361ms | 49 | 1,774/3,811/4,361ms | 1,428/1,668 | 1,428/2,114 |
| 4G/96k | 38 | 0/3,175/3,727ms | 18 | 1,257/3,727/3,727ms | 1,428/4,048 | 1,430/4,048 |

The import/commit rows are not the original Stage 8 finalize-FCU issue. They are engine
work after payload construction and become large when the block cap is pushed: unique
commit rows had p50/p95 `commit_ms` of 2,123/3,734ms for 2G/48k, 3,110/6,228ms for
3G/72k, and 2,609/7,340ms for the unstable 4G/96k boundary run.

## State access and cache behavior

State-provider cache metrics were healthy and do not point at cold MDBX reads as the main
limit:

| Run | Account cache hit rate | Storage cache hit rate | Code cache hit rate | State-provider account fetches | State-provider storage fetches |
| --- | ---: | ---: | ---: | ---: | ---: |
| 2G/48k | 98.47% | 97.92% | 96.08% | 116,881 | 232,802 |
| 3G/72k | 98.69% | 98.67% | 95.06% | 58,771 | 117,052 |
| 4G/96k | 98.28% | 98.38% | 92.31% | 20,955 | 41,754 |

The slow-block rows agree: `state_read_ms` was effectively zero at p95 in the stable runs,
while the state-read/write slot counts remained non-zero. The workload is stateful, but
state access is mostly cache-resident in this local profile.

## State root and Twig

`N42_FINISH_BREAKDOWN` measures the reth builder finish/root/assembly path for tx-bearing
payloads. Twig timing comes from `Twig updated` rows with `storage_changes > 0` across all
validators.

| Run | state_root_ms p50/p95/max | assemble_block_ms p50/p95/max | total_finish_ms p50/p95/max | Twig root elapsed p50/p95/max |
| --- | ---: | ---: | ---: | ---: |
| 2G/48k | 8/155/349ms | 57/417/807ms | 90/565/853ms | 10/25/84ms |
| 3G/72k | 8/186/254ms | 253/511/617ms | 273/634/741ms | 10/19/69ms |
| 4G/96k | 35/168/306ms | 170/484/572ms | 246/660/767ms | 10/26/47ms |

Parallel state-root fallback counters stayed at zero in all runs:
`reth_sync_block_validation_state_root_parallel_fallback_total=0`,
`reth_sync_block_validation_state_root_task_timeout_total=0`, and
`reth_sync_block_validation_state_root_task_fallback_success_total=0`.

Twig itself is not the throughput bottleneck here. Its p95 stayed below 26ms even with
real storage changes. Reth state-root and block assembly are non-trivial but still below
the multi-second EVM/import tail observed at higher tx caps.

## Conclusion

The real knee is EVM/payload packing plus engine import/commit at large tx counts, not
finalize-FCU, not gas limit, and not Twig.

At 2G/48k the network can fill the tx cap and converge, but leader packing already has a
p95 around 1.05s and positive reth execution rows have a p95 around 2.13s. At 3G/72k the
network still converges, but the average block interval stretches to 4.7s and positive
reth execution p95 rises to 3.81s. At 4G/96k the run is no longer stable locally: block
progress stalls under pressure and validator heads diverge after the stress window.

This is not gas-bound: full 48k/72k/96k blocks all land at about 68% gas usage. The next
optimization should focus on:

1. first-class leader EVM transaction-loop instrumentation, including per-tx/account hot
   paths, because `n42_execution_block_ms` is not emitted on this reth payload path;
2. payload packing/txpool iteration and nonce/queued pressure, where `pool_overhead_ms`
   reaches p95 296-359ms in the stable runs;
3. reth import/commit tail under large blocks, especially the 72k path where commit rows
   reach p95 6.2s;
4. only after those are understood, revisit larger tx caps. Raising gas limit alone is
   the wrong lever for this workload.

No symbolized `samply`/`xctrace` flamegraph was captured in this pass; the log and metric
profile was enough to identify the throughput boundary, but function-level self-time is
still the right next measurement if we need to split `evm_exec_ms` further.
