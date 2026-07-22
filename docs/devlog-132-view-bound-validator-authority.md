# Devlog 132 — View-bound validator authority

Date: 2026-07-22

## Outcome

QC and sync verification now resolve the validator committee authorized for the
certificate's exact view. The previous bitmap-length fallback was removed from
the consensus engine and sync importer.

This closes an authority-substitution path: BLS verification proves that a
selected key set signed the bytes, but a matching bitmap width does not prove
that the set was authorized at that view. A removed committee could otherwise
sign a future-view certificate, and the verifier could select it from history
solely because its size matched.

## Changed paths

- Decide, PrepareQC, piggybacked PrepareQC, and generic QC verification use
  `known_validator_set_for_view` and fail closed when the exact set is absent.
- Sync-block CommitQC verification applies the same view-bound lookup and checks
  the signer bitmap width only after authority has been resolved.
- `find_validator_set_by_len` was removed to prevent new safety-critical callers
  from reintroducing committee guessing.
- Cross-epoch liveness remains available when an unchanged or rotated next set
  is explicitly staged by the authenticated schedule or committed transition.

## Regression coverage

- A cryptographically valid four-validator CommitQC claiming a view governed by
  a five-validator set is rejected by the core resolver.
- The same old-committee substitution is rejected by the block-sync verifier.
- A lagging leader still recovers across an epoch when the target view's
  unchanged committee is explicitly staged.

Focused results:

- `cargo check -p n42-consensus -p n42-consensus-service --all-targets`: passed;
- `cargo test -p n42-consensus`: 212 unit, 12 chaos, and 67 integration tests
  passed;
- `cargo test -p n42-consensus-service`: 162 tests passed.

## Interop impact

The Gov5 seven-node static H2-v4 profile uses one committee for every view, so
observer following is unchanged. Dynamic rotation and validator participation
must provide exact staged/scheduled membership; unknown historical authority is
now a recoverable liveness gap instead of a safety guess.
