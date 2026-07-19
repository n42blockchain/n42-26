# Devlog 106: Mobile evidence sparse-bitfield persistence

Date: 2026-07-18
Branch: `fix/gov5-sync-mobile-evidence-bitfield`

## Finding

`AggregatedAttestation.participant_bitfield` uses verifier-registry indices as
bit positions, while `participant_count` is only the number of set bits. The
MDBX evidence codec incorrectly serialized exactly
`ceil(participant_count / 8)` bytes.

That happens to work for a dense prefix such as indices 0 through 20. It loses
evidence for any sparse cohort: a single signer at registry index 9,999 creates
a 1,250-byte bitfield, but the old codec persisted only its first byte.

## Fix

- Mobile evidence encoding `0x02` explicitly stores a `u32` bitfield byte
  length and then the complete bitfield.
- Encoding `0x01` remains readable for historical rows. It necessarily cannot
  restore high bits that an old writer already discarded.
- Unknown encoding tags, trailing bytes, and decoded bitfields over 16 MiB are
  rejected.
- Node write-back verifies that `valid_count`, `participant_count`, and the
  bitfield popcount agree, and rejects trailing zero-byte non-canonical masks.

The consensus-validator bitfield is unchanged: its `signer_count` field is the
validator-set bit length, so `ceil(signer_count / 8)` already has the intended
meaning there.

## Compatibility

New binaries read both v1 and v2 evidence rows and write v2 for all new mobile
evidence. The change affects only node-local bypass evidence; mobile evidence
does not enter HotStuff voting and the evidence root is not placed in the block
header (`parent_beacon_block_root` remains zero).

The datadir migration is one-way for rows rewritten as v2: an older binary does
not understand tag `0x02` and must not be pointed at the same MDBX datadir after
the upgrade. Rollback requires a backup made before v2 writes (or a fresh
resync), not an in-place binary downgrade.

## Validation

- `cargo test -p n42-jmt evidence_store --lib`: 11 passed, including v1 read,
  invalid-tag rejection, and registry index 9,999 round-trip.
- `cargo test -p n42-node mobile_evidence --lib`: 5 passed, including sparse
  write-back and count/bitfield mismatch rejection.
- `cargo clippy -p n42-jmt -p n42-node --all-targets -- -D warnings`: passed.

Full-repository `cargo fmt --all -- --check` is not a usable gate on this
baseline because the upgraded stable rustfmt reports existing formatting
differences throughout n42-26 and the reth submodule. The two touched Rust files
were formatted directly and `git diff --check` is clean.
