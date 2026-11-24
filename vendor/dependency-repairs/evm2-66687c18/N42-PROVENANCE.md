# evm2 EIP-8282 builder-request switch

Base: evm2 git revision 66687c18b34d3bdbb899f5eb7605070615c03475
(https://github.com/alloy-rs/evm2, formerly https://github.com/DaniPopes/evm2;
the revision reth v2.11.0 pins), `crates/evm2`, package evm2 0.1.0. Upstream
licenses retained (`LICENSE-*`, and `LICENSES/` from the repository root, which
`src/block/bundle_state` refers to). `README.md` is a copy of the file the
upstream crate symlinks to (`lib.rs` includes it as the crate docs).
`Cargo.toml` is flattened from the workspace manifest (`Cargo.toml.orig` is the
member manifest as published); tests, examples, dev-dependencies and lints are
dropped, and `ark-std/std` is removed from the `std` feature because ark-std is
only a dev-dependency. `evm2-macros` stays a git dependency at the same
revision, so evm2-macros and evm2-jit are not vendored and the graph keeps one
copy of each evm2 crate.

Patch (`N42-PATCH.diff`, against `crates/evm2/src` of the base):

- `block/ethereum/execution.rs`: new
  `post_execution_system_call_state_changes_with_builder_requests(..., builder_requests: bool)`;
  the two EIP-8282 calls (builder deposit `0x0000BFF4…8282`, builder exit
  `0x000064D6…8282`) run only when `builder_requests && spec_id.enables(AMSTERDAM)`.
  The original `post_execution_system_call_state_changes` keeps its signature
  and calls it with `true`.
- `block/ethereum/mod.rs`: `EthBlockExecutor` carries
  `eip8282_builder_requests` (`true` from `new`, kept across `start_block`),
  with `set_eip8282_builder_requests` / `eip8282_builder_requests`, and passes
  it to the function above from `apply_post_execution_changes` (which
  `finish` uses).

Default behaviour is upstream's: nothing changes unless an embedder calls
`set_eip8282_builder_requests(false)`. No other system call, gas rule,
signature, wire format or commitment changes.

Why: evm2 gates the EIP-8282 calls on its spec alone, never on the chain.
n42-26 runs evm2's Amsterdam rules on Profile 1 from `amsterdamTime` while the
chain spec carries no reth Amsterdam hardfork (AM1), so unpatched it calls both
predeploy addresses at the end of every such block (n42-rs, on the same evm2
rev since 2026-11-06, does too until its EVM2_PORT_PLAN S1 lands). The
addresses have no code on N42 chains, but the predeploys are deployable by
anyone (Nick's method; native chains admit pre-EIP-155 transactions); from
then on the calls change state and emit requests (or a reverting contract
makes every block invalid), while gov5 never makes them on a native chain
(`UsesEIP8282BuilderRequests` = Glamsterdam and not `IsN42Native`; standard
D2): a chain split. n42-26 sets the switch in
`n42_execution::restored_slots::TrackingBlockExecutorFactory::create_executor`
(the one factory every `N42EvmConfig` executor comes from) to
`n42_execution::eip8282_builder_requests_active(genesis)` =
`!n42_chainspec::native::is_n42_native(genesis)`, gov5's predicate. Tests:
`crates/n42-node/tests/profile1_vectors.rs` (`*eip8282*`: genesis deploy,
mid-chain deploy, non-native counterpart).

Re-applying on an evm2 upgrade (new rev R):

1. If upstream evm2 has gained its own embedder switch for the EIP-8282 calls
   (or gates them on the chain's hardforks), drop this directory, restore the
   `evm2` git `[patch]` entry in the workspace `Cargo.toml`, and use that switch
   in `TrackingBlockExecutorFactory::create_executor`.
2. Otherwise copy `crates/evm2/{src,build.rs}` of R here (README dereferenced,
   licenses from the root), rename the directory to `evm2-<R prefix>`, apply
   `N42-PATCH.diff` (`patch -p1` from this directory; resolve by hand if
   `post_execution_system_call_state_changes` or `EthBlockExecutor` moved), and
   regenerate the diff with `diff -u --label upstream-<R>/<f> --label n42/<f>`.
3. Re-flatten `Cargo.toml` from R's `crates/evm2/Cargo.toml` and root
   workspace manifest, keeping `evm2-macros` as a git dependency at R, and move
   the workspace `evm2`, `evm2-macros`, `evm2-jit` revisions to R together.
4. Check `cargo tree -i evm2` shows the path package only, `Cargo.lock` has a
   single `evm2`, and the `*_eip8282_*` tests pass.

Remove this replacement when the maintained dependency provides the same
switch.
