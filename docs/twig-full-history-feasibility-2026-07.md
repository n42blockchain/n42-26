# P2-1: QMDB binary twig full-history feasibility

Date: 2026-07-18  
Scope: `docs/codex-task-sync-from-gov5-2026H1.md` P2-1

## Decision

**NO-GO for a production full-history layer now.** Keep the current QMDB-derived
binary twig as the default latest-state proof tree. Do not port gov5's death
stamps or change the root formula until a product requirement needs arbitrary
historical state proofs.

If mobile verification later needs an older state anchor, first ship a bounded,
header-anchored recent window and measure it. A full append-history migration is
only justified when callers require arbitrary-height state queries and accept a
one-time replay of the tree.

This document discusses the QMDB binary twig, **not an MPT**. The Rust tree is
the default state-proof sidecar exposed by `n42_twigRoot`/`n42_twigProof`; mobile
block replay remains receipts-root based.

## Current Rust behavior

`n42-twig-core` has 16 append-only shards, 2,048 leaf slots per twig, an active
flag per entry, a latest-key index, and a binary Merkle root. Updating a key
deactivates the old slot and appends a new slot. A deleted/inactive slot is a
null leaf in the current commitment.

The persistence format is optimized for latest-state restart, not history:

- `TwigSnapshot` preserves append-slot order and active flags;
- inactive entries keep their key/slot metadata but deliberately persist an
  empty value, so historical values are already discarded;
- snapshot plus WAL reconstructs the latest tree and latest proof only;
- `prove(key)` has no height argument, and neither `n42-twig-core` nor
  `PersistentTwig` has a `ProofAtHeight`/undo API.

This matches the present consumers. `n42-mobile::verifier` decodes the committed
header, re-executes the block from its read-log/witness, and returns a recomputed
receipts root. State-proof RPC tests fetch the current root and proof on demand;
they do not request an arbitrary old block.

## What the gov5 design changes

The relevant gov5 sequence is:

- `18dd833b4`: split each twig commitment into frozen `leafRoot` and mutable
  liveness bitmap commitment, with
  `twigRoot = hashNode(leafRoot, bitsRoot)` and
  `bitsRoot = BLAKE3(0x03 || activeBits)`;
- `f401d4d1b`: add a 4-byte death height per append slot, an 8-byte append
  cursor per block, per-key version positions, retained historical entries,
  top-band nodes, and `ProofAtHeight`;
- `a34b5d627`: persist the new tables and gate them behind `--qmdb-history`;
- `f387a7763`/`53f1e02fc`: exercise historical proofs against real-chain roots,
  including randomized membership and absence cases.

The frozen leaf tree is the important trick: leaf hashes do not need to be
rewritten when an old version dies; a height query derives liveness from death
stamps and authenticates the selected version against the corresponding bitmap
root.

## Rust adaptation cost

The minimum honest port is not just one `u32` field. It needs all of:

1. death height for every append slot (`4 * appended_entries` bytes before
   indexing/encoding overhead);
2. block height to append-cursor checkpoints (`8 * blocks` bytes);
3. key to all version positions, not only the latest-position index;
4. retention of dead values that the current snapshot intentionally erases;
5. frozen leaf-tree material and historical/top-band roots;
6. crash-consistent writes across snapshot, WAL, checkpoints, version index,
   and death stamps;
7. historical membership, temporal-absence, pure-absence, boundary, restart,
   and corruption tests.

The dominant storage term is workload-dependent dead value/version retention,
not the 4-byte stamp. n42-26 has no months-long production trace from which to
price it, so a capacity promise would be invented rather than measured.

## Is the root change destructive?

**Yes.** The current root authenticates each active leaf directly and turns an
inactive slot into a null leaf. The split formula authenticates the frozen leaf
tree and active bitmap separately. Even with identical keys, values, and append
order, the roots are different.

Consequences:

- existing snapshots cannot be reinterpreted as the new commitment;
- RPC/mobile roots and stored proof evidence cross a format boundary;
- all validators must switch at one deterministic activation height;
- historical values missing from old snapshots require replay from a trusted
  execution source; a format-only migration cannot recover them.

Therefore this cannot be enabled as a local optimization or rolled out one node
at a time.

## K-block mobile anchors

The task book's M4 model compares roughly 94.6 MB for independent per-block
state anchoring with roughly 2 MB for one anchor per 100 blocks. That saving is
plausible **for the state-proof/header-anchor layer**, because intermediate
headers can chain to one periodically proven state root.

It does not remove the current per-block verification packet: phones still need
transactions/read-log/code needed to re-execute and compare the receipts root.
Increasing `K` also delays an independently refreshed state anchor and requires
the phone to retain and verify the intervening header chain. It is therefore a
traffic/freshness trade, not a substitute for execution verification.

Recommended experiment if state anchors become necessary:

- start with `K=100` and a bounded recent window (at most one or two epochs);
- distribute proof bundles out of band but bind every bundle to committed
  headers;
- measure proof bytes, retained history bytes per block, lookup p50/p99,
  restart time, and detection delay;
- keep latest-per-block receipts replay unchanged.

## Revisit gate

Reopen full history only when all conditions hold:

- an API/mobile requirement names the required historical depth;
- a 30-day state-change trace prices dead values and version-index growth;
- an offline Rust prototype reproduces the old latest root before activation
  and the new reference root after activation;
- replay/migration, mixed-version rejection, corruption recovery, and rollback
  policy are specified;
- the activation is treated as a consensus-visible tree-format fork.

Until then, full history adds consensus and storage risk without serving a live
consumer.
