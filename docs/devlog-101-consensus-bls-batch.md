# Devlog 101: Consensus BLS Vote Batching

Date: 2026-07-18

Task: `docs/codex-task-sync-from-gov5-2026H1.md`, P1-1

## Audit result

N42 already had a coefficient-randomized `blst` batch primitive and the mobile
StarHub production path already micro-batched receipts at 256 entries or 50 ms.
The latter was measured previously at roughly 24x on the original workstation.
Consensus R1 and R2 votes, however, were still verified one at a time. The
orchestrator verified each vote once to authenticate its peer and the state
machine verified the same signature again before recording equivocation
evidence.

The optimization threshold was met, so consensus batching was implemented.

## Implementation

- The high-priority consensus channel non-blockingly drains up to 256 already
  queued current-view R1/R2 votes. There is no batching timer and therefore no
  added vote latency when only one message is ready.
- The engine authenticates the drained votes with
  `verify_multiple_aggregate_signatures`. The existing primitive assigns
  unpredictable non-zero coefficients and, if the batch fails, verifies
  individually to identify only the bad entries.
- A private-field `AuthenticatedConsensusMessage` binds the authentication
  result to the exact owned message. The orchestrator can use its signer for
  validator-peer promotion, then hands the token back to the engine. The state
  machine records equivocation evidence and collects the vote without a second
  pairing.
- Invalid batch entries are dropped and counted. Unknown validators and
  non-vote messages stay on the existing verification path.
- `VoteCollector` and `TimeoutCollector` also batch any signatures not already
  authenticated by their caller. This preserves defense-in-depth and speeds
  offline/test callers that add raw votes.
- Metrics: `n42_consensus_vote_batch_size`,
  `n42_consensus_vote_batch_verify_ms`, and
  `n42_consensus_vote_batch_invalid_total`.

The opaque token and the existing verify-before-evidence ordering preserve the
S4 rule: an unauthenticated vote cannot poison an equivocation tracker or claim
a validator slot.

## Measurement

The task-book command was run first and passed all five ignored benchmarks. It
runs the tests concurrently, so a controlled single-test, single-thread repeat
was used for the before/after decision:

```text
cargo test -p n42-consensus --test performance_bench --release \
  bench_bls_operations -- --ignored --nocapture --test-threads=1
```

Both measurements use the P0 `n-f` quorum thresholds.

| Validators | Quorum | Before | After | Speedup |
|---:|---:|---:|---:|---:|
| 4 | 3 | 3.18 ms | 2.15 ms | 1.48x |
| 10 | 7 | 7.40 ms | 3.62 ms | 2.04x |
| 67 | 45 | 47.81 ms | 19.31 ms | 2.48x |
| 100 | 67 | 70.09 ms | 28.23 ms | 2.48x |
| 333 | 223 | 232.80 ms | 92.51 ms | 2.52x |
| 500 | 334 | 351.00 ms | 137.31 ms | 2.56x |

An independent 1,000-signature primitive run measured randomized batch verify
at 58.8 ms versus 736.7 ms individually (12.5x for verification alone). The QC
benchmark also includes signing and aggregation, which is why its end-to-end
gain is smaller.

## Verification

- Mixed-validity batch test isolates the wrong-key signature.
- Opaque authenticated votes form a QC without a second verification.
- Orchestrator batch test forms the QC from valid votes and drops the bad vote.
- Existing invalid-vote/TC fallback tests continue to cover exact bad-index
  handling and below-quorum rejection.
- `cargo check --all-targets`, strict clippy, workspace tests, and chaos E2E are
  run as the task closeout gate.

## 2026-07-19 rework audit

The first implementation authenticated a `CommitVote` batch against whatever
`validator_changes_hash` happened to be cached before the drained events were
dispatched. An earlier Proposal in that same drain can establish or change the
canonical domain. A signer-only authentication token could therefore suppress
the required individual recheck under a stale domain.

The token now records the exact authenticated vote kind and, for R2, the exact
validator-changes hash. The state machine skips individual verification only
while that domain is still canonical. A batch miss for R2 falls back to the
ordinary check after preceding events have populated the proposal cache. The
primitive also randomizes every coefficient (including index zero) and treats
an unmatched input tail as a total batch failure.

The authoritative task-book benchmark was rerun serially in release mode. All
five ignored tests passed in 910.53 seconds. Representative current values:

| Workload | Result |
|---|---:|
| Mobile sign / verify | 401.4 / 755.6 us |
| 250K mobile receipts | 288,794 ms |
| QC n=100, quorum=67 | 28.26 ms |
| QC n=500, quorum=334 | 141.54 ms |
| 500×500 per-node mobile work | 574 ms |
| 100×2500 per-node mobile work | 2,883 ms |

The task-book's “same-message hash-to-G2 reuse” is not exposed by the current
safe Rust `blst` API. It remains a measured follow-up, not an unsafe local FFI
shortcut. The randomized batch path already clears the required 2x threshold.
