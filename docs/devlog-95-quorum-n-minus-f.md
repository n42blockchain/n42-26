# devlog-95: Consensus quorum hardened to n-f

Date: 2026-07-18  
Scope: gov5 2026H1 sync task S1 (consensus safety)

## Background

The consensus quorum was derived as `2f+1`. That is safe only when the active
validator count is exactly `3f+1`. N42 supports commit-then-activate validator
set changes, so valid epochs can contain other counts. For example, a 5-node
set with `f=1` previously accepted 3 signatures; two such certificates can
intersect only in the Byzantine validator.

## Change

- `ValidatorSet::quorum_size()` is now the authoritative `n-f` calculation,
  where `n` is the active set length and `f` is the validated fault-tolerance
  parameter stored with that set.
- `ConsensusConfig::quorum_size()` uses the same formula for the static initial
  set. Production QC, CommitQC and TC collection/verification already route
  through the epoch-aware `ValidatorSet`, so there is no second threshold.
- The leader startup gate continues to require `quorum_size - 1` connected
  validator peers and therefore automatically follows the active set.
- Dynamic reconfiguration now requires old/new address overlap of at least the
  old active set's `n-f` quorum. A 5-node transition with only 3 old validators
  is rejected; 4 is accepted.
- Historical QC verification remains keyed by the QC view's historical
  `ValidatorSet`. A regression test advances from 5 validators to 4 and proves
  that a view-5 QC still requires and verifies 4 old-set signatures even though
  the current set quorum is 3.
- The mobile receipt `2/3` sampling threshold is intentionally unchanged. It is
  a non-consensus attestation statistic and cannot authorize a QC or TC; the
  distinction is now explicit at its calculation site.
- A full Scenario 4 run exposed a pre-existing fresh-join catch-up regression:
  a validator could learn a descendant CommitQC before executing its ancestors,
  then incorrectly use that learned commit view as its sync floor. Catch-up now
  starts strictly from the execution-validated view. Restart recovery remains
  safe because startup seeds that view only from a view/hash pair proven against
  reth's canonical head. Opposing regressions cover both fresh join and restart.

Expected thresholds:

| n | f | old `2f+1` | new `n-f` |
|---:|---:|---:|---:|
| 4 | 1 | 3 | 3 |
| 5 | 1 | 3 | 4 |
| 7 | 2 | 5 | 5 |
| 10 | 3 | 7 | 7 |
| 21 | 6 | 13 | 15 |

The 21-node boundary tests were corrected accordingly: 15 nodes may commit,
while 14 nodes (`f+1` offline) must stall. A legacy 3-node, `f=0` test that
formed a one-signature certificate was also corrected to require all 3 nodes.

## Compatibility and rollout

This is a consensus-rule break even though the serialized QC/TC layout is
unchanged. `CONSENSUS_PROTOCOL_VERSION` is bumped from 3 to 4 so mixed-version
peers reject each other instead of silently disagreeing about certificate
validity.

All validators must be stopped and upgraded together. A rolling validator
upgrade is not supported. Persisted v3 certificates with fewer than `n-f`
signers will be rejected after the upgrade; deployments whose validator count
was always `3f+1` are unaffected because the two formulas are equal there.

## Verification

- `cargo test -p n42-chainspec -p n42-consensus --lib` (31 + 184 passed)
- `cargo test -p n42-consensus --test integration_test` (67 passed)
- `cargo test -p n42-consensus-service quorum -- --nocapture` (5 passed)
- `cargo test -p n42-consensus --test chaos_7node` (12 passed)
- fresh-join/restart execution catch-up floor regressions (2 passed)
- `cargo check --all-targets`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --workspace`
- release build: `cargo build --release -p n42-node-bin -p e2e-test`
- real full-profile Scenario 4:
  `target/release/e2e-test --binary target/release/n42-node --scenario 4`
  (1/3/5/7-node sets all finished at height 14; all 21 nodes finished at
  height 16; V1-V5 and the complete scenario passed)

The first Scenario 4 attempt correctly failed at the 5-node boundary
(`[10, 10, 10, 10, 0]`). After the catch-up-floor correction, the late fifth
node requested from view 1, imported the two missing ancestors, and converged
with the other four. The 7-node run independently exercised the same path: its
last two starters imported two and three missing blocks respectively before all
seven converged at height 14.
