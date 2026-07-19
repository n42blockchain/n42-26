# P2-2: reth hot/cold archive assessment

Date: 2026-07-18  
Scope: `docs/codex-task-sync-from-gov5-2026H1.md` P2-2

## Decision

**Do not port gov5's custom three-tier archive into n42-26 now.** Use reth static
files and pruning as the storage policy surface, keep at least one separately
operated archive role, and first collect a 30-day disk-growth trace from the
actual n42 workload.

A seven-day warm window is a useful measurement starting point, not a production
constant. The required window must be derived from reorg/repair operations and
RPC service levels. Current mobile verification needs latest/recent committed
data, not arbitrary historical state proofs.

## What gov5 actually measured

gov5's Go/MDBX archive separates:

- snapshot: current accounts, storage, and code;
- history: per-key old-value timelines indexed by RecSplit MPHF plus a 4-byte
  fingerprint;
- warm changesets: a rolling unwind window;
- BLAKE3 manifests: per-file and per-64MiB-segment trust anchors.

On its 25.1M-block source, the final measured **state-only** archive was
169.13GB versus 945GB raw (5.59x smaller). Storage-history entries fell from
18.41 bytes to 9.69 bytes with MPHF+fingerprint in the measured format. These are
real results for that dataset and implementation, not forecasts for Rust/reth.

The task book repeats the early `397GB -> 820MB` warm-CS estimate. The final
real-data run superseded it: 50,400 late-chain blocks occupied **2.44GB**
(825MB account CS + 1.64GB storage CS), built in 64 seconds and passed 10,000
sample comparisons. The gov5 source itself records that the 820MB dry-run was
low by roughly 3x because late-chain changes were denser. This assessment uses
the final measured number.

## n42-26's existing storage layers

n42-26 links the local n42 reth fork and inherits its storage model:

- immutable static-file segments for headers, transactions, receipts, recovered
  senders, account changesets, and storage changesets;
- configurable blocks-per-file per segment;
- archive/full/minimal pruning profiles plus explicit distance/before/full
  controls for sender recovery, transaction lookup, receipts, account history,
  storage history, and bodies;
- a minimum pruning distance protecting reorg/manual unwind operations.

The current reth fork sets 10,000 blocks per file for minimal mode so old
history can be retired at that granularity. This already supplies the policy
mechanism that gov5's warm-CS freezer added to its different database stack.
Porting the Go tools would duplicate lifecycle code without making the file
formats or readers compatible.

The QMDB binary twig is a separate default state-proof sidecar. Its snapshot/WAL
contains the append history (including dead slot metadata, with dead values
emptied). Snapshot size and reload work therefore grow with appended versions.
Its deterministic compaction changes the root and cannot be run as a casual
per-node disk prune. It must be measured separately from reth MDBX/static files.

## What is not yet known

There is no months-long n42-26 production trace that breaks growth down by:

- reth MDBX tables;
- each static-file segment;
- QMDB twig snapshot and WAL;
- mobile verification packets/evidence;
- payload-cache material;
- logs, crash dumps, and temporary build artifacts.

The 90K-transaction and batch-fast-lane tests are capacity experiments, not a
continuous-chain storage trace. In particular, the batch fast lane explicitly
skips production reth execution, state roots, receipts, and MDBX persistence.
Multiplying its peak TPS by a month would not estimate real disk growth.

Follower payload-cache import also does not imply “no storage”: followers still
import the leader's authenticated execution output into their local canonical
database. The optional independent full replay changes validation CPU, not the
basic retention question.

## Mobile history depth

The live mobile verifier needs the committed header, transactions/read-log,
required code, and expected receipts root for the block being verified. It does
not call `ProofAtHeight`. Latest state proofs are available separately through
the QMDB proof RPC.

Current evidence stores are intentionally bounded (for example, recent
attestation history is capped at 1,000). Therefore mobile service alone does not
justify full state history. A practical service target is:

- latest block plus resend/reconnect margin for verification packets;
- the operator-selected recent header/evidence window;
- optional periodic state anchors as described in P2-1;
- archive retrieval for older explorer/audit requests, not validator-local hot
  storage.

## Recommended node roles

| Role | Hot local data | Cold/full data | Notes |
|---|---|---|---|
| Validator / IDC | current state, QMDB latest tree, static segments and changesets covering the repair/unwind window | no arbitrary state history requirement | Never prune inside the proven operational recovery window |
| Mobile gateway | latest/recent packets, code cache, bounded attestations and header anchors | fetch old evidence from archive if policy requires | Mobile replay remains per-block receipts-root validation |
| RPC/explorer full node | policy-selected recent bodies, receipts, logs, tx lookup and state history | cold static files or archive service for older queries | Publish the earliest available height accurately |
| Archive node | full bodies/receipts/history plus manifests/backups | replicated/off-site copy | At least one independent operator; test restoration, not just retention |

Start an experiment with 7 and 30-day candidate windows, but select the final
distance only after replaying the longest supported repair/unwind and measuring
RPC misses.

## Required 30-day measurement

Sample at a fixed UTC time and after each static-file/prune cycle:

1. canonical head, block count, transaction count, and changed account/storage
   counts for the interval;
2. allocated and logical bytes for MDBX, each static segment, QMDB snapshot/WAL,
   and mobile/evidence stores;
3. daily delta, bytes/block, bytes/transaction, and bytes/changed-slot;
4. prune/segment build wall time, write amplification, peak temporary space,
   restart/recovery time, and oldest queryable height;
5. seven-day and thirty-day projections with p50/p95 daily growth, not one peak
   benchmark extrapolation.

Acceptance for changing retention policy:

- two successful prune-and-restart drills;
- one deep unwind/repair drill at the proposed boundary;
- archive restore plus manifest verification;
- mobile resend/reconnect and explorer historical-query tests at both sides of
  the boundary;
- disk alert thresholds that include temporary compaction/build headroom.

## Revisit gate for a custom archive

Only consider a gov5-style custom history index if the trace shows reth's
static/prune formats miss a required capability, and a prototype demonstrates a
material end-to-end win including build time, temporary space, verification,
and restore. Until then the existing reth machinery has lower integration and
correctness risk.
