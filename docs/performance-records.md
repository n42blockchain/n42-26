# N42 Performance Records

This page is the stable index for high-throughput records. It separates production-like Ethereum transaction runs from benchmark-only upper-bound experiments so future readers do not compare incompatible numbers.

## Production-Like / Ethereum Transaction Path

These runs go through the normal transaction path and are the right references for general chain throughput discussions.

| Record | TPS | Shape | Source | Notes |
| --- | ---: | --- | --- | --- |
| Cache-hit fast path peak | 90,949 | 90K tx blocks | `docs/90K-cap-timing-analysis.md`, `docs/devlog-76-tps-bottleneck-map.md` | Peak simple-transfer record; historical Ubuntu 7-node run. |
| TCP inject + fast propose | 47,527 | 48K cap | `README.md`, `docs/devlog-49-lan-max-tps-cap-sweep.md` | Sustained injection at about 122K tx/s. |
| TCP inject + pool gate + fast propose | 45,668 | 48K cap | `README.md` | Zero nonce gaps and zero stuck tx. |
| 2s slot, all optimizations | 13,858 | 48K cap | `README.md`, `docs/devlog-76-tps-bottleneck-map.md` | More production-like timing. |

## Benchmark-Only Upper Bounds

These runs do **not** represent production-chain TPS. They intentionally bypass parts of the Ethereum/reth path to measure the ceiling of a narrow protocol idea.

### Batch Transfer Fast-Lane

Source:

- `docs/devlog-81-batch-transfer-fastlane-bench.md`
- `docs/devlog-82-batch-transfer-fastlane-7node.md`
- `docs/devlog-83-batch-transfer-profile-optimize.md`
- commit `8e1a077`

Protocol shape measured:

- one sender batch per block;
- one ECDSA signature per sender batch;
- monotonically increasing nonces inside the batch;
- per transfer record: `recipient_index u32 + amount u64` = 12 bytes;
- block hash: `blake3(encoded_batch)`;
- 7-node consensus/network shell with direct sidecar propagation.

Important caveat:

This path skips reth/EVM execution, state roots, receipts, MDBX persistence, and production replay semantics. It is an upper-bound measurement for the compressed transfer format and the surrounding consensus/network path.

| Run | Transfers / block | Encoded size | TPS avg | TPS p50 | TPS p95 | TPS max | Notes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Optimized 256 x 10k | 2,560,000 | 30,019 KiB | 3,267,444 | 2,825,607 | 6,918,919 | 11,277,533 | Clean practical setting; direct rejections 0; gossip fallback skipped. |
| Optimized release 512 x 10k | 5,120,000 | 60,038 KiB | 3,237,965 | 3,038,576 | 6,416,040 | 13,333,333 | Peak exploration setting; heavier memory and verify/apply tail. |

Profiling summary:

- macOS `sample` was used because `pprof` was not installed.
- CPU hot path moved to QUIC send/encryption plus `batch_transfer::verify_apply_block`.
- ECDSA is no longer dominant because recovery is per sender batch, not per transfer.
- 512 x 10k follower verify/apply is roughly 1s p50, so sender-sharded parallel verification/application is the next obvious benchmark optimization.

### Simple Transfer EVM CPU Microbenchmark

Source:

- `bin/n42-evm-bench`
- local validated control run on the same `revm + CacheDB` execution shape

Scope:

This measures only simple-transfer EVM execution CPU. It excludes signatures,
RLP decode/encode, txpool admission, block propagation, consensus, receipts,
state roots, MDBX persistence, and all cross-node effects.

| Run | Transactions | Time | TPS | Notes |
| --- | ---: | ---: | ---: | --- |
| `n42-evm-bench` default ETH transfer | 100,000 | 80.65 ms | 1,239,979 | Stable in-repo command: `cargo run --release --bin n42-evm-bench`. |
| Validated hot sender -> hot receiver | 1,000,000 | 465.73 ms median | 2,147,149 | One sender, sequential nonce; every transaction asserted success and `21000` gas. |
| Validated many senders -> many receivers | 1,000,000 | 874.87 ms median | 1,143,021 | Independent senders, nonce `0`; every transaction asserted success and `21000` gas. |

Interpretation:

- The CPU-only simple-transfer EVM ceiling on the tested Mac is roughly
  1.1M TPS for a many-account shape and roughly 2.1M TPS for the hottest
  single-account upper-bound shape.
- This does not make the 3.24M TPS batch fast-lane result a normal EVM result:
  at 21,000 gas per transfer, 3.24M TPS would imply about 68B gas/s before
  receipts, roots, persistence, networking, or consensus.
- Use this row as an execution CPU control when comparing compressed-transfer
  experiments against the normal Ethereum/reth path.

### Local PEVM Replay Harness

This repository is accompanied by a local sibling PEVM replay harness at
`../pevm` in the development workspace. It is a Reth-backed Ethereum block
executor/replayer with a state-read log mode. Use it as a local execution
research tool, not as an N42 network throughput result.

Local source snapshot:

- path: `../pevm`;
- git commit: `50d0530 Rewrite CLAUDE.md with comprehensive project guidance`;
- primary docs: `../pevm/CLAUDE.md`, `../pevm/README_BENCHMARK.md`,
  `../pevm/TEST_GUIDE.md`, `../pevm/compression_report.md`;
- code counters: `../pevm/src/cli/evm/mod.rs` reports live `TPS` and
  `Ggas_per_s` once per second from atomic block/tx/gas counters.

Documented design:

- DB-backed mode executes real Ethereum blocks directly from a Reth datadir.
- Recording mode wraps state reads and writes `blocks_log.bin/.idx` or
  `state_logs_data.bin/state_logs_index.bin`.
- Replay mode runs from pre-recorded state logs using `--use-log on`, avoiding
  most database state reads.
- The local guide describes log replay as 10-50x faster than database-backed
  execution, but this is a tool-level statement and should be verified with
  the run's own `Execution ... TPS=... Ggas_per_s=...` logs before quoting a
  record.

Existing local sample logs:

| File set | Entries | Block range | Size | Notes |
| --- | ---: | --- | ---: | --- |
| `../pevm/bench_logs/blocks_log.bin/.idx` | 100,001 | `8,901,500..9,000,000` | 704 MiB data + 2.3 MiB index | Legacy accumulated log format. |
| `../pevm/bench_logs/state_logs_data.bin/state_logs_index.bin` | 100,001 | `8,900,200..8,999,799` | 866 MiB data + 1.9 MiB index | Mmap v2 state-log format. |

Reproducible commands:

```bash
cd ../pevm
cargo build --release

# Generate mmap state logs for a historical block range.
./target/release/pevm evm \
  --begin 9000000 \
  --end 9100000 \
  --step 100 \
  --log-block on \
  --log-dir test_bench_logs \
  --mmap-log \
  --datadir /path/to/reth-mainnet-datadir

# Replay the same range from logs and capture TPS / Ggas/s.
./target/release/pevm evm \
  --begin 9000000 \
  --end 9100000 \
  --step 3 \
  --use-log on \
  --log-dir test_bench_logs \
  --mmap-log \
  --datadir /path/to/reth-mainnet-datadir 2>&1 | tee pevm-replay.log

rg "Execution|Ggas_per_s|TPS" pevm-replay.log
```

Compression / full-history planning data from `../pevm/compression_report.md`:

| Mode | Test range | Measured generation time | Measured size | 1-20M block extrapolated size | 1-20M block extrapolated generation time |
| --- | ---: | ---: | ---: | ---: | ---: |
| `zstd` | 600 blocks | 2.32 s | 3.93 MiB | ~130 GiB | ~64 h |
| `lz4` | 600 blocks | 2.31 s | 4.16 MiB | ~138 GiB | ~64 h |
| `none` | 600 blocks | 2.35 s | 4.57 MiB | ~152 GiB | ~65 h |

Interpretation:

- The local PEVM harness is the right place to verify claims such as "replay a
  decade-plus of Ethereum history in hours" because it has real Reth datadir
  integration and log replay. The current checked-in text supports the
  workflow, sample logs, and generation estimates, but does not include a
  citable full-history replay output with final TPS / Ggas/s.
- For documentation, quote concrete local PEVM numbers only from captured
  `Execution ... TPS=... Ggas_per_s=...` logs or from a committed run report.
- If a full-history replay completes in a few hours, add the exact command,
  datadir snapshot, block range, wall time, average / p95 `Ggas_per_s`, average
  / p95 TPS, and whether it used DB-backed mode or `--use-log on --mmap-log`.

### External Parallel-EVM References

These rows are external references only. They are useful for sizing the gap
between N42's current sequential `revm` path, the local `../pevm` replay
harness, and aggressive parallel EVM research, but they are not N42 records.

External sources:

- RISE pevm repository:
  <https://github.com/risechain/pevm>
- RISE pevm benchmark document:
  <https://github.com/risechain/pevm/blob/main/crates/pevm/benches/README.md>
- RISE pevm overview:
  <https://medium.com/@rise_chain/rise-pevm-parallel-evm-bdfc4bc9f38e>
- BAL / parallel execution research:
  <https://ethresear.ch/t/achieving-10gigagas-s-evm-execution-with-bal-and-parallel-execution/23632>
- Reth gigagas roadmap:
  <https://www.paradigm.xyz/2024/04/reth-perf>
- Supra saSTM whitepaper:
  <https://supra.com/documents/Supra_Specification_Aware_STM_whitepaper.pdf>

| Source | Workload | Scope | Reported result | Notes |
| --- | --- | --- | ---: | --- |
| RISE pevm bench | 1G gas raw transfers | mocked, no-dependency CANCUN block, in-memory state | 47,620 tx in 56.425 ms = about 844k TPS / 17.7 Ggas/s | Parallel speedup 2.82x over sequential. |
| RISE pevm bench | 1G gas ERC-20 transfers | mocked, no-dependency CANCUN block, in-memory state | 37,123 tx in 60.817 ms = about 610k TPS / 16.4 Ggas/s | Parallel speedup 4.05x. |
| RISE pevm bench | 1G gas Uniswap swaps | mocked, no-dependency CANCUN block, in-memory state | 6,413 tx in 18.707 ms = about 343k TPS / 53.5 Ggas/s | Parallel speedup 22.1x; low TPS but very high gas/s because swaps are gas-heavy. |
| RISE pevm mainnet-block bench | sampled Ethereum mainnet blocks across hardforks | in-memory state, execution microbench | average speedup about 2.02x; max speedup 4.32x; worst slowdown 0.89x | RISE's Medium summary also reports 1.73x average on randomly chosen Ethereum blocks. |
| BAL parallel execution research | 2,000 mainnet blocks, BAL preloaded state, 16-core Ryzen 5950X | pure execution, sender pre-recovered, no state root / DB commit | 14.0 Ggas/s at 50-block / 1.053G gas batches; 14.9-15.3 Ggas/s at larger batches | Sequential baseline reported as 1.212 Ggas/s; enabling sender recovery drops the mega-block case to about 5 Ggas/s. |
| Supra saSTM whitepaper | synthetic mixed ETH/ERC-20 workload based on Ethereum mainnet access patterns | in-memory REVM execution | PEVM average 229k TPS; sequential average 157k TPS | Useful TPS-scale reference for mixed workloads, but not a full-node or full-history replay result. |
| Reth roadmap | live sync, including sender recovery, transaction execution, and trie calculation | full-client direction-setting reference | 100-200 Mgas/s live sync; roadmap target 1 Ggas/s | This is a full-client context, not pure EVM CPU. |

Interpretation:

- The public PEVM data supports the same direction as N42's measurements:
  simple-transfer execution CPU is not the only limit once networking, roots,
  receipts, persistence, and cadence are included.
- Tens of Ggas/s appears in controlled pure-execution or mocked-gigagas
  settings, especially when block/batch size is large enough to expose
  parallelism.
- Public mainnet-block PEVM data is more modest: roughly 1.7-2.0x average
  speedup on sampled historical Ethereum blocks, with workload-dependent
  slowdowns possible.
- I did not find a public, citable PEVM result that exactly states
  "replayed the full 10+ year Ethereum history in a few hours." Treat that
  claim as unverified until a primary source is available.

## Interpretation Rules

- Use production-like rows when discussing normal N42 chain throughput.
- Use benchmark-only rows when discussing the theoretical ceiling of a specialized compressed transfer lane.
- Do not quote batch fast-lane numbers as Ethereum transaction TPS until state root, persistence, replay, receipts, and production validity semantics are defined and measured.
