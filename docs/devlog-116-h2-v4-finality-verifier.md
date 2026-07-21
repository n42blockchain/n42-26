# Devlog 116 — H2-v4 cross-client finality verifier

Date: 2026-07-21

## Outcome

The Rust observer can now treat a gov5 H2-v4 `Decide` as a proven sync target,
but only after validating the complete CommitQC against the validator set. The
path remains read-only: it does not vote, propose, or enter the native Rust
HotStuff state machine, so existing validator and seven-node defaults are
unchanged.

The verifier rejects:

- non-`Decide` messages;
- wrapper/QC view or block-hash mismatches;
- non-canonical signer bitmaps or validator-count mismatches;
- fewer than `n-f` signers;
- malformed aggregate signatures;
- signatures produced for another chain identity or validator-changes hash.

## Cross-language BLS compatibility

The signed fixture is produced by gov5 with four deterministic validators and
three signers, serialized through the real H2-v4 envelope, and consumed by the
Rust test. This exposed a ciphersuite difference hidden by the earlier byte
vectors: gov5 uses the BLS proof-of-possession DST (`..._POP_`), while native
Rust consensus uses `..._NUL_`.

H2-v4 therefore has explicit POP signing/aggregate-verification entry points.
The native Rust BLS domain remains unchanged, and a regression test proves that
an H2-v4 signature is rejected by the native verifier. This avoids silently
changing existing validator keys or signatures while making gov5 proofs
verifiable.

Fixture: `crates/n42-consensus-service/testdata/h2_v4_finality_v1.json`.

## Validation

- gov5 `TestH2V4CrossClientFinalityProof`: pass.
- Rust gov5 fixture verification plus wrong-domain and under-quorum cases: pass.
- Rust native/H2-v4 ciphersuite separation test: pass.
- `cargo test --workspace` before this additive verifier: pass, including the
  seven-node chaos suite, QMDB compatibility, stream-v2, node, and doc tests.
- `go test ./...`: interop packages pass. The aggregate run is not green due to
  absent external Hive genesis fixtures and one timing-sensitive negative-cache
  harness iteration; the latter passes three consecutive focused reruns.

No existing replay, archive, testnet, or performance data directory was
deleted or rewritten.
