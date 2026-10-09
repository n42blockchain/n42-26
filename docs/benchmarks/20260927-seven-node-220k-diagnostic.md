# Seven-node 220k-cap diagnostic (2026-09-27)

## Result and scope

The 220,000-transactions-per-block setting did **not** yield 220,000 TPS. The
first 60-second leg committed 1,817,344 successful transactions in 62.168 s
(29,232.98 TPS); the second committed 1,352,768 in 62.050 s (21,801.19 TPS).
Both passed the strict seven-validator H2-v4 audit with zero failed receipts,
matching committed hashes and transaction, receipt and state roots, no QMDB
read errors or mismatches, and a passing post-run common-block verification.
The 32 MiB by-hash recovery codec was separately tested (21/21 tests). These
runs establish correctness at this cap, not a throughput improvement.

This is a **cold-fleet diagnostic**, not the intended warmed baseline. The
controller started and stopped an independent fleet for each leg. Its first
leg therefore did not warm the nodes used by the second. The result must not
select a production optimization or support a 400k/600k claim.

## Reproducibility

- Campaign: `.artifacts/native-seven-220k-20260926/`, with preserved
  `result-cap220-warmup/`, `result-cap220-baseline/`, and their runtime logs.
- Claim: `.artifacts/native-seven-220k-run-claim-20260927.jsonl`; both
  directories were claimed, and the driver returned zero and released them.
- Release node SHA-256: `1626d24622ad748a33818654bacbd9ca858304d7836d8b4bc10c5beb19353d27`.
  Other binary hashes are in `.artifacts/native-seven-220k-build-sha256-20260927.txt`
  and each leg's `binary-workload-sha256.txt`.
- Presigned N42T v2 workload: chain 941007, 24,000,000 transactions,
  SHA-256 `559547e918c53ab75d5f13a8fb2a9ddbffc7afd81bd0a69efd77a34f6f92b4bb`.
  It passed a 13.2-million-transaction header-capacity preflight. The live
  auditor, not that preflight, verified committed receipts and chain state.
- Both legs used native H2-v4, QMDB-only reads, full transaction verification,
  220k block cap, execution reuse, `--parallel-build`, a bounded 512 MiB
  JSON-RPC response limit, and the same release binaries and signed file.
  The source checkout had unrelated uncommitted changes; binary hashes, not
  the branch name alone, identify the tested build.

| Metric | First cold leg | Second cold leg |
| --- | ---: | ---: |
| Successful committed TPS | 29,232.98 | 21,801.19 |
| Successful / failed transactions | 1,817,344 / 0 | 1,352,768 / 0 |
| Audited transaction-bearing blocks; unmatched hashes | 10; 0 | 8; 0 |
| Fifteen-second commit windows, transactions | 277,344 / 0 / 880,000 / 440,000 | 32,768 / 220,000 / 440,000 / 660,000 |
| Median build start → leader broadcast | 2,553 ms | 2,491 ms |
| Median leader normalized-payload validation | 912 ms | 898.5 ms |
| Median follower block-data → accepted | 1,384.5 ms | 1,402 ms |
| Median payload built → Engine receive | 389 ms | 386.5 ms |
| Median validation complete → compression complete | 429 ms | 426.5 ms |

The stage medians come only from transaction-bearing audited block hashes.
Some stages overlap; their medians must not be added into a synthetic critical
path. In the second leg, six full 220k blocks show a 2,560 ms median leader
build-to-broadcast and a 1,423 ms median follower import. Once the early
gaps pass, audited full-block commit intervals are about 4.9–5.0 s, or about
44k TPS at 220k transactions per block. The same leg has early 19.95 s and
14.79 s gaps. Thus the 60-second aggregate is heavily affected by startup
and pool/ingest settling. Stress sent 5,316,608 transactions with zero send
errors but reported repeated pool-credit waits; sent transactions are not
counted as committed throughput.

`n42_parallel_evm_blocks_total_delta` was zero. This counter is for the
standalone Block-STM path, whereas the measured `--parallel-build` selects a
different payload-builder path; zero alone is not proof that parallel build
was disabled. Block-STM was not inserted into canonical execution.

## Decision gate

**No-Go on selecting a protocol optimization from this run.** First make
warmup and scoring use the *same live fleet and one nonce-continuous signed
stream*. Capture an authenticated midpoint boundary after 60 unscored seconds,
then audit the following 60 seconds independently. Generate and preflight at
least 48 million chain-941007 signed transactions so supply cannot cap that
two-minute diagnostic. Keep both start/mid/end boundaries and audit warmup
and scored periods separately. Only if the warmed run passes strict receipt,
QC, root, hash, persistence and seven-node post-run gates should its
hash-joined timeline choose the next optimization.

If the warmed full-block interval remains near 5 s, the observed 2.5 s leader
build-to-broadcast span is the first candidate to instrument more finely;
follower import at about 1.4 s is second. In particular, separate the roughly
0.39 s payload-to-Engine gap and roughly 0.43 s validation-to-compression
completion gap from actual execution or compression work before changing
correctness-sensitive code.
