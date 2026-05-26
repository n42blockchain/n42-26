# Devlog 57: reth v2.2.0 Upgrade

> Date: 2026-05-26
> Scope: Confirm N42 reth fork integration on upstream v2.2.0 and refresh dependency lockfile

## Summary

N42 continues to resolve reth through the repository-standard sibling path:

```text
../reth
```

That checkout must be `n42blockchain/reth` branch `n42-v2-upgrade`. The fork is
based on upstream tag `v2.2.0` (`88505c7fcbfdebfd3b56d88c86b62e950043c6c4`)
with the N42 patch layer applied.

A temporary clean worktree named `../reth-v2.2.0-n42` was used locally during
the audit to avoid modifying an unrelated dirty `../reth` checkout. That
temporary path was intentionally not committed because CI and developer setup
use `../reth`.

## Dependency Alignment

- reth local path packages: `2.1.1` -> `2.2.0`
- alloy / alloy-evm / revm dependency graph refreshed through `Cargo.lock`
- `Cargo.toml` remains on `../reth/...` so GitHub Actions can use the existing
  `n42blockchain/reth@n42-v2-upgrade` checkout step

## N42 Patches Reapplied

- Payload build path keeps N42 packing limits and timing logs:
  - `N42_MAX_TXS_PER_BLOCK`
  - `N42_BUILD_TIME_BUDGET_MS`
- Payload execution output is cached for both engine validation and broadcast
  paths.
- Engine payload validation can reuse cached execution output instead of
  re-executing local payloads.
- N42 state-root acceleration flags are preserved:
  - `N42_SKIP_STATE_ROOT=1`
  - `N42_DEFER_STATE_ROOT=1`
- `BasicBlockBuilder::finish` supports the N42 placeholder state-root path when
  those flags are enabled.

## Validation

- `cargo check --all-targets`
- `cargo fmt --all`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --workspace`
- `cargo build --release -p n42-node-bin -p e2e-test`
- `E2E_SCENARIO_FILTER=1,3,4 target/release/e2e-test --binary target/release/n42-node`
- `E2E_SCENARIO_FILTER=5,8,12 target/release/e2e-test --binary target/release/n42-node`

## E2E Coverage

- Scenario 1: single-node block production passed.
- Scenario 3: ERC-20 deployment and transfer batch passed.
- Scenario 4: consensus passed for 1, 3, 5, 7, and 21 node profiles.
- Scenario 5: mobile attestation verification passed.
- Scenario 8: mobile EVM verification over QUIC passed.
- Scenario 12: Blockscout RPC compatibility passed, 17/17 checks.

## Status

- [x] Latest upstream reth version confirmed as `v2.2.0`
- [x] Local dependency paths kept on reproducible `../reth` fork checkout
- [x] N42 payload cache and state-root acceleration patches verified on fork
- [x] Workspace compile, clippy, tests, release build, and E2E passed
