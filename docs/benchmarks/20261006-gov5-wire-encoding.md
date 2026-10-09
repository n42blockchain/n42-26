# Gov5 large-block wire encoding candidate — 2026-10-06

## Measured motivation

The existing October 1 audited 220,000-transaction baseline records median
leader build-to-broadcast of 2,734.5 ms, leader validation of 940.5 ms, and
follower import of 1,454 ms. The encoder runs after leader validation and before
the reliable Gov5 broadcast. Those stages overlap other work, so their durations
must not be added to predict a block cycle.

The current n42-rs reference uses different transaction commitments, workloads,
and acceptance counters. Its historical canonical-log TPS is not a successful
receipt TPS target. This change addresses one concrete cost in N42's critical
path; it does not claim to close the full throughput gap.

## Candidate and invariants

`encode_gov5_block_rlp` previously decoded every transaction, built a trie from
the raw payload, built another trie from the decoded transactions, then encoded
every transaction into a separately allocated buffer for the outgoing list.

The candidate reconstructs the header using the raw payload body once. It
validates each transaction with exact EIP-2718 decoding and compares its canonical
encoding against the original bytes using one reusable scratch buffer. It then
encodes the original transaction-byte list into a pre-sized output vector.

The transaction trie is still derived from all raw transactions and checked
through the authenticated block-header hash. Per-transaction byte equality
establishes that this same trie also commits to the canonical decoded
transactions. Header profile, native rewards, mobile-registry commitment, and
exact native header/hash checks remain in the reconstruction path. No changes
are made to execution, signature validation, H2/QC, receipts, QMDB roots,
persistence, or proposal-release ordering.

`N42_LEADER_GOV5_ENCODE` and `n42_gov5_block_encode_ms` now isolate this stage.
The seven-node timeline tool reports its median, p90, and missing observations;
older logs have missing observations rather than inferred zero durations.

## Validation artifacts

Artifacts are under `.artifacts/gov5-wire-20261006/`:

- `gov5_block.before.rs`, `candidate.patch`, and `manifest.json`: baseline,
  isolated candidate diff, exact source/lock/benchmark/claim-driver hashes.
- `benchmark.rs`: a 220,000-transaction native-header fixture, alternating
  baseline/candidate order over seven rounds. Both encoders must emit exactly
  identical bytes; the candidate's wire output must decode to the same block
  hash and transaction count. Both must reject mutated header and transaction
  inputs. Transactions are synthetic; no successful execution TPS is measured.
- `run.sh` and `claim.jsonl`: foreground preparation/measurement supervisor,
  using the shared dual-directory hardware claim protocol.

The controller failed to grant a quiet window over 19 checks and
565.8 seconds of waiting. Other builds/tests were observed and
the highest sampled one-minute load was 119.38. The pending
driver was cancelled cleanly, with no build, test, benchmark, or fleet started.
`status.json` records this gate failure. Only Rust formatting/whitespace and
Python syntax checks completed; compilation and behavioral validation remain
unverified. No performance gain or seven-node TPS result is claimed. Retaining this candidate as an end-to-end
optimization requires a same-source, single-factor seven-node A/B/A with complete
successful receipt, H2/QC, QMDB-root, persistence, and restart checks.
