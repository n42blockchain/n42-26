# Devlog 79: High-TPS Harness Sustained Recheck

Date: 2026-06-19

Base: `cb2ca86` (`main`, blake3/binary roots prototype merged)

Branch: `feat/high-tps-harness-sustained`

## Summary

This pass fixed the stress harness knobs needed to avoid the devlog-78 global-wave split:

- `n42-stress --sync-ingest-mode per-node` now forces each ingest endpoint to submit a full `--wave` cap independently.
- `--wave-drain-pending` controls the refill threshold instead of hard-coding `<500`.
- `--wave-per-endpoint-cap` lets global mode override the historical `wave / endpoint_count` split for diagnostics.
- continuous TCP credit-probe senders now stop when the duration guard fires instead of waiting indefinitely for the next credit reply.
- `N42_INGEST_VERIFY_SENDER=1` adds a temporary admission probe that measures the ECDSA recovery cost while preserving the same TCP binary path.

The harness fix does make 90k leader blocks reproducible on the 7-node Mac setup. It does not restore the old 43-45k sustained ceiling. The best duration-window run here reached 11.6k TPS, close to the older 13.5-14.8k Mac sustained range but far below the historical 43,469 single-block peak. The remaining limiter is not transaction propagation send time; it is big-block cadence/import/commit backpressure after the leader can already build full 90k blocks.

## Build

```bash
cargo build --release --bin n42-node --bin n42-stress --bin n42-mobile-sim
```

Result: pass. Only existing upstream/reth warnings were emitted.

## Harness Changes

### Synchronized ingest

Before this change, `--wave 90000` with TX Forward enabled used global mode and split the wave across 7 endpoints, so each endpoint only sent about 12.8k tx. That reproduces devlog-78's low-block-size artifact.

New CLI:

```bash
--sync-ingest-mode auto|global|per-node
--wave-drain-pending <pending>
--wave-per-endpoint-cap <txs>
```

`auto` preserves historical behavior. `per-node` explicitly runs one independent wave loop per endpoint, with `--wave` as the per-node cap.

### Admission probe

`N42_INGEST_VERIFY_SENDER=1` makes TCP ingest decode each signed transaction, recover the sender, and compare it to the pre-signed sender embedded in the binary file. The default remains trusted sender (`Recovered::new_unchecked`), so production/test default behavior is unchanged.

The new `N42_INGEST_ADMISSION` log reports:

```text
verify_presigned_sender num_txs consumed accepted decode_us recover_us pool_insert_us
```

## Test Setup

Common 7-node setup:

```bash
N42_TWIG=1
N42_FAST_PROPOSE=1
N42_MIN_PROPOSE_DELAY_MS=0
N42_DEFER_STATE_ROOT=1
N42_SKIP_TX_VERIFY=1
N42_MAX_TXS_PER_BLOCK=90000
N42_GAS_LIMIT=2000000000
./scripts/testnet.sh --nodes 7 --clean --no-explorer --no-tx-gen --no-monitor --no-mobile-sim --block-interval 2000
```

Pre-sign file:

```text
/tmp/n42-devlog79-5m-7rpc.bin
5,000,000 simple transfers, 7 RPC partitions, 666 MB
presign rate: 187,816 tx/s
```

## Results

Duration-window TPS means tx included in non-empty blocks whose timestamps fall inside the stress run's declared duration window. The "all drain" TPS includes residual blocks after the client duration fired, so it is a conservative end-to-end drain figure.

| run | config | duration-window tx / TPS | all-drain TPS | max block | full-block density | main observation |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| devlog-78 reference | global wave split | ~7.5k TPS | n/a | 12.8k-25.7k | no 90k | harness split artifact |
| per-node strict drain | `--wave 90000 --sync-ingest-mode per-node --wave-drain-pending 500` | 1,042,704 / 11,586 | 6,715 | 90,000 | 2x 90k in window, 5x all-run | fixes split, but drain p50 28.3s creates empty windows |
| per-node overlap | `--wave-drain-pending 45000`, ingest high-water 180k | 139,000 / 1,158 | 1,492 | 90,000 | 1x 90k in window | bad: pools pile to 180k, TCP/write path stalls |
| continuous credit, trusted sender | `N42_INGEST_CREDIT_PROBE=1`, 70k/82k/90k pacer | 1,284,596 / 10,705 | 7,274 | 90,000 | 6x 90k in window, 7x all-run | best sustained full-block harness; still cadence-limited |
| continuous credit, verify sender | same, `N42_INGEST_VERIFY_SENDER=1`, 60s | 751,684 / 12,528 | 7,290 | 90,000 | 4x 90k in window, 6x all-run | recover is costly but not the only limiter |

Short-window monitor peaks:

| run | monitor avg | monitor max |
| --- | ---: | ---: |
| continuous credit, trusted | 23,636 TPS | 37,306 TPS |
| continuous credit, verify sender | 19,812 TPS | 33,892 TPS |

Comparison to historical Mac baselines:

| baseline | value | devlog-79 comparison |
| --- | ---: | --- |
| historical sustained, devlog-36 | 13,535 TPS | best duration-window run is 86% |
| historical sustained, devlog-34/35 | 14,820 TPS | best duration-window run is 78% |
| historical single-block peak, devlog-35 | 43,469 TPS (86,939 tx block) | credit monitor peak reached 86%, but sustained remains far below |

## Payload Build

Trusted continuous credit run:

| metric | p50 | p95 | max |
| --- | ---: | ---: | ---: |
| tx/block | 89,000 | 90,000 | 90,000 |
| `packing_ms` | 141 | 1,058 | 1,118 |
| `evm_exec_ms` | 97 | 813 | 868 |
| `pool_overhead_ms` | 43 | 361 | 380 |

The low p50 confirms the fast simple-transfer path is intact once the cache/pool is warm. The tail still reaches about 1s in large blocks, and blocks then arrive with long gaps. That is why full-block peak does not convert into 40k sustained TPS on this Mac run.

## Propagation Probe

For 85k-90k blocks:

| metric | p50 | p95 | max |
| --- | ---: | ---: | ---: |
| compact/direct payload size | 7.3 MB | 7.45 MB | 7.45 MB |
| `N42_DIRECT_PUSH send_ms` | 3 ms | 5 ms | 7 ms |
| `N42_BROADCAST total_broadcast_ms` | 3 ms | 5 ms | 8 ms |
| follower `ready_to_accept_ms` | 56 ms | 1,299 ms | 3,053 ms |
| follower `np_elapsed` | 1 ms | 54 ms | 1,651 ms |

Conclusion: leader send/broadcast is not the limiter on this machine. The tail is follower reconstruction/import/commit after receipt, not raw direct-push network time. Replicated mempool/range blocks would not be the first optimization to build from these numbers.

## Admission Probe

Trusted sender path:

| metric per 500 tx batch | p50 | p95 | max |
| --- | ---: | ---: | ---: |
| `decode_us` | 0 | 62 | 60,765 |
| `recover_us` | 0 | 0 | 0 |
| `pool_insert_us` | 379 | 1,016 | 119,479 |

Verify sender path:

| metric per 500 tx batch | p50 | p95 | max |
| --- | ---: | ---: | ---: |
| `decode_us` | 4 | 108 | 86,888 |
| `recover_us` | 17,877 | 199,442 | 612,334 |
| `pool_insert_us` | 373 | 1,102 | 315,369 |

ECDSA recovery is real cost: p50 is about 35.8 us/tx, p95 about 399 us/tx. Turning recovery on reduced the monitor average from 23.6k to 19.8k TPS and reduced p50 tx/block from 89k to 61k in this short run.

This supports a possible BLS/batch-verify fast lane as a future improvement, but it does not by itself explain the sustained ceiling. The trusted-sender run still had long credit waits and full-block cadence gaps.

## Go / No-Go

Go:

- Keep the harness changes. `--sync-ingest-mode per-node` and credit-probe continuous ingest are the correct tools for future 90k Mac tests.
- Keep `N42_INGEST_VERIFY_SENDER=1` as a benchmark-only probe for admission CPU.
- Optimize big-block cadence/import/commit before building a larger mempool architecture.

No-go for now:

- Do not start the replicated-log mempool/range-block project based on this data. Compact/direct payload send is only a few milliseconds even at about 7.45 MB.
- Do not treat BLS-tx as the first bottleneck fix. ECDSA recovery is expensive, but the trusted path still fails to sustain 40k because full 90k blocks arrive with long gaps.

Next likely useful measurements:

1. Split follower `ready_to_accept_ms` into decode, compact-pool match, import, state-root/commit, and FCU/notify.
2. Add block-to-block cadence logging around leader selection, proposal build start, payload ready, commit, and next view start.
3. Re-run continuous credit after reducing import/commit tails; target is not just max tx/block=90k, but 90k every about 2s.
