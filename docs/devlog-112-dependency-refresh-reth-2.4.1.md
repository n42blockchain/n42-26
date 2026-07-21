# Devlog 112: dependency refresh and Reth 2.4.1 baseline

Date: 2026-07-21

## Scope

Refresh the N42 workspace to the latest compatible stable dependencies while preserving the
deployed replay, consensus, snapshot, keystore, and proof formats. The paired execution-layer
checkout is the N42 Reth 2.4.1 fork:

- branch: `chore/reth-upstream-20260719`
- commit: `c533db8bad6f300be93ec047ecffc717b08957f8`
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

## Verification

- `cargo check --workspace --all-targets`: passed
- `cargo test --workspace`: passed (1,285 passed, 8 intentionally ignored)
- `cargo clippy --workspace --all-targets -- -D warnings`: passed
- JNI 0.22 host compile/clippy harness for `src/android.rs`: passed with warnings denied
- paired `../reth` status after verification: clean

The excluded `n42-zkproof-guest` remains a separate SP1 RISC-V build and was not host-compiled.
No Android Rust target is installed in this macOS verification environment, so the JNI bridge was
type-checked through a host-side JNI 0.22 compile harness rather than a full Android artifact.
