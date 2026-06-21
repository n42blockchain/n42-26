# Devlog 83: Batch Transfer Fast-Lane Profile and Network Optimization

Date: 2026-06-20
Branch: `feat/continuous-ingest-cadence`
Base: `1ccaf7c`

## Goal

Profile the compressed batch-transfer fast lane from devlog-82, identify the next bottleneck, apply the smallest benchmark-only optimization, and rerun the 7-node TPS ceiling.

This remains a benchmark fast lane. It bypasses reth execution, state roots, receipts, MDBX persistence, and production replay semantics. The result is an upper-bound measurement for the surrounding consensus/network path with 12-byte transfer records and one ECDSA signature per sender batch.

## Profiling Setup

CPU profiling used macOS `sample` because `pprof` was not installed in this environment. The profiling binary was built with symbols:

```bash
cargo build --profile profiling --bin n42-node --bin n42-mobile-sim
```

Baseline profile run:

- data dir: `/tmp/n42-batch-fastlane-profile-256x10k`
- shape: 256 senders x 10,000 transfers = 2.56M transfers/block
- encoded size: 30,019 KiB, 12.008 bytes/transfer
- CPU/RSS: `ps` sampled all validator PIDs for 90s
- CPU stack: `sample <validator-pid> 15 -file sample-v4.txt`

## Baseline Finding

The devlog-82 256x10k path was not limited by signature verification or 12-byte decoding. The visible issues were network/cadence:

- direct block push rejections: 71
- pacemaker timeouts: 11
- commit TPS: avg 366,886, p50 325,410, p95 552,677, max 1,096,360
- verify/apply: avg 458.5ms, p50 438ms, p95 787ms, max 1,801ms
- direct send: avg 20.4ms, p50 16ms, p95 45ms
- RSS peak: roughly 1.3-1.6GB per validator

Root causes:

1. `block_direct` accepted inbound large sidecars only after the sender was in `authenticated_peer_validator_map`. The configured deterministic validator PeerIds were already known, but large block push could race before auth promotion and get rejected.
2. Batch-transfer block data was sent both by direct request/response and by full GossipSub fallback. At 30-60MB/block this creates duplicate large buffers and extra network pressure.

## Optimization

Added two benchmark/testnet flags, both defaulting to production-compatible behavior:

```bash
N42_BLOCK_DIRECT_ACCEPT_EXPECTED=1
N42_BATCH_TRANSFER_GOSSIP_FALLBACK=0
```

Changes:

- `crates/n42-network/src/service.rs`
  - `block_direct` still prefers authenticated validator peers.
  - With `N42_BLOCK_DIRECT_ACCEPT_EXPECTED=1`, it also accepts peers matching configured expected validator PeerIds.
  - Default remains strict authenticated-only.

- `crates/n42-node/src/orchestrator/execution_bridge.rs`
  - batch-transfer broadcast can skip full GossipSub fallback.
  - Default still keeps fallback enabled.

Validation:

```bash
cargo fmt --check
cargo check -q -p n42-node
cargo build --profile profiling --bin n42-node --bin n42-mobile-sim
cargo build --release --bin n42-node --bin n42-mobile-sim
```

`cargo check` and both builds passed. Only existing reth unused warnings remained.

## Optimized 256x10k Run

Run:

- data dir: `/tmp/n42-batch-fastlane-opt-256x10k`
- binary: `target/profiling/n42-node`
- shape: 256 x 10k = 2.56M transfers/block
- encoded size: 30,019 KiB

Results:

- direct rejections: 0
- GossipSub fallback: skipped on 108 broadcasts
- pacemaker timeouts: 6
- commit samples: 107
- commit TPS: avg 3,267,444, p50 2,825,607, p95 6,918,919, max 11,277,533
- verify/apply: avg 393.5ms, p50 422ms, p95 573ms, max 1,016ms
- direct send: avg 24.4ms, p50 17ms, p95 58ms, max 133ms
- RSS avg: roughly 571-686MB per validator
- RSS peak: roughly 970-1,151MB per validator

Per-validator commit averages:

| Validator | Samples | Avg TPS | Max TPS |
|---|---:|---:|---:|
| v0 | 15 | 3,720,266 | 6,918,919 |
| v1 | 16 | 3,808,308 | 7,111,111 |
| v2 | 15 | 3,578,504 | 4,785,047 |
| v3 | 15 | 2,772,042 | 4,951,644 |
| v4 | 15 | 2,599,516 | 7,975,078 |
| v5 | 15 | 4,068,776 | 11,277,533 |
| v6 | 16 | 2,349,814 | 6,400,000 |

The 256x10k profile establishes the clean practical fast-lane result: multi-million TPS samples across all validators, with lower memory than the previous 30MB run because duplicate GossipSub propagation was removed.

## Optimized Release 512x10k Run

Run:

- data dir: `/tmp/n42-batch-fastlane-opt-release-512x10k`
- binary: `target/release/n42-node`
- shape: 512 x 10k = 5.12M transfers/block
- encoded size: 60,038 KiB

Results:

- direct rejections: 0
- GossipSub fallback: skipped on 51 broadcasts
- pacemaker timeouts: 5
- commit samples: 49
- commit TPS: avg 3,237,965, p50 3,038,576, p95 6,416,040, max 13,333,333
- verify/apply: avg 885.5ms, p50 980ms, p95 1,207ms, max 1,496ms
- direct send: avg 63.4ms, p50 38ms, p95 147ms, max 174ms
- RSS avg: roughly 876MB-1.03GB per validator
- RSS peak: roughly 1.50-1.88GB per validator

Per-validator commit averages:

| Validator | Samples | Avg TPS | Max TPS |
|---|---:|---:|---:|
| v0 | 7 | 4,173,931 | 13,333,333 |
| v1 | 8 | 2,539,917 | 4,252,492 |
| v2 | 6 | 1,897,244 | 3,160,494 |
| v3 | 7 | 3,912,569 | 7,121,001 |
| v4 | 7 | 3,962,997 | 6,440,252 |
| v5 | 7 | 2,995,567 | 5,452,609 |
| v6 | 7 | 3,091,720 | 4,115,756 |

512x10k is now usable for peak exploration. It is still heavier than 256x10k: follower verify/apply approaches 1s p50 and RSS peak reaches 1.5-1.9GB. The best practical sustained setting remains 256x10k; the 512x10k setting is the higher peak setting.

## CPU Profile

`sample` profile:

```text
/tmp/n42-batch-fastlane-opt-256x10k/sample-v4.txt
```

Dominant sampled stacks:

- QUIC send path:
  - `quinn_udp::imp::send`
  - `__sendmsg`
  - `ring_core_0_17_14__chacha20_poly1305_seal/open`
- batch verification:
  - `n42_node::batch_transfer::verify_apply_block`
  - `blake3_hash_many_neon`
  - `rustsecp256k1_v0_11_ecdsa_recover`
- transfer application loops in `verify_apply_block`

Interpretation:

- ECDSA is no longer the dominant limiter. One recovery per sender keeps it bounded, though 512 senders still costs about 125ms average in the 512x10k run.
- The next visible ceiling is large-sidecar networking plus follower-side hashing/apply.
- GossipSub fallback removal materially reduced memory pressure and eliminated direct-push auth rejections.

## Answer

Yes, this compressed batch-transfer idea can exceed the historical Mac record by a large margin in the benchmark fast lane:

- historical reference: about 43-45K single-block peak and about 14K sustained;
- cleaned 256x10k run: avg 3.27M TPS, p50 2.83M, p95 6.92M, max 11.28M;
- cleaned 512x10k release run: avg 3.24M TPS, p50 3.04M, p95 6.42M, max 13.33M.

The result is not a production-chain TPS number because the path skips state root, receipts, persistence, and EVM/reth execution. It is a valid upper-bound measurement for the proposed 12-byte transfer format plus one-signature-per-sender batch verification over the existing 7-node consensus/network shell.

## Next Optimization Plan

The next high-leverage work is no longer transaction encoding. It is sidecar transport and follower parallelism:

1. Replace request/response full-buffer sends with a streaming or chunked direct sidecar path to reduce `sendmsg`/QUIC tail latency and large `Vec` cloning.
2. Keep GossipSub as metadata/hash announcement only for this fast lane; bulk bytes should stay on direct transfer.
3. Parallelize `verify_apply_block` by sender shard:
   - block-level hash can remain one pass;
   - per-sender prehash/recover and transfer application can split across workers;
   - expected gain is strongest at 512x10k, where verify/apply p50 is already about 980ms.
4. Add adaptive timeout based on encoded sidecar size, so 60MB test blocks do not trigger avoidable view changes.
5. If this becomes production work, define real state-root, persistence, replay, receipt, and fraud/audit semantics before treating the number as chain TPS.
