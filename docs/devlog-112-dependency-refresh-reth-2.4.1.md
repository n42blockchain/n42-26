# Devlog 112: dependency refresh and Reth 2.4.1 baseline

Date: 2026-07-21
Latest compatible lock refresh: 2026-08-02

## Scope

Refresh the N42 workspace to the latest compatible stable dependencies while preserving the
deployed replay, consensus, snapshot, keystore, and proof formats. The paired execution-layer
checkout is the N42 Reth 2.4.1 fork:

- branch: `chore/reth-upstream-20260726`
- commit: `d025e10403b5b0e7ef31f8d6359406f528b0e203`
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

Moving the paired checkout from `c533db8` to `d025e1040` added two real dependency edges to the
N42 lock graph (`parking_lot` and `libc`) without changing package versions or deployed formats.
The lockfile was regenerated offline, then accepted by a fresh `--locked` all-target check.

The refreshed RustSec database reports no new vulnerability outside the three exact, documented
nightly exceptions in `audit.toml`. Two belong to hickory 0.25 through libp2p mDNS, which is
disabled by default in production. The latest published libp2p remains 0.56 and still selects
hickory 0.25; upstream PR 6423 moves the unreleased 0.57 line to hickory 0.26.1. The third is an
inactive optional `tracing-subscriber 0.2.25` edge under arkworks; N42's active logging path is
0.3.23. Running `cargo audit` with those three explicit advisory IDs ignored passes, so a new
advisory will still fail the nightly gate.

## Verification

- `cargo check --workspace --all-targets`: passed
- `cargo test --workspace`: passed (1,285 passed, 8 intentionally ignored)
- `cargo clippy --workspace --all-targets -- -D warnings`: passed
- JNI 0.22 host compile/clippy harness for `src/android.rs`: passed with warnings denied
- paired `../reth` status after verification: clean

The 2026-08-02 lock refresh and paired-Reth follow-up repeated the locked all-target workspace
check, complete locked workspace test suite, and warnings-denied locked all-target Clippy gate;
all passed. These checks ran against the clean paired Reth checkout at
`d025e10403b5b0e7ef31f8d6359406f528b0e203` (workspace version 2.4.1). Reth's own 24 integration
tests, package tests, full-workspace all-target check, and warnings-denied all-target Clippy gate
also pass at that revision.

The excluded `n42-zkproof-guest` remains a separate SP1 RISC-V build and was not host-compiled.
No Android Rust target is installed in this macOS verification environment, so the JNI bridge was
type-checked through a host-side JNI 0.22 compile harness rather than a full Android artifact.
