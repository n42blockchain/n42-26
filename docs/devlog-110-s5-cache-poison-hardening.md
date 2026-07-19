# Devlog 110: S5 cache-poison hardening

Date: 2026-07-19
Branch: `fix/gov5-sync-p1-rework-20260719`

## Finding

A sync peer could poison both local rejection state and the execution cache for
an honest future block hash `H` without supplying the block committed by `H`. The peer declared
`block_hash = H`, supplied different payload contents and attached a forged
cached execution output under the same declared key. Reth recomputed the header
hash and returned `Invalid("block hash mismatch: ...")` before consuming that
execution bundle.

The old S5 policy treated every `Invalid` as deterministic evidence against
the declared key, so it marked `H` bad. The forged execution output also stayed
in reth's process-local payload cache. A later honest `H` was therefore either
filtered by `BadBlockCache` or paired with the forged bundle. One Byzantine
sync response could wedge recovery for that hash until restart.

## Fix

- `BadBlockCache::insert_if_invalid` recognizes reth's `PayloadError::BlockHash`
  text (`block hash mismatch: want ..., got ...`) and never records the
  sender-declared hash. Deterministic execution failures such as a state-root
  mismatch remain cacheable and preserve the bounded LRU behavior.
- `ExecutionOutputCache` now exposes `evict(hash)`. The node adapter removes the
  exact entry from reth's payload cache. Follower eager import, ordinary sync,
  syncing retry, committed background import, observer import and leader eager
  import all evict injected/cached output whenever `new_payload` is not
  `Valid`, including `Syncing`, `Accepted` and transport errors.
- A staged QMDB/Twig `StateDiff` is bound to the exact per-view hash that reth
  returned `Valid` for. A mismatch is discarded and converted into a missing
  sidecar barrier; execution rejection also discards the peer-supplied diff so
  a later honest payload cannot flush it.

## Additional acceptance cleanup

- sync/2 response construction measures the complete bincode response and
  truncates blocks/lineage before the 16 MiB codec limit instead of repeatedly
  attempting an unsendable frame;
- Twig's deterministic account reservoir reads at most 256 storage slots per
  sampled account;
- devlog-96 records consensus protocol v4, devlog-104 names the
  `collect_future_timeout` consensus break, and mobile evidence v2 documents
  that an older binary cannot reopen a datadir after v2 rows are written.

## Regression coverage

The adversarial mock-EL test first returns a real reth-style block-hash mismatch
for declared hash `H`, then `Valid` for the honest arrival. It asserts that `H`
never enters `BadBlockCache`, the injected output is evicted, the honest block
reaches `new_payload`, and the execution head advances to `H`. Separate tests
retain deterministic-invalid caching, reject a sidecar diff whose staged hash
does not match the execution-valid hash, enforce the 16 MiB lineage budget and
cap Twig probe slots.

## Validation

- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo check --workspace --all-targets`
- Scenario 9 long-run crash/recovery: 7/7 checks passed; all three nodes ended
  at height 226 and the recovered node caught up 79 blocks.
- Scenario 10 seven-node chaos: 10/10 checks passed; the network stalled at
  4/7, resumed after recovery, and all seven nodes ended at height 143 with
  identical sampled block hashes.
