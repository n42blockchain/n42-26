# State-root hard floor and Twig↔reth probe (gov5 2026H1 P1-3)

Date: 2026-07-18

## Outcome

The node now has two independent state-integrity floors:

1. reth state-root bypass flags cannot start on a production chain;
2. the default QMDB binary-tree/Twig sidecar is sampled against reth's exact
   post-state and mobile output is fail-closed on a real value mismatch.

The two roots are deliberately **not compared**. reth commits an Ethereum MPT;
Twig uses a sharded append-ordered binary-tree commitment, so equal values have
different roots by construction.

## Startup hard gate

The existing production guard was audited and retained. Either
`N42_SKIP_STATE_ROOT=1` or `N42_DEFER_STATE_ROOT=1` requires both:

- `N42_ALLOW_BENCH_MODE=1`;
- a chain id in the reserved `[4242400, 4242500)` benchmark range.

Any production chain id exits non-zero. An allowed benchmark process now also
prints an explicit `UNSAFE ... STATE-ROOT ... BYPASSED` startup banner and
exports `n42_deferred_state_root_enabled`.

## Exact-block value probe

`StateSink::apply_diff` now carries the committed block hash. Across each
32-version window by default, the node maintains a bounded deterministic top-M
reservoir ranked by `blake3(block_hash || address)` and samples eight changed
accounts at the window boundary. This retains candidates from earlier blocks;
sampling only the boundary block would miss an earlier bad write permanently.
The rate is tunable:

- `N42_TWIG_PROBE_INTERVAL` (minimum 1);
- `N42_TWIG_PROBE_ACCOUNTS` (1..=256).

For each selected account, the probe compares:

- account existence, balance, nonce and normalized code hash;
- every storage slot changed for that selected account.

The reference is `StateProviderFactory::state_by_block_hash`, not `latest`, so a
faster later import cannot cause a false mismatch. Zero/missing storage is
normalized according to EVM semantics. Provider availability errors are
observable and retried at a later sample; they do not accuse the tree of
divergence.

## Fail-closed mobile path

A malformed Twig leaf, Twig apply/persistence error, or actual reth/Twig value
mismatch latches a shared health flag false and sets rebuild-required metrics.
The mobile packet loop then suppresses both block packets and new-session cache
sync. A later successful sample cannot clear the latch: packet output resumes
only after the operator rebuilds/reopens a known-good sidecar.

Primary metrics:

- `n42_twig_consistency_probe_total`;
- `n42_twig_consistency_probe_error_total`;
- `n42_twig_consistency_mismatch_total`;
- `n42_twig_sidecar_error_total`;
- `n42_twig_sidecar_healthy`;
- `n42_twig_rebuild_required` / `_total`;
- `n42_mobile_packet_suppressed_unhealthy_sidecar_total`.

## Verification

The fault-injection unit test writes an intentionally wrong Twig account value
at sidecar version 1 while the reference reader returns the correct post-state;
version 2 deliberately has an empty diff. With `interval=2`, the retained
candidate is still checked at the boundary, latches unhealthy, and proves that
a later matching sample does not self-heal.

```text
cargo test -p n42-node
  141 passed; 0 failed

cargo test -p n42-consensus-service
  137 passed; 0 failed

cargo clippy --all-targets -- -D warnings
  passed
```

Task-book E2E Scenario 5 was run against a release node:

```text
E2E_SCENARIO_FILTER=5 RUST_LOG=info \
  target/release/e2e-test --binary target/release/n42-node

3 nodes: final height 16/16/16, QC present on all nodes
15 phones: 240 accepted, 0 rejected, 0 errors
Scenario 5 PASSED
```
