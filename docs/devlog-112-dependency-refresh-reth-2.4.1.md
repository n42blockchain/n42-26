# Devlog 112: dependency refresh and Reth 2.4.1 baseline

Date: 2026-07-21
Latest compatible lock refresh: 2026-08-04

## Scope

Refresh the N42 workspace to the latest compatible stable dependencies while preserving the
deployed replay, consensus, snapshot, keystore, and proof formats. The paired execution-layer
checkout is the N42 Reth 2.4.1 fork:

- branch: `chore/reth-alloy-matrix-20260804`
- commit: `acb016ee1d81db90a4747bac22129d3c57c1bc04`
- workspace version: `2.4.1`

The Reth checkout remained clean. N42 continues to use local `../reth` paths because the fork
contains the N42 execution hooks; `README.md` and the workspace manifest now record the exact
paired revision.

## Updated dependency families

- crypto and randomness: `aes-gcm 0.11`, `scrypt 0.12`, `hmac 0.13`, workspace `sha2 0.11`,
  `rand 0.10`, and `getrandom 0.4`
- clients and wire transports: `reqwest 0.13` and `tokio-tungstenite 0.30`
- runtime and configuration: `tokio 1.53.1`, `toml 1.1`, `lru 0.18`, and `rcgen 0.14.8`
- tooling and platform bindings: `criterion 0.8`, `tikv-jemallocator 0.7`, and Android `jni 0.22`

The source adaptations are API-only: Rand 0.10 trait imports, HMAC `KeyInit`, Scrypt parameter
construction, AES-GCM nonce conversion, and Criterion's replacement of its deprecated
`black_box` re-export. Keystore encrypt/decrypt and save/load tests confirm the upgraded crypto
path retains its behavior. The Android bridge was migrated from the deprecated `JNIEnv` entry
shape to JNI 0.22's FFI-safe `EnvUnowned`/`Env` boundary.

## Compatibility pins

These versions intentionally do not follow their newest major release:

- `bincode 1.3`: consensus, network, replay, and snapshot bytes are already deployed. A move to
  bincode 3 requires a versioned wire migration and old-data replay fixtures.
- direct `secp256k1 0.30`: Reth testing utilities expose this version's key types. Using 0.31 as
  the direct test dependency creates distinct, non-interchangeable Rust types.
- `sp1-sdk 4.2.1`, guest `sha2 0.10`, and guest `bincode 1.3`: the prover and guest are one
  deployed circuit/serialization boundary and must be upgraded together.
- Alloy 2.2.0 / alloy-primitives 1.6.1 / alloy-evm 0.37.1 / revm 41.0.0 remain the Reth 2.4.1
  release matrix and are not independently overridden.

`cargo update --dry-run --verbose` reports no compatible updates left. Its remaining newer
versions are the explicit pins above or transitive packages whose parent dependency controls the
version.

## 2026-08-02 compatible lock refresh

A fresh registry and git-index resolution found 37 newer semver-compatible packages after the
original 2026-07-21 validation. `Cargo.lock` now includes all of them while retaining the exact
Reth 2.4.1 checkout and the compatibility pins above. The refresh covers AES/BLST and supporting
crypto crates, Rustls and PKI types, Tokio macros/streams, HTTP, Clap, TOML, platform/build
tooling, palette, Pest, Schemars, and other transitive maintenance releases. It also removes the
obsolete `fast-srgb8` edge and adds `palette_math` through palette 0.7.7.

After the update, another `cargo update --dry-run --verbose` locked zero packages. The only newer
versions reported are the intentional major or release-matrix pins already documented above.
The updated lockfile SHA-256 is
`62463bc57fa161ffaf411f7544473603e63ee5ab3781279c2e0682ce4567124e`.

## 2026-08-02 paired Reth follow-up

The paired fork now includes the post-2.4.1 upstream fixes merged by
`chore/reth-upstream-20260726`. Its default `reth` feature set no longer enables
`revmc`/LLVM 22 implicitly, keeping ordinary macOS and Windows builds portable; JIT remains
available through explicit `--features jit`. The Reth dev-node integration harness also uses
unique ports and a bounded 60-second readiness check with captured child logs instead of the
dependency helper's fixed ten-second deadline.

Moving the paired checkout from `c533db8` to `91725e3aa` added two real dependency edges to the
N42 lock graph (`parking_lot` and `libc`) without changing package versions or deployed formats.
The lockfile was regenerated offline, then accepted by a fresh `--locked` all-target check. The
Reth follow-up also removes the now-unused `alloy-node-bindings` test dependency and its obsolete
transitive lock entries; that Reth-only dev-graph cleanup does not change the N42 lock graph.

The refreshed RustSec database reports no new vulnerability outside the three exact, documented
nightly exceptions in `audit.toml`. Two belong to hickory 0.25 through libp2p mDNS, which is
disabled by default in production. The latest published libp2p remains 0.56 and still selects
hickory 0.25; upstream PR 6423 moves the unreleased 0.57 line to hickory 0.26.1. The third is an
inactive optional `tracing-subscriber 0.2.25` edge under arkworks; N42's active logging path is
0.3.23. Running `cargo audit` with those three explicit advisory IDs ignored passes, so a new
advisory will still fail the nightly gate.

## 2026-08-04 live compatible refresh

A current-index `cargo update --dry-run --verbose` found another compatible maintenance wave.
The lock now absorbs `aho-corasick 1.1.5`, the data-encoding 2.11.1 macro family,
`instability 0.3.13`, `line-clipping 0.3.8`, `lru 0.18.2`, and the vergen 10.0.2 family,
including the new Darling 0.24 derive edge required by the refreshed graph.

The same dry run also exposed that the documented Alloy release-matrix pin was only a lockfile
pin: direct workspace requirements used normal caret semantics and therefore allowed the newly
published Alloy 2.3 line. The direct Alloy 2.2 requirements are now exact, and every Alloy
protocol/RPC/transport package in `Cargo.lock` remains on the single 2.2.0 matrix selected by
Reth 2.4.1. The paired Reth manifest now exact-pins the complete Alloy 2.2 family, including its
proc-macro helper, so a dry run reports every Alloy 2.3 package as unchanged rather than allowing
a partial 2.2/2.3 graph. No compatible update remains.

The refreshed lockfile SHA-256 is
`834ca9a291f075358a602907447e5c86edbcbf6bfc4c1660c13b508063405716`.

Fresh isolated-worktree validation against clean Reth commit `acb016ee1d81` passed:

- `cargo check --locked --workspace --all-targets`
- `cargo test --locked --workspace` (1,285 passed, 8 intentionally ignored)
- `cargo clippy --locked --workspace --all-targets -- -D warnings`

The security gate also passes with only the three existing vulnerability exceptions, using the
same explicit `--ignore` arguments as `.github/workflows/nightly.yml`. RustSec additionally
reports `RUSTSEC-2026-0002` for `lru 0.12.5` as an informational unsound warning. That version is
absent from the default feature graph and is reachable only through the optional
`n42-zkproof -> sp1-sdk 4.2.1 -> sp1-cuda/sp1-prover` all-features graph. N42's direct path uses
patched `lru 0.18.2`, and Reth's active path uses patched `lru 0.16.4`; the SP1 stack stays pinned
until its deployed circuit and serialization boundary can be migrated as a unit.

All workflows now fetch the exact paired Reth commit instead of the older movable integration
branch, so CI exercises the same 2.4.1 source revision used by this validation.

The paired Reth lock independently absorbed 64 compatible maintenance updates. Its optional JIT
source changed from a moving `main` branch to exact revmc commit `520462a4`, preventing a routine
refresh from silently introducing Alloy-EVM 0.38 and REVM 42. The resulting Reth graph contains
only Alloy 2.2.0, Alloy-EVM 0.37.1, and REVM 41.0.0. Reth's locked full-workspace all-target check,
`reth-consensus` tests/doc-test, and full-workspace warnings-denied Clippy gate all pass; its
security audit passes with the sole inactive `tracing-subscriber 0.2.25` exception.

## Verification

- `cargo check --workspace --all-targets`: passed
- `cargo test --workspace`: passed (1,285 passed, 8 intentionally ignored)
- `cargo clippy --workspace --all-targets -- -D warnings`: passed
- JNI 0.22 host compile/clippy harness for `src/android.rs`: passed with warnings denied
- paired `../reth` status after verification: clean

The 2026-08-02 lock refresh and paired-Reth follow-up repeated the locked all-target workspace
check, complete locked workspace test suite, and warnings-denied locked all-target Clippy gate;
all passed. These checks ran against the clean paired Reth checkout at
`91725e3aa8f2a0bbc5a425e931a2f2b2f31b2a7b` (workspace version 2.4.1). Reth's own 24 integration
tests, package tests, full-workspace all-target check, and warnings-denied all-target Clippy gate
also pass at that revision.

The excluded `n42-zkproof-guest` remains a separate SP1 RISC-V build and was not host-compiled.
No Android Rust target is installed in this macOS verification environment, so the JNI bridge was
type-checked through a host-side JNI 0.22 compile harness rather than a full Android artifact.
