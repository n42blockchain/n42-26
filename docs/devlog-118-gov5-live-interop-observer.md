# Devlog 118 — gov5 ↔ n42-26 live H2-v4 observer

Date: 2026-07-21

## Outcome

A Rust n42-26 observer connected to a live seven-node gov5 custom chain over
TCP/Noise/Yamux, subscribed to both gov5 H2 topics, decoded an H2-v4 Decide
envelope, and verified its BLS CommitQC against the seven-validator set.

The successful proof was for view 476 and block
`0x70fd6e30d5672f14f2b229640d08fff4e207e90af1cd8dff9c79620ad5b1688d`.
The source peer was gov5 node0
`16Uiu2HAm3Gt3Mb8e4HX7iD6LJKKygiX9qUFjMF2Pticmva2EpxkX`.

This is a real cross-process and cross-language result, not a fixed-vector
test. The run used a newly generated seven-node runtime tree. Existing replay,
seven-node, archive, and performance data was neither opened nor removed.

## Compatibility gaps found and closed

1. n42-26 consensus networking was QUIC-only, while the tested gov5 libp2p
   host listened on TCP. Observer mode now adds TCP/Noise/Yamux while the
   validator/full-node transport remains QUIC-only.
2. gov5 JSON represents BLS public keys as `0x`-prefixed 48-byte strings.
   `BlsPublicKey` deserialization now accepts that representation in addition
   to the existing byte array representation.
3. gov5/QMDB and Reth/MPT compute different block-0 hashes from the same custom
   genesis JSON because the state commitment is different. Observer mode now
   accepts a strict `N42_INTEROP_GENESIS_HASH` override for H2 topics and
   signing domains while retaining the local Reth genesis for execution.
4. Read-only observers no longer require deterministic validator libp2p
   identities merely because a multi-validator config omits `p2p_peer_id`.
   The production validator authentication policy is unchanged.

## Reproduction

Build n42-26 and start an already initialized gov5 custom network. Then export
the following values and run the non-destructive observer launcher:

```bash
export N42_GOV5_GENESIS=/path/to/gov5/genesis.json
export N42_CONSENSUS_CONFIG=/path/to/n42-consensus.json
export N42_INTEROP_GENESIS_HASH=0x<gov5-block-0-hash>
export N42_TRUSTED_PEERS=/ip4/127.0.0.1/tcp/30300/p2p/<gov5-peer-id>
export N42_INTEROP_RUNTIME_ROOT=/path/to/a-new-or-existing-observer-runtime
scripts/run-gov5-interop-observer.sh
```

The launcher only creates missing observer directories. It does not delete,
truncate, migrate, or clean any data directory. A successful run logs:

```text
peer connected ...
gossipsub message received ... topic=/n42/h2/4/ssz_snappy
verified gov5 H2-v4 finality proof ...
```

## Verified boundary and next work

The live H2-v4 chain identity, wire envelope, Snappy framing, validator bitmap,
BLS keys, signing domain, and CommitQC proof are interoperable. The observer
does not yet import gov5 execution blocks: gov5 does not implement Rust's
`/n42/sync/1` request-response protocol, and Reth's MPT execution genesis is
not the gov5 QMDB execution commitment.

The next interoperability step remains finalized-range/replay-v2 transport:
feed gov5 finalized block bundles plus portable QMDB checkpoint data into the
Rust QMDB verifier/importer, first for a bounded range and then for the existing
full replay data. Mixed validator voting stays disabled until that execution
and catch-up path is deterministic in both clients.
