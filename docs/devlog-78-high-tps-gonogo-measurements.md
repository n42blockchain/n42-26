# Devlog 78: aggressive high-TPS go/no-go measurements

> Date: 2026-06-19
> Base: `cb2ca86` (`origin/main`, includes devlog-77)
> Branch: `feat/high-tps-gonogo-measurements`
> Goal: before building BLS-tx or replicated-mempool protocol changes, measure the
> cheap upper-bound signals first.

## 1. Probe implementation

Added a temporary TCP ingest admission probe in `crates/n42-node/src/ingest.rs`.

- Default path now performs ECDSA sender recovery with `tx.try_into_recovered()`
  and verifies it matches the sender carried by the TCP presign file.
- `N42_TRUST_PRESIGNED_SENDER=1` restores the old trusted local benchmark path:
  `Recovered::new_unchecked(tx, sender)`.
- Added per-batch `N42_INGEST_ADMISSION` logs:
  `trust_presigned_sender`, `num_txs`, `consumed`, `accepted`, `decode_errors`,
  `pool_errors`, `pending_before`, `pending_after`, `decode_us`, `recover_us`,
  `pool_insert_us`.

This flag is a measurement-only bypass. It must not be used on exposed networks.

Build used for the runs:

```bash
cargo build --release --bin n42-node --bin n42-stress --bin n42-mobile-sim
```

Shared test shape:

- 7 validators, 2s slot, `N42_TWIG=1`
- `N42_MAX_TXS_PER_BLOCK=95000`
- `N42_GAS_LIMIT=2000000000`
- pure transfer workload, 450k pre-signed txs
- presign source:
  `/tmp/n42-devlog78-single-450k.bin`

## 2. Measurement 1: ecrecover admission cost

Single-entry 90k-block run, TX forward disabled:

```bash
N42_TWIG=1 N42_DISABLE_TX_FORWARD=1 N42_TRUST_PRESIGNED_SENDER={0,1} \
N42_MAX_TXS_PER_BLOCK=95000 N42_GAS_LIMIT=2000000000 \
./scripts/testnet.sh --nodes 7 --clean --no-explorer --no-tx-gen \
  --no-monitor --no-mobile-sim --block-interval 2000

target/release/n42-stress \
  --rpc http://127.0.0.1:18000 \
  --ingest 127.0.0.1:19900 \
  --presign-load /tmp/n42-devlog78-single-450k.bin \
  --wave 90000 --duration 120 --batch-size 2000 \
  --accounts 5000 --target-tps 0 --erc20-ratio 0
```

Admission timing from `N42_INGEST_ADMISSION`:

| run | accepted | batches | recover_us p50 | recover_us p95 | recover_us max | pool_insert_us p50 | pool_insert_us p95 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| recover on | 450,000 | 225 | 62,283 | 266,864 | 598,635 | 1,465 | 1,839 |
| trusted sender | 450,000 | 225 | 0 | 0 | 0 | 1,501 | 2,139 |

Per-wave inject/drain:

| run | wave 1 ingest | later ingest range | drain behavior |
| --- | ---: | ---: | --- |
| recover on | 3,893ms | 4,213-4,419ms | drain grew from 12.9s to 29.0s |
| trusted sender | 277ms | 275-1,167ms | drain hit 30s timeout on later waves |

Stress / chain summary:

| run | stress wall TPS | stress sustained TPS | chain overall TPS | max tx/block |
| --- | ---: | ---: | ---: | ---: |
| recover on | 3,750 | 4,545 | 3,103.4 | 90,000 |
| trusted sender | 3,805 | 3,906 | 4,044.1 | 95,000 |

Interpretation:

- ECDSA recovery is a real TCP admission CPU cost. At 2k tx/batch it costs
  about 62ms p50 and 267ms p95 on this Mac.
- For a 90k single-entry wave, skipping recovery removes several seconds of
  admission time.
- On-chain TPS did not improve cleanly in this single-entry run because the
  bottleneck moved to drain / leader rotation / import cadence after admission
  became fast. This is a positive admission-headroom signal, not a proven
  end-to-end TPS multiplier.

BLS-tx verdict:

- Go for a narrow batch-verify / trusted-sender prototype if the target is
  admission headroom.
- Do not start the full protocol migration yet. First make the multi-entry
  harness produce a true high-throughput leader workload and show that admission
  is the live chain limiter there.

## 3. Measurement 2: compact block / RSV propagation

90k single-entry block propagation, using the same runs above:

| metric | recover-on 90k p50/p95 | trusted 90-95k p50/p95 |
| --- | ---: | ---: |
| leader full payload (`N42_LEADER_SERIALIZE payload_kb`) | 20,829 / 20,829 KB | 20,829 / 21,986 KB |
| leader full payload serialize | 17 / 19 ms | 17 / 18 ms |
| compact exec raw (`N42_COMPACT_BLOCK raw_kb`) | 11,245 / 11,245 KB | 11,245 / 11,867 KB |
| compact exec compressed | 281 / 281 KB | 282 / 297 KB |
| full payload compressed (`N42_COMPRESS compressed_kb`) | 6,705 / 6,705 KB | 6,705 / 7,077 KB |
| compact exec inside full payload (`exec_kb`) | 281 / 281 KB | 282 / 297 KB |
| full payload compression | 59 / 61 ms | 60 / 64 ms |
| direct push encoded size | 6,987 / 6,987 KB | 6,986 / 7,374 KB |
| leader direct send | 3 / 4 ms | 3 / 3 ms |
| total broadcast | 3 / 4 ms | 3 / 3 ms |
| follower ready->decode | 3 / 921 ms | 2 / 905 ms |
| follower ready->compact inject | 4 / 940 ms | 3 / 959 ms |
| follower ready->accept | 9 / 1,662 ms | 10 / 1,705 ms |
| follower new_payload elapsed | 1 / 548 ms | 1 / 613 ms |

7-entry global-wave run with smaller 12.8k/25.7k blocks:

| metric | p50 | p95 | max |
| --- | ---: | ---: | ---: |
| tx/block | 12,857 | 25,714 | 25,714 |
| leader payload | 2,976 KB | 5,952 KB | 5,952 KB |
| direct push encoded size | 1,001 KB | 2,004 KB | 2,005 KB |
| leader direct send | 0 ms | 0 ms | 1 ms |
| follower ready->accept | 123 ms | 400 ms | 854 ms |
| follower new_payload elapsed | 23 ms | 150 ms | 313 ms |

Interpretation:

- The execution-output compact part is already small at full 90k scale
  (`exec_kb` around 281-297KB compressed).
- The full direct-push payload is still MB-scale because it carries tx bytes:
  around 7MB for 90k/95k blocks.
- On local Mac, leader broadcast itself is not the bottleneck: direct send is
  3-4ms for 7MB payloads. The large tail is follower import / scheduling lag,
  not the leader send call.

Replicated-mempool verdict:

- No-go as the next local throughput lever. It would reduce network bytes, but
  this run does not show leader broadcast as a local critical path.
- Reconsider for WAN / real network latency, or if a future multi-machine run
  shows MB-scale payload transfer dominating follower ready latency.

## 4. Measurement 3: current 7-entry ceiling

Corrected setup:

- First attempt failed because macOS fd limit was 256; stress connected only
  `19900` and hit `Too many open files` on the other six endpoints.
- Valid run used `ulimit -n 65536` and confirmed all seven TCP ingest endpoints:
  `19900` through `19906`.
- TX Forward enabled, which uses stress global-wave semantics:

```text
SYNC_INJECT global wave mode (TX Forward ON) wave_cap=90000 per_ep_cap=12857 num_eps=7
```

Stress result:

| metric | value |
| --- | ---: |
| total tx | 450,000 |
| total errors | 0 |
| elapsed | 60.9s |
| wall TPS | 7,389 |
| sustained TPS | 7,723 |
| chain overall TPS | 7,497.8 |
| max tx/block | 25,714 |
| p95 / max block TPS | 12,857 |
| avg gas utilization | 15.2% |

Global waves:

| wave | txs | inject_ms | drain_ms | view_tps |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 89,999 | 373 | 4,658 | 19,321 |
| 2 | 89,999 | 373 | 13,445 | 6,694 |
| 3 | 89,999 | 553 | 13,387 | 6,723 |
| 4 | 89,999 | 764 | 13,270 | 6,782 |
| 5 | 89,874 | 482 | 13,443 | 6,686 |

Leader pack on tx-bearing blocks:

| metric | p50 | p95 | max |
| --- | ---: | ---: | ---: |
| tx/block | 12,857 | 25,714 | 25,714 |
| packing_ms | 31 | 60 | 73 |
| evm_exec_ms | 20 | 41 | 49 |
| pool_overhead_ms | 9 | 22 | 29 |

Interpretation:

- This current "7-entry TCP + `--wave 90000`" path does not create 90k leader
  blocks. With TX Forward enabled, stress divides the 90k wave across seven
  endpoints, so most tx-bearing blocks are 12,857 tx and the largest observed
  blocks are 25,714 tx.
- The observed ceiling on this Mac / command path is about 7.5k on-chain TPS,
  not 90k.
- The run is not gas-bound (`15.2%` average gas utilization) and not EVM-bound
  (`evm_exec_ms` p95 `41ms`). It is primarily harness / distribution / cadence
  limited.

## 5. Go/no-go summary

| proposal | signal | recommendation |
| --- | --- | --- |
| BLS-tx fast path | Strong admission signal: ECDSA recovery costs ~62ms p50 / ~267ms p95 per 2k TCP batch, and removing it cuts a 90k single-entry inject from ~4s to sub-second. | Narrow go for an admission prototype only. No full protocol go until multi-entry chain TPS is shown to be admission-limited. |
| Replicated mempool / range block | Full 90k direct-push payload is ~7MB, but local leader send is only 3-4ms; follower tail is import / scheduling. | No-go as the next local throughput project. Revisit for WAN or multi-machine payload-transfer bottlenecks. |
| Current 7-entry ceiling | Valid 7-entry run processes 450k tx at ~7.5k chain TPS and maxes at 25,714 tx/block. | First fix the high-TPS harness / tx-forward path so it actually fills leader blocks before starting large architecture work. |

Bottom line:

- The only clear positive signal is ecrecover admission cost.
- The current run does not justify building both aggressive architectures now.
- The next cheap step should be measurement hygiene: make the 7-entry workload
  produce real 90k leader blocks under TX Forward, then rerun the same admission
  and propagation probes. If that run becomes admission-limited, BLS-tx becomes
  the obvious next prototype. If it remains cadence/import-limited, BLS-tx will
  not move on-chain TPS enough by itself.

## 2026-06-20 follow-up: narrow batch-transfer fast lane

The follow-up did not start a full BLS-tx or replicated-mempool migration.
Instead it prototyped the narrowest measurable idea: a benchmark-only transfer
sidecar with 12-byte transfer records and one ECDSA signature per sender batch.

Follow-up records:

- `docs/devlog-81-batch-transfer-fastlane-bench.md`
- `docs/devlog-82-batch-transfer-fastlane-7node.md`
- `docs/devlog-83-batch-transfer-profile-optimize.md`
- `docs/performance-records.md`
- commit `8e1a077`

Clean 7-node benchmark results:

| Run | Transfers / block | TPS avg | TPS p50 | TPS p95 | TPS max |
| --- | ---: | ---: | ---: | ---: | ---: |
| Optimized 256 x 10k | 2.56M | 3.27M | 2.83M | 6.92M | 11.28M |
| Optimized release 512 x 10k | 5.12M | 3.24M | 3.04M | 6.42M | 13.33M |

This validates the upper-bound potential of the specialized compressed transfer
lane, while preserving the original caution: these are not production-chain TPS
numbers because the benchmark path skips reth/EVM execution, state root,
receipts, MDBX persistence, and production replay semantics.
