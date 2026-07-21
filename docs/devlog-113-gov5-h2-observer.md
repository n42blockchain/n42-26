# Devlog 113 — gov5 H2 wire parity and observer transport

Date: 2026-07-21

## Outcome

n42-26 can now subscribe to and strictly decode the live legacy gov5
HotStuff-2 gossip stream in observer mode. The compatibility path is isolated
from Rust consensus participation because the two clients do not yet sign the
same Round-2 preimage.

## Shared vectors

`crates/n42-network/testdata/cross_client_h2_v1.json` is generated and pinned
by gov5. It covers all seven current envelope kinds:

- Proposal, Vote, CommitVote, PrepareQC, Timeout, NewView, and Decide;
- nested QC and TC values, signer bitmap packing, optional HighTC, and a
  Go-produced raw-Snappy Vote gossip frame;
- prepare, commit, timeout, and new-view signing preimages.

The same fixture has SHA-256
`0c5877432b8d7adb3fc60c5226564ad1d0e099b6c73f39b823703926e82d2aee`
in both repositories.

## Rust observer path

Observer startup derives gov5's topic from the first four bytes of the local
genesis hash:

`/n42/<fork-digest>/hotstuff_consensus/ssz_snappy`

Inbound data is bounded before Snappy allocation, then decoded with exact
field limits, fixed 32-byte hashes, valid 96-byte individual BLS signatures,
canonical signer bitmaps, complete nested QC/TC parsing, and trailing-byte
rejection. Valid messages emit observer metrics/events only; they never reach
the Rust voting engine.

The codec encoder applies the same field limits as the decoder. In particular,
encoded HighTC fields are capped at 4096 bytes, with an explicit regression
test for the audit-reported asymmetric-bound failure mode.

## Remaining safety gate

gov5 signs CommitVote as `commit || view_le || block_hash` (46 bytes). Rust v3
also binds `validator_changes_hash` (78 bytes). Mixed validators therefore stay
disabled until the versioned H2-v4 schema and signature domain are implemented
on both clients. Observer support is intentionally read-only.

## Validation

- `go test ./internal/consensus/hotstuff`
- `cargo test -p n42-network h2_wire`
- `cargo test -p n42-network gossipsub::topics`
- `cargo check -p n42-consensus-service`
- `cargo check -p n42-node`

