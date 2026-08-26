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

### Controlled 7-Node Binary Payload A/B

These one-minute runs use the Ethereum transaction/block path but also enable explicit benchmark
flags (`N42_SKIP_TX_VERIFY=1`, deferred state root, trusted TCP ingest). They are controlled codec and
network comparisons, not production-chain records. Source and raw-data accounting:
`docs/devlog-139-zstd-quic-direct-push-audit-20260824.md`.

| Run | Reported 60s TPS | Payload format | Full envelope | Notes |
| --- | ---: | --- | ---: | --- |
| round29 JSON control | 85,050 | legacy JSON + zstd | 15,606,245 B | Paired baseline. |
| round30 binary-v1 | 90,783 | binary-v1 + zstd | 14,957,470 B | +6.74% TPS; envelope -4.16%; direct ACK 0/42 dispatches. |
| round38 direct fixed | 90,613 | binary-v1 + zstd | 12.49 MiB full-block sample | 5,437,000 committed tx/60.002165s. Direct ACK 204/204, failure/reject 0. |
| round39 direct-only | 94,199 | binary-v1 + zstd | 12.15 MiB full-block sample | 5,652,000 committed tx/60.000901s; one final 163k proposal committed after the boundary. Six direct copies, no full-block GossipSub copy; direct ACK 222/222. |
| round42 thread/copy optimized direct-only | **136,996** | binary-v1 + zstd | 12.14 MiB full-block sample | 8,220,000 committed tx/60.001539s; first strict >100k result. Threads 16,791→3,093 versus unbounded load baseline; injection-window direct ACK 318/318. |
| round44 commit-cadence optimized direct-only | 153,197 | binary-v1 + zstd | 12.38 MB strict-window average | 9,192,000 committed tx/60.001320s; +11.83% over round42. Commit-path p95 533→218 ms; direct ACK 348/348. |
| round46 zero-copy follower decode | 152,410 | binary-v1 + zstd | 12.33 MB strict-window average | 9,145,000 committed tx/60.002453s, also 58 transaction blocks; boundary fill explains the 0.51% delta from round44. Whole-run accounting avoided 8.30 GiB of transaction-body copies; direct ACK 342/342. |
| round50 async commit + QUIC v3 chunks | 151,870 | binary-v1 + zstd | 14.97 MB full-block average | 9,112,456 committed tx/60.001845s. Async FCU 406 Valid/0 Syncing; 336 large transfers and 1,338 chunks with zero failure/retry/digest/auth errors. Control cpuset remained scheduler-managed. |
| round52 sender-run deterministic merge | **156,500** | binary-v1 + zstd | 14.96 MB full-block average | **Current strict record:** 9,390,144 committed tx/60.001004s. Sender drain 52.74→19.21 ms versus round51; 420 FCUs all Valid; 348 large transfers/1,380 chunks, all error counters zero. |

Disabling zstd is rejected by the paired isolation runs: the envelope grew to 20,697,396 bytes when
the payload outer layer was disabled and to 44,864,336 bytes when all block-payload compression was
disabled. Both exceeded active propagation limits and stalled well below the zstd-on runs.

Round38 fixes silent direct-push loss by placing connection limits before stateful libp2p
request-response behaviours. It establishes propagation reliability but does not materially change
the then-reported leader-proposal window throughput from round30; 100k remained unachieved by that
historical metric. The run used
154 aggregate node cores on average during injection, 39.1 GiB peak aggregate node RSS, 132.7 MB/s
physical `/data` NVMe writes (about 3.7% device busy), and 291.4 MB/s one-direction loopback traffic
over the broader 69-second resource window. See devlog 139 for counter definitions and raw paths.

Round39 adds the explicit benchmark switch `N42_BLOCK_DIRECT_ONLY=1`. Full-block GossipSub is skipped
only when the configured finite direct fanout is completely resolved (6/6 in this run); otherwise it
automatically remains the fallback. This isolated removal improved reported leader-proposal throughput
by 6.95% over round38 and reduced leader block-propagation bytes from 546.88 to 469.02 B/proposed tx,
but 96,915
leader-proposal TPS was still 3.08% below 100k. A parser audit that counts both follower `Decide`
and validator-0 self-leader `block committed!` events gives 94,199 strict commit TPS for round39 and
90,613 for round38. Consensus messages remain on GossipSub; healthy-path state
sync and Gov5 block fetch generated zero data requests during the measured window.

Round42 partitions the 256 logical CPUs across the seven local validators, prevents the transaction
validation runtime from creating a second host-sized Rayon set, and shares the serialized block
envelope through the consensus/import/direct-push path. It is the first strict `Decide` wall-clock
result over 100k: 136,996 TPS, 45.43% above round39 under the same corrected commit-time parser.
The comparable leader-proposal figures are 139,713 versus 96,915 TPS (+44.16%). Unbounded load-state
thread count fell from 16,791 to 3,093;
aggregate injection CPU fell from 163.04 to 63.37 cores while throughput increased. Zstd stayed on
and saved 39.79% of payload bytes. The strict committed blocks used 64.23 MB/s of logical direct
origin payload, with no full-block GossipSub origin; all 318 direct requests dispatched during the
injection window were accepted and ACKed. At round42 the gap to 1M was 7.30x and was primarily an
execution/commit-cadence architecture problem, not an SSD capacity or host-wide thread-count problem.

Round44 moves state-diff completion and staking classification off the commit critical path and keeps
the committed-block ring on the shared block allocation. Strict commit TPS rises to 153,197, 11.83%
above round42. The strict set carries 4.308 GB of six-copy direct-origin data (71.80 MB/s), no full-block
GossipSub origin, and zstd still saves 39.80% of payload bytes. The gap to 1M is now 6.53x.

Round45 tested, then rejected, a zero-millisecond eager-metadata grace period: 313 async FCUs returned
`Syncing` and strict TPS regressed to 144,495. Round50 replaces that blocking grace with an explicit
ordered `Committed → ExecutionReady → Finalized` state machine and an off-loop deadline fallback.
Its 406 FCUs were all `Valid`, pending returned to zero, and strict throughput stayed within 0.87% of
the round44 record. Round46 independently removes the per-transaction follower decode copy and counts
8.30 GiB of avoided transient copies.

Round50 also validates `/n42/block-direct/3`: one manifest plus 2--4 MiB chunks on one request
substream per block over the persistent authenticated QUIC connection. The measurement records 336
large transfers, 1,338 chunks, 4.991 GB of chunk payload in each direction, and zero transport,
queue, retry, digest, or validator-auth failures. It does **not** validate the 1M target: 58 committed
blocks in 60 seconds corresponds to about 1.03 s/block, while 1M at 163K/block requires about
163 ms/block. The sender-sharded snapshot used 8 lanes, but canonical reth EVM execution remained
serial (`n42_parallel_evm_blocks_total=0`); the record must not be described as parallel-execution TPS.

Round52 replaces the sender snapshot's per-transaction heap pop/push with an order-equivalent run
release when consecutive nonces have the same sender and effective tip. Lane-local grouping and sender
preparation average 7.82 and 3.37 ms; deterministic merge falls from round51's 42.54 ms to 8.02 ms,
and total drain falls from 52.74 to 19.21 ms. Strict TPS improves 4.88% over round51 and 2.16% over
the prior round44 record. The run still has about 989 ms inter-block cadence and zero canonical
parallel-EVM blocks, so its remaining 6.39x gap to 1M is principally execution/assemble/pipeline work,
not txpool drain, QUIC reliability, CPU capacity, or NVMe bandwidth.

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
