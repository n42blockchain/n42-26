# Devlog 100: gov5 2026H1 P0 Ordered Validation

Date: 2026-07-18

Task: `docs/codex-task-sync-from-gov5-2026H1.md`, S1-S5 closeout

## Outcome

The five P0 consensus hardening tasks were audited and implemented as separate,
reviewable commits, then combined in the required S1 -> S2 -> S3 -> S4 -> S5
order for workspace and live-network validation:

| Task | Ordered commit | Result |
|---|---|---|
| S1 quorum `n-f` | `cecd5c3` | QC/CommitQC/TC and validator-set overlap use the active set's safe threshold; protocol v4 |
| S2 proof-gated view progression | `d4b1690` | QC successor-only jumps, quorum TC/NewView progression, stale-timer and restart guards |
| S3 LockedQC parent | `4f7fd2d` | leader build and asynchronous payload completion are bound to the locked view and parent |
| S4 equivocation guards | `cb9ad52` | verified, arrival-order-independent proposal/R1/R2 evidence and crash-safe double-vote guards |
| S5 deterministic bad-block cache | `929f353` | invalid-payload LRU across ingress/import paths; transient errors cannot poison it |

The commits above are the cherry-pick identities on the ordered validation
branch. Each task also has an independent branch and devlog.

## Aggregate correctness E2E

The exact task-book suite was run from a release build with Scenario 4's
correctness profile:

```text
E2E_SCENARIO_FILTER=1,3,4,5,8,12 \
E2E_SCENARIO4_PROFILE=correctness \
RUST_LOG=info target/release/e2e-test --binary target/release/n42-node
```

All six scenarios passed:

- Scenario 1: height 101, average block time 3.97 seconds, balances correct.
- Scenario 3: 300/300 ERC-20 transfers, total supply and balances conserved.
- Scenario 4: one node reached 14; three nodes reached 13/13/13; five nodes
  reached 16/16/16/16/16. V1-V5, sampled hashes, and leader fairness passed.
- Scenario 5: 15 mobile clients, 235 accepted responses, zero rejects/errors;
  all validators produced QCs.
- Scenario 8: real QUIC path replayed two blocks, included the transaction,
  produced two BLS receipts, and exercised the code cache.
- Scenario 12: 17 RPC checks passed with zero warnings or failures.

## Recovery audit and false-green correction

The combined quorum and view-gate rules uncovered a real three-validator
reconnect deadlock. A restarted next-view leader could miss a timeout vote, and
byte-identical repeat timeout publications were suppressed by GossipSub. Two
delivery defenses were added: direct peer delivery for Timeout/NewView control
traffic, and verified timeout relay to the next-view leader before duplicate
collector rejection.

Scenario 9's old V6 accepted a restarted process even if no block was produced
after recovery. The assertion now requires post-recovery chain progress. With
that strict check, the unpatched run failed 6/7 at height 57; the fixed run
passed 7/7, with node 2 restarting at height 57 and all nodes reaching height
107. Fifty blocks committed after recovery, 387 transactions were processed,
and 10 sampled block hashes matched.

## Verification

- `cargo check --all-targets`: passed
- `cargo clippy --all-targets -- -D warnings`: passed
- `cargo test --workspace`: passed
- `cargo test -p n42-consensus --test chaos_7node`: 12 passed
- Release build: `n42-node-bin` and `e2e-test` passed
- Required aggregate E2E: scenarios 1, 3, 4, 5, 8, and 12 passed 6/6
- Strict crash/recovery E2E: Scenario 9 passed 7/7

The P0 closeout preserves the required safety preference: deterministic invalid
execution stops progress on that branch; transient local/network failure
remains recoverable; and no optimistic-voting or QC rollback semantics were
introduced.
