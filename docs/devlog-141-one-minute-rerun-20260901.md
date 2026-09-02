# Devlog 141: one-minute record rerun and rejected ordered-drain fast path

Date: 2026-09-01 (UTC)

## Scope and comparison boundary

This pass searched the existing records, waited for a genuinely idle machine, rebuilt the current
`main` release binaries, reran the strict one-minute qualification, collected phase/resource/
transport data, and tested one data-driven txpool optimization. The workload remained the controlled
7-node Ethereum simple-transfer path with trusted pre-signed ingest, not the benchmark-only batch
fast lane.

The previous comparable record was round52:

- 9,390,144 committed transactions / 60.001004 s = 156,499.78 TPS;
- 163,000 transactions/block, target cadence 163 ms;
- zstd level 1, direct-only fanout 6, QUIC v3 4 MiB chunks;
- 8 execution lanes for sender preparation, asynchronous finalization, scheduler-managed CPU
  placement;
- benchmark flags `N42_SKIP_TX_VERIFY=1` and deferred state root enabled.

The 3.27M average / 13.33M peak batch-transfer fast-lane results remain excluded: they skip normal
EVM execution, receipts, roots, and canonical persistence.

## Round53: new clean baseline record

Before the run, no cargo/rustc/Go/N42 workload was active and two independent CPU gates measured
99.83% and 99.82% idle. The exact measurement window was
`20:50:17.036942Z..20:51:17.038009Z`.

| Result | round52 | round53 baseline | Change |
| --- | ---: | ---: | ---: |
| Strict committed TPS | 156,499.78 | **170,546.56** | **+8.98%** |
| Committed transactions | 9,390,144 | 10,232,976 | +842,832 |
| Full-block equivalents at 163K | 57.608 | 62.779 | +5.171 |
| Measurement time | 60.001004 s | 60.001068 s | equivalent |
| Heavy-block commit interval | 989.23 ms | 942.63 ms | -4.71% |
| Build start to broadcast | 701.60 ms | 641.68 ms | -8.54% |

The chain therefore remains 5.86x below 1M TPS. At 163K transactions/block, the measured heavy-block
cadence is still 5.78x above the approximately 163 ms target.

### Exact-window phase profile

| Phase | Samples | Mean | p95 | Maximum |
| --- | ---: | ---: | ---: | ---: |
| Payload packing | 63 | 288.94 ms | 339 ms | 349 ms |
| Canonical EVM inside packing | 63 | 229.48 ms | 273 ms | 278 ms |
| Pool/packing overhead | 63 | 58.95 ms | 67 ms | 73 ms |
| Builder finish | 64 | 174.34 ms | 186 ms | 197 ms |
| Block assembly inside finish | 64 | 161.77 ms | 169 ms | 180 ms |
| Compact execution serialization | 63 | 34.25 ms | 37 ms | 41 ms |
| Outer zstd compression | 62 | 31.39 ms | 33 ms | 48 ms |
| Build start to broadcast | 62 | 641.68 ms | 717 ms | 734 ms |
| Follower block-data to accepted | 372 | 150.85 ms | 208 ms | 237 ms |
| Async FCU | 434 | 3.17 ms | 8 ms | 60 ms |

The full binary envelope averaged 14,968,026 bytes. The logged outer payload moved from approximately
19,568 KiB raw to 13,960 KiB compressed, with about 656 KiB of compact execution data. This continues
to justify keeping zstd enabled.

### Txpool, CPU, memory, disk, and communication

| Metric | round53 value |
| --- | ---: |
| Sender drain | 20.121 ms mean |
| Sender group / prepare / deterministic merge | 7.561 / 3.758 / 8.818 ms |
| Aggregate node CPU | 77.611 CPU-core equivalents mean, 106.170 peak |
| Aggregate node RSS | 41.991 GiB mean, 55.843 GiB peak |
| `/data` NVMe read | 37.627 MB/s mean, 669.070 MB/s peak |
| `/data` NVMe write | 433.396 MB/s mean, 2,628.860 MB/s peak |
| `/data` NVMe busy | 14.078% mean, 68.400% peak |
| Logical direct send / receive | 94.018 / 94.518 MB/s |
| Loopback receive / transmit | 148.768 / 148.768 MB/s |
| `eno2np1` receive / transmit | 120.340 / 0.547 MB/s |
| Direct ACK latency | 68.685 ms mean |

The direct path sent 5.641 GB and received 5.671 GB of logical block data, including 1,500 sent and
1,508 received chunks. All direct send-failure, remote-rejection, queue-overflow, retry,
digest-mismatch, and unauthenticated-validator counters were zero. All 457 completed async FCUs were
`Valid`; the one `Committed` event not yet reflected in `ExecutionReady`/`Finalized` at the exact
counter boundary completed after the measurement.

The host UDP counters increased by 732 `RcvbufErrors` while the configured kernel receive/send maxima
remained 4 MiB. This is a host-level pressure signal worth addressing in a separate controlled sysctl
A/B, but it is not evidence of direct-push failure: the reliable QUIC application counters above were
all clean. CPU capacity and NVMe bandwidth also had substantial headroom; neither explains the gap to
1M.

## Ordered pending-snapshot experiment: rejected

Reth's current `PendingPool::all()` happens to walk an `OrdMap<TransactionId, _>`, so its concrete
snapshot is grouped by internal sender id and nonce. Two implementations tried to reuse that order
while preserving an automatic generic-pool fallback and the existing deterministic heap merge.

| Run | Environment | Strict TPS | Group | Prepare | Merge | Total drain | Decision |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| round53 original | clean | **170,546.56** | 7.561 ms | 3.758 ms | 8.818 ms | **20.121 ms** | keep |
| round54 validate then group | clean at start | 160,702.30 | 13.613 ms | 3.968 ms | 8.758 ms | 26.339 ms | reject |
| round55 fused group + fee prefix | external fleets present | 147,521.96 | 12.288 ms | 0.831 ms | 8.136 ms | 21.237 ms | reject |

Round54 proved that a full validation pass followed by sequential grouping was worse than the existing
8-lane HashMap grouping. Round55 fused order validation, grouping, and fee-prefix truncation into one
pass; all 59 measured snapshots used the ordered path and prepare fell to 0.831 ms, but sequential
grouping still made total drain 1.116 ms slower than the clean original. Round55 also overlapped an
external dormant `n42-rs` fleet and a Gov5 E2E session, so its whole-chain TPS is not a record-quality
comparison. Its phase-local drain data is sufficient to reject the candidate.

The candidate code was reverted. The repository retains the faster, already-qualified parallel
sender grouping. This avoids turning an implementation detail of Reth's current `OrdMap` into a
performance dependency without a measured gain.

## Conclusion and next optimization boundary

Round53 is the new strict controlled record. The one-round optimization experiment did not survive
measurement and therefore was not shipped. The remaining critical path is still canonical EVM
(about 229 ms), transaction/receipt block assembly (about 162 ms), and their placement in an
approximately 943 ms end-to-end heavy-block cadence. Sender drain is only about 20 ms, zstd is about
31 ms and materially reduces bytes, QUIC direct push is reliable, CPU is not saturated, and NVMe is
not saturated.

The next material optimization should therefore be a correctness-qualified live execution/commitment
pipeline: parallel execution lanes with deterministic state/receipt merge and parallel or incremental
transaction/receipt commitment construction. Further sender-drain micro-optimizations should only be
attempted with an isolated microbenchmark first.

## Artifacts

- clean record: `/data/n42-bench-artifacts-20260901/round53-main-baseline-60s`
- rejected two-pass candidate: `/data/n42-bench-artifacts-20260901/round54-preordered-drain-60s`
- rejected fused candidate: `/data/n42-bench-artifacts-20260901/round55-fused-ordered-drain-60s`
- node data: `/data/n42-bench-main-r53-20260901`, `/data/n42-bench-main-r54-20260901`,
  `/data/n42-bench-main-r55-20260901`
