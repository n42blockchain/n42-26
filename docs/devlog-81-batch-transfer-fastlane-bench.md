# Devlog 81: Batch transfer fast-lane benchmark

Branch: `feat/continuous-ingest-cadence`
Base: `7a4a8b8 perf(observability): inter-block cadence + follower import probes (#12)`
Reth path dependency baseline: `449ecfdcef n42: disable jit by default (Windows, no LLVM 22) + post-merge fixes`

Note: the local `../reth` worktree is dirty, but HEAD is still the expected baseline.

## Why

The regular Ethereum transfer path pays per-transaction overhead for RLP size,
signature recovery, tx-root hashing, propagation, and EVM transaction plumbing.
For a special benchmark-only workload where one address emits many simple
transfers in a block, the interesting question is how high the surrounding
system ceiling can go if those transfers are represented as:

- one sender batch per block,
- one ECDSA signature per sender batch,
- monotonically increasing nonces inside that batch,
- 12 bytes per transfer record.

This does not implement a consensus/EL transaction type yet. It is a cheap
go/no-go benchmark for the protocol idea.

## Implementation

Added `n42-stress --batch-transfer-bench`.

Synthetic encoding:

- block prefix: `u32 batch_count`
- per sender batch: `start_nonce u64`, `count u32`, `signature 65B`
- per transfer record: `recipient_index u32`, `amount u64`
- signature prehash: `blake3(domain || chain_id || block_number || start_nonce || count || blake3(records))`

At 10k transfers per sender, the 77-byte batch header amortizes to
`12.008 bytes/transfer`. This is intentionally close to the proposed 12B target.

The benchmark measures:

- encoded bytes and compression vs normal signed EIP-1559 transfer RLP,
- blake3 and keccak over the encoded block bytes,
- ECDSA recover throughput and per-tx vs per-batch recovery estimate,
- decode + records hash + one recover per sender + in-memory balance/nonce apply.

## Harness Changes

This branch also adds the devlog-80 follow-up instrumentation:

- `N42_POOL_AT_COMMIT` and `N42_POOL_AT_BUILD_START` logs with sampled txpool
  `pool_pending` / `pool_queued`.
- `n42_pool_pending_at_commit`, `n42_pool_queued_at_commit`,
  `n42_pool_pending_at_build_start`, `n42_pool_queued_at_build_start` gauges.
- `n42-stress --sync-ingest-mode per-node-continuous`, which reuses the
  continuous TCP ingest pacer and does not wait for a 30s pool-drain window.

One attempted 7-node run is discarded: I forgot to export
`N42_INJECT_PORT=19900`, so only validator 0 exposed `19900` and the other
ingest endpoints refused connections. That data is not used below.

## Commands

Build:

```bash
cargo build --release --bin n42-node --bin n42-stress --bin n42-mobile-sim
```

Batch transfer fast-lane runs:

```bash
target/release/n42-stress --batch-transfer-bench \
  --batch-transfer-senders 512 \
  --batch-transfer-per-sender 10000 \
  --batch-transfer-iters 3 \
  --batch-transfer-recover-samples 100000

target/release/n42-stress --batch-transfer-bench \
  --batch-transfer-senders 1024 \
  --batch-transfer-per-sender 10000 \
  --batch-transfer-iters 2 \
  --batch-transfer-recover-samples 100000

target/release/n42-stress --batch-transfer-bench \
  --batch-transfer-senders 512 \
  --batch-transfer-per-sender 50000 \
  --batch-transfer-iters 1 \
  --batch-transfer-recover-samples 100000
```

Logs:

- `/tmp/n42-batch-transfer-bench-512x10k.log`
- `/tmp/n42-batch-transfer-bench-1024x10k.log`
- `/tmp/n42-batch-transfer-bench-512x50k.log`

## Results

Baseline signed EIP-1559 transfer sample size: `117.6 bytes/tx`.

| Shape | Transfers | Encoded bytes | Bytes / transfer | Compression | blake3 / keccak block hash | Batch verify + hash + apply | CPU fast-lane TPS | 1Gbps wire cap | 10Gbps wire cap |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 512 x 10k | 5.12M | 61.48MB | 12.008 | 9.8x | 34.1ms / 110.3ms | 106.8ms | 48.0M | 10.4M | 104.1M |
| 1024 x 10k | 10.24M | 122.96MB | 12.008 | 9.8x | 69.4ms / 216.0ms | 212.5ms | 48.2M | 10.4M | 104.1M |
| 512 x 50k | 25.60M | 307.24MB | 12.002 | 9.8x | 176.2ms / 541.4ms | 400.2ms | 64.0M | 10.4M | 104.2M |

ECDSA recover sample throughput: about `18k recover/s`.

| Shape | Per-tx ecrecover estimate | Batch ecrecover estimate | Reduction |
| --- | ---: | ---: | ---: |
| 512 x 10k | 284.7s | 28.5ms | 10,000x |
| 1024 x 10k | 565.1s | 56.5ms | 10,000x |
| 512 x 50k | 1,413.4s | 28.3ms | 50,000x |

## Interpretation

The special-case batch lane completely changes the ceiling. In this synthetic
path, per-tx signature recovery is no longer a viable bottleneck because the
recover count is per sender batch, not per transfer. The hot path becomes record
hashing plus simple balance/nonce application.

For the 10k-batch shape, CPU-side decode + records hash + batch recover +
in-memory apply is roughly `48M transfers/s` on this machine. Larger 50k batches
amortize signatures further and reached about `64M transfers/s`.

The compressed representation makes network bandwidth a first-order bound:
at 12B/transfer, a raw 1Gbps link tops out around `10.4M transfers/s` before any
transport, consensus, or framing overhead. A 10Gbps link is about `104M
transfers/s`. So the "millions TPS" benchmark becomes plausible for this
special workload, but a real multi-node run will quickly become a bandwidth,
state persistence, and state-root problem rather than an EVM or ecrecover problem.

## Go / No-Go

Go for a prototype only if the goal is a deliberately narrow transfer fast lane:
registered recipient index + common fee semantics + one sender batch per block.
This is not a general Ethereum transaction throughput path.

Next prototype should be feature-gated and fresh-genesis only:

- define a consensus payload sidecar for `BatchTransferBlock`,
- execute it outside EVM against a simple account table,
- commit balances/nonces through the same state-root path,
- add fraud/validity checks for start nonce, sufficient balance, recipient table,
  and replay domain,
- run 7-node bandwidth/state-root tests with 5M-25M transfer blocks.

Claude bridge note: I attempted to send this direction to the bridge, but the
bridge was unreachable (`Cannot reach Codex Bridge at http://192.168.0.120:8788`).
This devlog is the durable handoff until the bridge comes back.
