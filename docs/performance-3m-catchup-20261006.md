# 3M TPS catch-up audit — 2026-10-06

The earlier 7,372 TPS result is a short local RPC benchmark, not the repository's historical throughput ceiling. It must not be compared directly with either the Linux fleet records or native attested-frame execution.

## Verified source and measurement differences

| Measurement | Architecture and scope | Result |
| --- | --- | --- |
| n42-26 round 53 | Seven execution layers, EPYC 128 cores / 256 threads, binary ingest, 163,000 transactions per block; benchmark verification and persistence settings | 170,546.56 TPS |
| n42-26 local transfer-repeat | M1 Max, seven execution layers, ordinary signed RPC transactions, 22,857 transaction cap, configured 1-second slots | 7,371.61 common-canonical TPS over 64.42 seconds |
| Historical batch-transfer fast lane | Specialized batch benchmark bypassing EVM execution, canonical state roots, receipts and MDBX persistence | 3.24–3.27M average; not canonical chain execution |
| n42-rs native shared execution, loop342P | Seven validator keys sharing one execution layer; attested Ed25519 frames and parallel deferred execution | Three 30-second canonical windows: 3,093,333 / 3,126,667 / 3,080,000 TPS |

Sources: `performance-records.md`, `devlog-141-one-minute-rerun-20260901.md`, `devlog-82-batch-transfer-fastlane-7node.md`, `devlog-83-batch-transfer-profile-optimize.md`, sibling `n42-rs/docs/SHARED_EXECUTION_SCOPE.md`, and **`n42-rs/scripts/fleet7-runs/results/loop342.out`**. The raw loop342 result supersedes the earlier 2.6M documentation baseline. Loop342Db also records 3.087M / 2.973M / 3.033M TPS and persistence counts of 458/463, 446/446 and 456/455 produced blocks in the respective windows. These are recorded Linux results, not results reproduced on this Mac. They prove a shared-layer 3M benchmark, not seven independent execution layers or an unrestricted production security configuration.

## Branch and protocol audit

* Current n42-26 performance branch starts from origin/main `007ffcf7b861b28c91d9583d6cdb5ea9de8e6a88`. Its actual state-root implementation is `Gov5QmdbStateRootStore` / `QmdbLeafTree`.
* Native reference is frozen at `n42-rs/feat/native-fleet7` commit `09a1284e4`, 241 commits ahead of its main. Another session's uncommitted edits in the sibling repository are preserved.
* Raw loop342 provenance names `a34faf0b0+tree`, an ancestor 13 commits behind the frozen reference. Its recorded 3M result is attributed to that build, rather than silently assigned to the current binary.
* The old batch-transfer implementation at `8e1a077` is not an ancestor of current n42-26 main. Its benchmark documents survive on main while its implementation does not. This is a real branch mismatch.
* Native reference uses Reth v2.7.0 plus local patches; n42-26 uses a v2.4.1 integration. Native frame ingestion, parallel output shards, early sealing, deferred field computation and shared import deduplication are not equivalent to the current n42-26 payload path. A blind merge would change protocol and execution contracts.
* At the previous benchmark's 22,857-transaction cap and one-second interval, the configured ceiling is 22,857 TPS before overhead. Reaching 3M with that cap would require a 7.62 ms interval. These parameters cannot test a 3M claim.

## Implemented sync and validation

The sender-recovery cache is now shared by the custom N42 payload converter and node configuration, following the native/Reth integration. Cache misses still perform signature recovery and invalid signatures still fail. Restored-slot tracking remains enabled. Execution library tests: 98 passed, including cache reuse across cloned configurations and invalid-signature rejection with and without a cache. The optimized profiling node and stress binaries built successfully. Runtime activation requires `--engine.sender-recovery-cache`; the Reth default is disabled. This version wires that cache into network transaction handling, but the current RPC benchmark does not demonstrate that RPC admission populates it.

QMDB binary-tree batching changes and eager-tree equivalence tests remain part of this working change. Their microbenchmark gains are not presented as fleet TPS gains.

## Local acceptance scope

Both requested layouts are reported separately: **seven validators / one shared execution layer (E1)** and **seven validators / seven independent execution layers (E7)**. They will use the same frozen native source, genesis and transaction dataset on this machine. E1 has one execution failure domain. A consensus seal is not counted as completed canonical execution; roots and canonical transaction counts must also be checked.

Preliminary local checks, while other sessions and the native linker used CPU:

| Run | Canonical window TPS | Final common transaction count | Signer-recovery inclusive CPU share |
| --- | ---: | ---: | ---: |
| Updated converter, cache disabled | 5,675.81 | 638,000 | 39.15% |
| Updated converter, cache explicitly enabled | 5,450.72 | 560,469 | 38.30% |

All sampled commitments matched, and all fourteen node exits were zero. These are **contended short runs**, not a controlled performance A/B against the earlier idle 7,371.61 TPS run. Neither the throughput nor the CPU shares establish a cache speedup. Raw evidence is under `.artifacts/performance-20261006-141644/{catchup-cache,catchup-cache-enabled}`; each contains the new CPU-weighted flame graphs. Cache activation alone has not removed the recovery bottleneck from this RPC workload.

The isolated native optimized binaries built successfully. The sibling advanced during the audit to `beef94990` with five additional audit/platform commits; this campaign keeps `09a1284e4` frozen so its two layouts have the same source. The sibling's subsequent Cargo/mobile edits remain untouched.

## Native local comparison

| Layout | Shared profile and dataset | Common canonical transactions / wall window | TPS |
| --- | --- | --- | ---: |
| E1: seven validators, one execution layer | loop342P pipeline, 2 execution threads, 163k block cap, 100ms pacing, 300k offered rate, 6.4M replay dataset | 3,963,500 / 15.328s | 258,574.38 |
| E7: seven validators, seven execution layers | Same pipeline, per-layer thread budget, genesis and replay dataset | 1,040,500 / 16.475s | 63,158.03 |

Evidence: `.artifacts/performance-20261006-141644/native-e1-reference-profile/summary.json` and `native-e7-reference-r3/summary.json`. Every layout passed common validator commit, canonical commitment and endpoint receipt-count checks; three endpoint receipts were successful with matching transaction/block hashes. E7 recorded two recoverable compact-body refusals and no invalid payload, header-variant or pre-execution errors. These are short, contended local windows, not sustained capacities. The 300k offered rate also bounds what the E1 comparison can show. The native trusted gateway attests Ed25519 transaction frames; it is a different authentication contract from ordinary ECDSA RPC admission.

An earlier fallback-enabled E1 window delivered 313,943.12 TPS, but its configuration differs and it is not the comparison baseline. Failed startup, wrong import-route and unavailable-read-view runs remain in the artifact directory and are not credited as capacity results.

The driver now imports the complete `C/R/D/A2` + `legf P` configuration from the frozen runner, derives ingress high water from pool capacity, starts followers before the leader, and counts the latest **readable common commit**, rather than the latest uncommitted EL head. Transaction-hash lookup is pruned in this benchmark, so execution receipts are read by block after the measured window.

### Measured bottlenecks

Native window timing evidence: `native-window-timings.json`. Windows contain varying block sizes and background CPU competition; milliseconds are not normalized per transaction.

* E1 with two execution threads: parallel execution p50/p95 423/547ms; roots 160/282ms; seal 556/716ms. This configuration cannot exercise the Linux pipeline's roughly 60ms full-block cycle.
* E7 follower import: execution 314/1,004ms, roots 151/571ms, total 784/1,999ms, parent-field waiting p95 481ms. Sender recovery p50/p95 is zero on this attested-frame path. Increasing signature cache capacity is therefore not the main native E7 fix.
* Short-tenure experiments exposed QMDB read-view/compaction and missing-published-output stalls. These require state-version and persistence validation before interpreting a stalled fleet as an execution ceiling.
* A late follower can miss block 1 and hold all subsequent bodies while its EL remains at genesis. Starting followers first makes the benchmark reproducible; automatic recovery of the missing parent remains a production follow-up.

Completed capacity probes used the 32M replay dataset, eight execution threads, 200k block cap and 60ms pacing:

| Probe | Canonical transactions | TPS | Measurement limitation |
| --- | ---: | ---: | --- |
| `native-e1-capacity`, 1M offered TPS | 13,410,500 | 536,631.41 | Full transaction-hash RPC reads stretched the intended 20-second window to about 25 seconds; preliminary only |
| `native-e1-capacity-log-count`, 1.5M offered TPS | 4,431,000 | 294,886.27 | 15-second window counts canonical logs and common validator commits; RPC count/root/receipt verification runs afterward |

The latter passed post-window canonical count, common commit and receipt checks, with zero detected import errors or tracked QMDB warnings. Its producer delivered roughly 285–330k TPS despite 1.5M requested TPS. This exposes an ingress/backpressure bottleneck, but does not by itself distinguish producer, admission, execution and persistence causes. CPU-by-role is unavailable in this log-count probe. The substantial variation prevents crediting either probe as sustained capacity or attributing a gain to thread count. The 3M target has not been achieved or verified locally; the native pipeline has not yet been fully ported into n42-26.

## Architecture catch-up sequence

1. Sender-cache probes completed, but CPU contention and unproven RPC cache warming prevent crediting a gain. Establish a controlled cache-hit/miss comparison before further cache tuning. Keep these legacy RPC results separate from native frame results.
2. Native E1 and E7 are established on this Mac with the frozen commit and the same 6.4M-transaction replay dataset. Repeat longer windows while recording producer/admission backlog and host CPU competition. Report offered rate, canonical counts, validator commits and execution-layer roots. Increase offered load only when the dataset covers the full window; an offered-rate ceiling is not execution capacity.
3. Port transaction envelope / frame authentication and the binary ingest boundary together. Current Ethereum-only primitives cannot simply decode native 0x50 transactions. Preserve ordinary RPC transaction handling and rejection of malformed or unauthenticated frames.
4. Adapt the validated transfer executor and output shards to the current QMDB state interface. Keep arbitrary-contract EVM fallback, nonce, balance, gas and receipt semantics; validate against ordinary execution before enabling the path by default.
5. Adapt early seal / parent-field completion and import deduplication with the H2 header protocol. This changes when fields become final and requires explicit cross-client validation; setting environment flags alone does not implement it.
6. Check persistence and bounded backlog over longer runs. The reference's loop342 window-1 example used 35.8 execution cores plus 1.2 validator cores; its 3M result cannot be projected to a 10-core Mac by comparing TPS alone. E7 adds seven independent executions and must keep its own target and measurement.

The native macOS port changes only optional Linux affinity/priority, stack diagnostics and unsupported resource/socket timestamp counters. Isolated tests of the actual `road_runtime.rs` passed all five cases, including preservation of the first byte, remaining bytes and EOF. Unsupported TCP timestamps return `None`; they are not recorded as measured zero delay.
