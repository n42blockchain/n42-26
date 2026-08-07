# Devlog 131 — Gov5 finalized far catch-up

Date: 2026-07-22

## Outcome

The Rust observer can now join a Gov5 seven-validator network from the
authenticated replay-v2/QMDB block-49 checkpoint even when the finalized head
is farther away than the 512-body live cache. It discovers a Decide-authenticated
lineage backwards with exact hash fetches, then executes and finalizes it in
forward order without increasing the body-cache bound.

This closes the remaining admission gate for attaching `n42-26` as a
non-voting follower. Validator proposal and voting interoperability remain a
separate milestone.

## Safety and memory boundary

- A verified H2-v4 Decide supplies the finalized tip hash. The catch-up target
  is latched until completion so newer finality cannot starve an older proof.
- Every response must match the exact requested hash. Reconstructed block hash,
  parent hash, and contiguous block number extend the authenticated lineage one
  edge at a time.
- Cycles, substitutions, discontinuities, and lineages longer than 65,536
  entries fail closed.
- Discovery is metadata-only. Bodies are not submitted to Reth while their
  parent chain is incomplete, avoiding quadratic repeated `new_payload` work.
- Only after the lineage reaches the current local head are blocks executed and
  promoted in forward order. Each promoted block is already known to be an
  ancestor of the verified Decide tip.
- The body cache remains hard-bounded at 512. While a finalized lineage is being
  executed, cached pending ancestors take eviction priority over unrelated live
  tip gossip; consumed bodies are removed immediately and later ancestors are
  fetched on demand.

## Regression coverage

Five focused tests cover:

1. a 1,025-block lineage, larger than the body window;
2. requested-hash binding;
3. rejection of non-contiguous block numbers;
4. execution of one complete 512-body window followed by an exact forward
   refetch;
5. protection of pending finalized bodies from unrelated live-gossip eviction.

## Live seven-node evidence

All nodes and databases were copy-on-write temporary copies. The existing
seven-node runtimes, replay history, and performance datasets remained
untouched.

Starting anchor:

- block: 49;
- hash: `765e220ce23ff55715d75bb5d51d5e193d189c6bf88e6f4d29e7b00063cd93f8`;
- QMDB root: `6fb33357c8db5eb206f506af271cf5fff885fc11bbd82b405b74a42943c98314`.

The fresh Rust/Reth observer connected to all seven Gov5 peer IDs and verified
a distant finalized lineage. Its first completed proof imported 2,883 blocks
in one authenticated catch-up, reaching block 2,932 at H2 view 2,888 with tip
hash `6af30ae7ee24b675a9576a1a1a28e00a64ddddac2decaf098a40902d6a0be34e`.
It then transitioned automatically to one-block finalized tracking.

At the final comparison both Gov5 JSON-RPC and Rust/Reth JSON-RPC reported:

- block: 3,194 (`0xc7a`);
- hash: `e048ae8499ec73b953249fde74dd49ea6da97b12acd7e25949a73176151ff87a`;
- parent: `c463b07d26fccb1924d2ff12afa5f929ee8a343a8a7da7ca74bd4140c0121f3d`;
- state root: `6fb33357c8db5eb206f506af271cf5fff885fc11bbd82b405b74a42943c98314`.

Observer status had entered `tracking` with `local_view == highest_seen`, seven
connected peers, and zero H2-v4 shadow rejections. The historical fixture has a
difficulty-zero replay anchor and legacy difficulty-one descendants; the
authenticated header compatibility path accepts both. Current Gov5
difficulty-zero production remains covered by the earlier live-following run.

## Remaining interoperability distance

The existing deployment can now attach a fresh Rust node as a non-voting,
finalized follower using the matching replay-v2 checkpoint and trusted peer
multiaddrs. Participation in consensus still requires Gov5-compatible outbound
proposal/vote/new-view production, identical validator membership and key
lifecycle, and mixed-client fault/partition testing. Minimal full archive+
also still needs historical QMDB state/proof serving beyond the retained replay
range.
