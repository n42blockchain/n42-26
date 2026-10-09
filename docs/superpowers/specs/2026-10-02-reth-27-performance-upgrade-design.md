# Reth 2.7.0 and dependency performance upgrade

Date: 2026-10-02

## Intent

Advance the N42 execution stack toward the seven-node sustained performance
target while keeping Gov5 H2-v4 validation and QMDB commitments unchanged.
The current plan's primary target remains 400,000 successfully committed
transactions per second under full validation. Upstream Reth results are leads
for experiments, not N42 performance claims.

The recorded 2026-09-27 baseline was 37,866 successful committed TPS on a
seven-node, 220,000-transaction block-cap run. The newer 2026-10-01 parallel
follower-import A/B/A passed receipt/root/QC audit but regressed: A1 41,222.50
TPS, B 37,536.38 TPS, A2 41,370.65 TPS. A1/A2 drift was about 0.36%; B was
about 9.1% below the A mean. Parallel follower import is rejected for this
configuration unless a later controlled run overturns this result.

## Selected approach

Use Reth v2.7.0 as the compatibility and performance target. Its upstream base
tag resolves to `3d592ece6de8c4559987416a544fc215fd6d6921` in the available
Reth object database. Upgrade the N42 Reth source in an isolated writable
checkout and preserve the existing local checkout, which currently has 407
modified or untracked paths. Do not reset, clean, or overwrite that checkout.
Commit the N42 adaptations on top of the exact upstream base and pin that final
adapted commit in the dependency source lock/check; record the upstream base
SHA separately. Do not follow a moving branch.

Carry forward the N42-specific Reth integration as a small, reviewable patch
series: payload transaction-root handling, QMDB state-reader integration,
import-batch execution, Gov5 cached state-root support, and the payload
execution/broadcast cache APIs used by current N42 call sites. Port each patch
to the selected Reth base, document its source commit and API surface, and keep
N42's compatibility code in the N42 repository where possible. Upgrade other
direct dependencies only when required by the selected Reth version or when a
separate compatibility/security review identifies a concrete need. Preserve
protocol pins used by Gov5, SP1, and replay formats.

Reth v2.6 introduced partial persistence defaults. Until QMDB reads, canonical
execution and restart recovery have been proven with that behavior, configure
the upgraded node to preserve the current immediate-persistence semantics.
Do not enable state masking as part of the initial dependency migration. Any
later partial-persistence experiment is a separate design and A/B round.

Keep hardfork activation, transaction validity, receipts, state roots,
CommitQC rules, and the standard execution fallback unchanged. Reth's new
protocol support must not activate a fork for the N42 chain as a side effect
of upgrading the library.

## Upgrade stages

1. **Freeze and map the source.** Record the current Reth base commit, all
   N42-specific patches, current Cargo lock graph, and N42 integration call
   sites. Preserve the dirty sibling checkout. Select a clean, exact v2.7.0
   source commit in a separate writable checkout.
2. **Port N42 integration.** Reapply the five required N42 Reth patches individually.
   Build a compatibility table for changed public APIs and update the N42
   adapter/manifest only where needed. Keep each patch independently
   reviewable and reversible.
3. **Refresh dependency pins.** Update Cargo manifests and lockfile in a
   separate change. Pin Reth to the exact selected commit and update only
   required transitive versions. Record any independent dependency changes
   separately from the Reth bump.
4. **Prove semantics.** Run focused tests for payload transaction roots,
   QMDB reads and state-root comparison, import-batch execution, Gov5 block
   validation, receipt roots, fork activation, persistence, and restart
   recovery. Run the complete workspace checks before performance testing.
5. **Measure performance.** Use the same seven-node H2-v4/QMDB workload,
   genesis, block cap, pacing, host placement, and full validation gates for
   the old and upgraded builds. Run warmup and A/B/A legs with immutable
   binaries/configuration hashes. Compare first and sustained windows,
   successful receipts, block cadence, leader validation, follower import,
   persistence lag, CPU, RSS, and disk activity. Keep only improvements that
   exceed A-bookend drift and pass every correctness check.
6. **Update records.** Record the exact Reth SHA, required API adaptations,
   Cargo lock changes, tests, and benchmark artifacts. Make small commits
   grouped as Reth source integration, N42 compatibility, dependency pins,
   and benchmark evidence. Push only when the repository's Git metadata is
   writable and the result has passed review.

## Acceptance criteria

- Exact, immutable Reth source revision is recorded and reproducible.
- All five required N42 Reth patches are represented and apply cleanly to the new
  base; no local changes in the existing sibling checkout are lost.
- Workspace builds and focused semantic tests pass with the upgraded stack.
- QMDB-only reads, Gov5 H2-v4 signatures, five-of-seven CommitQC, transaction
  roots, receipt roots, state roots, persistence, and restart checks remain
  correct on all seven validators.
- A performance claim is made only from a complete seven-node run with all
  successful receipts and matching hashes, QC, and roots. A headline TPS
  number alone is insufficient.
- Any regression in sustained throughput, memory growth, persistence lag,
  invalid blocks, or timeout certificates is documented and blocks retention
  until resolved.

## Upstream evidence and limits

Reth v2.5.0 reports 5–10% lower mean block-processing latency versus v2.4.1,
including sender-recovery caching and transaction-pool prewarming. Reth v2.6.0
reports an 8.6% `newPayload` latency reduction on a 300M-gas BAL workload, but
also changes persistence defaults and public APIs. Reth v2.7.0 enables shared
sender-recovery caching by default and changes RPC cache limits. These are
upstream workloads and do not predict N42's Gov5/QMDB result.

Sources: https://github.com/paradigmxyz/reth/releases/tag/v2.5.0,
https://github.com/paradigmxyz/reth/releases/tag/v2.6.0,
https://github.com/paradigmxyz/reth/releases/tag/v2.7.0

## Constraints discovered during design

- The N42 repository's `.git` metadata is read-only in this session, so
  commits and pushes cannot be created here even though workspace files are
  writable.
- The current `../reth` checkout is outside the writable roots and contains
  407 modified/untracked paths. It must remain untouched. Reth source work
  needs a clean, writable checkout supplied inside the workspace or in a
  later environment where the intended checkout is writable.
