# Follower Execution Validation Modes

**Status:** approved target architecture; implementation gaps are listed below.

## Canonical terminology

- The N42 state tree is the QMDB-style binary twig tree implemented by
  `n42-twig-core`. `N42_TWIG` is enabled by default unless the reserve SBMT path
  is explicitly selected.
- The homomorphic state digest is **LtHash** (the spelling used by
  `N42-gov5/lib/lthash`). LtHash is a 2048-byte lattice accumulator with a
  32-byte BLAKE3 summary. It complements the QMDB tree; it is not itself a
  Merkle tree and cannot serve membership proofs.
- “Full replay” means executing every transaction independently on a follower.
  Applying and checking a leader-provided execution diff is not full replay.

The target protocol must not describe its state commitment as MPT. Ethereum
compatibility code may retain a legacy execution-root adapter during migration,
but that adapter is not the target N42 state-tree design.

## Default: cache-hit plus QMDB/LtHash verification

For a seven-validator deployment, the normal production path is:

1. The leader executes the block's transactions once and produces the payload,
   receipts, senders, execution output, and canonical state diff.
2. The signed proposal commits to the transaction/receipt data, the execution
   output, the resulting QMDB binary root, and the LtHash summary.
3. Each of the six followers authenticates the proposal and injects the compact
   execution output into `payload_cache`; it does not independently run all
   transactions through the EVM.
4. Each follower derives the canonical state diff from the cached output,
   updates its local QMDB tree and LtHash accumulator in committed-view order,
   and compares both commitments with the proposal/header commitments.
5. A mismatch is `Invalid`: the node must not vote for the block, advance its
   execution-validated head, apply later state-tree diffs, or build a child.

A cache hit therefore means “skip duplicate EVM work”, not “trust the leader”.
All validators still receive the same transactions/header, participate in
HotStuff-2, import the committed payload, persist state, and verify the compact
transition commitments.

## Optional: independent full replay

The audit/diagnostic mode is:

```text
N42_FOLLOWER_VALIDATION=full-replay
```

In this mode a follower ignores the received execution-output cache entry,
independently executes every transaction, rebuilds the same QMDB state diff and
binary root, updates LtHash, and compares all committed roots. On a seven-node
testnet this means six independent replays of a 90K-transaction block.

The production default is intended to be:

```text
N42_FOLLOWER_VALIDATION=cache-lthash
```

The mode must be homogeneous across a production validator set and exposed in
startup logs and metrics. Full replay is a correctness control and incident
diagnostic, not the throughput baseline.

## Current Rust status (2026-07-12)

| Component | Status |
|---|---|
| Leader execution-output serialization and follower `payload_cache` injection | Implemented; enabled by default through `N42_COMPACT_BLOCK` |
| QMDB-style binary twig proof backend | Implemented; `N42_TWIG` defaults on |
| Apply sidecar diffs only after reth accepts execution, in committed-view order | Implemented by PR #22 |
| Refuse to advance/build on an execution-invalid committed block | Implemented by PR #21/#22 |
| Rust LtHash accumulator with Go byte-for-byte vectors | **Missing** |
| QMDB root + LtHash summary committed by the proposal/header | **Missing** |
| Atomic speculative preview/rollback for leader QMDB/LtHash commitments | **Missing** |
| Explicit `cache-lthash` / `full-replay` configuration | **Missing**; `N42_COMPACT_BLOCK=0` only provides an indirect legacy full-EVM path |
| Seven-node parity test for both modes | **Missing** |

Until the missing rows land, documentation and benchmarks must not claim that
the Rust cache-hit path already verifies a QMDB/LtHash header commitment.

## Follow-up implementation acceptance

1. Port `N42-gov5/lib/lthash` to Rust with identical domain encoding, 2048-byte
   digests, BLAKE3 summaries, persistence, and cross-language golden vectors.
2. Define fork-gated proposal/header commitment fields for the execution-output
   hash, QMDB root, and LtHash summary. Old peers must fail closed after the
   activation point.
3. Add a non-mutating QMDB/LtHash preview or a reversible overlay so the leader
   can commit the post-state without advancing the canonical tree before
   execution acceptance and consensus commit.
4. Implement the two explicit follower modes. `cache-lthash` is the default;
   `full-replay` must bypass cache injection and independently execute the block.
5. Treat missing or mismatched commitments as `Invalid`, with an error log and
   labeled metrics. Never silently fall back from full replay to cache trust.
6. Add deterministic invalid-output tests plus a seven-node test that proves all
   validators converge on block hash, QMDB root, LtHash summary, receipts root,
   balances, and storage in both modes.
7. Keep `N42_SKIP_STATE_ROOT` and `N42_DEFER_STATE_ROOT` benchmark-only. They are
   not substitutes for either production validation mode.

