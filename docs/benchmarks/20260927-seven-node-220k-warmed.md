# Seven-node 220k-cap warmed diagnostic (2026-09-27)

## Accepted run

The corrected controller kept one seven-validator fleet alive across a 60-second
unscored load period and the scored period. It used one nonce-continuous
150-second signed stream, then captured an authenticated start/end pair around
the scored 60-second window. The complete 150-second run passed the strict
H2-v4 audit at 36,521.28 successful committed TPS. The separate score-window
audit passed at 37,866.15 TPS: 2,420,000 successful transactions, zero failed
transactions, matching QC and roots across all seven validators, zero QMDB
read errors/mismatches, and 100% matched audited transaction-bearing hashes.
The post-run verification passed on 100 common blocks.

The score-only timeline first exposed a clock-domain mismatch: H2 snapshots use
the host monotonic clock while node logs use Unix time. The boundary mapper now
uses the same-boot monotonic-to-wall offset. The corrected four 15-second
windows contain 440,000 / 660,000 / 660,000 / 660,000 successful committed
transactions. The new conversion test and timeline tests pass 4/4.

## Workload and evidence

- Campaign: `.artifacts/native-seven-220k-warmed-20260927/`.
- Score audit: `result-cap220-warmed/qualification/h2-score-audit.json`.
- Corrected score timeline: `result-cap220-warmed/timeline-score.json`.
- 48-million presigned chain-941007 transactions, SHA-256
  `fae3d545c62850d57cb436d5200cf3fe2785e49032c7fd5b5e08b299cee2832a`.
- Release `n42-node` SHA-256:
  `1626d24622ad748a33818654bacbd9ca858304d7836d8b4bc10c5beb19353d27`.
- Workload options: native H2-v4, seven validators, QMDB-only reads, full
  signature/receipt/root checks, 220k transaction cap, builder execution
  reuse, parallel transfer-prefix builder enabled, parallel import disabled.

The corrected scored interval has eleven full 220k blocks with commit gaps of
5.02–5.26 seconds. Transaction-bearing block medians are 2,899 ms from build
start to leader broadcast, 940 ms for leader `newPayload(Valid)` validation,
410 ms from payload build completion to Engine receipt, 429 ms from validation
completion to compression completion, and 1,431 ms for follower import. These
durations overlap and must not be summed. The parallel transfer prefix executed
all 220,000 transfers in about 166 ms at the builder. Thus builder transfer
execution alone does not explain the 5-second commit cycle.

## Next experiment

The first causal comparison is a warmed, same-binary A/B/A of follower import:

| Leg | Payload builder | Follower import |
| --- | --- | --- |
| A1 | parallel transfer prefix | serial |
| B | parallel transfer prefix | parallel transfer batch |
| A2 | parallel transfer prefix | serial |

Each leg must use a fresh immutable seven-node fleet, a 60-second warmup and a
separate 60-second scored window. Compare successful committed TPS, per-window
rates, full-block commit gaps, leader validation, follower import and all
correctness gates. Retain the change only if B improves throughput beyond
A-bookend drift and every receipt, root, hash, QC and restart check passes.
If this result does not explain the remaining gap to 400k TPS, add finer
timestamps around payload retrieval, Engine validation, consensus release and
commit before selecting another change.
