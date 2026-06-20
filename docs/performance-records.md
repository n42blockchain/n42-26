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

## Interpretation Rules

- Use production-like rows when discussing normal N42 chain throughput.
- Use benchmark-only rows when discussing the theoretical ceiling of a specialized compressed transfer lane.
- Do not quote batch fast-lane numbers as Ethereum transaction TPS until state root, persistence, replay, receipts, and production validity semantics are defined and measured.
