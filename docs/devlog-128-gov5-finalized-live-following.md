# Devlog 128 — Gov5 finalized live following

Date: 2026-07-22

## Outcome

The Rust observer can now continue from an authenticated Gov5 replay-v2/QMDB checkpoint and follow a running seven-validator Gov5 H2 chain. The observer obtains Gov5-native block bodies, executes them through Reth, and advances canonical/safe/finalized head only after independently verifying the matching H2-v4 Decide/CommitQC.

This is an observer/follower milestone. It does not enable the Rust node to propose or vote in the Gov5 validator set.

## Compatibility path

- Subscribe to Gov5's genesis-scoped `/n42/<fork-digest>/block/ssz_snappy` block topic.
- Accept Gov5's reliable `/rpc/block_push/1/ssz_snappy` inbound stream.
- Fetch missed ancestors with `/rpc/block_by_hash/1/ssz_snappy` from connected Gov5 peers.
- Decode the exact Gov5 ETH-style RLP block form and enforce canonical framing, the interop header profile, transaction decoding, transaction-root equality, and reconstructed block-hash equality.
- Execute candidate bodies with `new_payload`; an unauthenticated body can never move forkchoice.
- Verify the H2-v4 Decide against the configured validator set, chain identity, validator-changes hash, signing domains, and CommitQC quorum.
- Promote only an executed, parent-linked, block-number-contiguous lineage ending at the hash authenticated by the Decide.

Pending bodies, finality records, and fetch bookkeeping are bounded to 512 entries. Fetch responses are bound to the requested hash; malformed or substituted responses are retried through another connected peer.

## Live evidence

The test used copies of the replay data and validator configuration. No existing seven-node runtime, replay history, or performance dataset was modified.

Starting anchor:

- replay-v2 block: 49
- block hash: `765e220ce23ff55715d75bb5d51d5e193d189c6bf88e6f4d29e7b00063cd93f8`
- QMDB root: `6fb33357c8db5eb206f506af271cf5fff885fc11bbd82b405b74a42943c98314`

A fresh seven-validator Gov5 process group resumed from that anchor. A fresh Rust observer connected to all seven peer IDs, received the finalized descendant at block 55, fetched and executed the missing parent lineage 54 through 50, verified the block-55 H2-v4 Decide at view 8, and promoted the six-block lineage. It then followed steady-state finalized blocks through block 60/view 13. Gov5 JSON-RPC and Rust/Reth JSON-RPC both reported block `0x3c` (60), while the observer reported `local_view=13`, `highest_seen=13`, and 11 imported live blocks.

An earlier soak of the same path continued from block 49 through block 142. The shorter clean run above is the reproducible acceptance record.

## Safety boundary and remaining work

The body transports are availability mechanisms, not finality authorities. A peer-provided body is strictly hash/commitment checked and may be pre-executed, but only a verified H2-v4 Decide can update forkchoice. A finalized descendant authenticates its entire exact parent-hash lineage; block numbers must also be contiguous.

The existing seven-node deployment can therefore admit the Rust implementation as a non-voting follower once the observer is configured with all trusted peer multiaddrs and the matching replay-v2 checkpoint. Validator participation remains gated on a shared proposal/vote/new-view transport, identical membership/key lifecycle, and mixed-client fault/partition tests. Full archive+ parity additionally needs historical QMDB state/proof serving beyond the live head and retained replay range.
