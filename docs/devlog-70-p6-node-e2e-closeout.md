# Devlog 70: P6 Node E2E Closeout and Slot Profiling Status

Date: 2026-06-13
Branch: `chore/merge-reth-main-deps-upgrade`

## Scope

This records the P6 node-level E2E results that are already on the linear branch, and
separates them from the still-open real-node slot profiling task. It intentionally does
not report guessed slot-stage numbers: the real 8s slot breakdown requires a Linux
testnet and leader-process sampling.

Relevant commits:

- `cd35a1a` - Twig node E2E, RPC proof, and WAL recovery coverage.
- `a8b50e1` - Consensus leader-payload cache protection for deferred finalization.

## Step 5: Fresh Testnet Cross-Node Twig Root

Status: complete.

Environment:

- Fresh local 4-node testnet data dir.
- `N42_TWIG=1`
- `N42_TWIG_SNAPSHOT_INTERVAL=2`
- `N42_ENABLE_MDNS=0`

Results:

| Check | Result |
|-------|--------|
| Fresh genesis root | all four validators reported `0x3d722f7c3ab136688a8a07c3a2e7b917ee3c3bff5257d3e058b5df83019207f6` |
| Real transfer | tx `0x30c6111c61da697d441f2c196b1b8426518e2da923f99a7f933b047c71fb4e52` succeeded in block `0x48` |
| Post-transfer root | all four validators reported `0xcd436390d268b94d7b6cbcb66a870e6862a342a0e36590b02ef789d758f2f78d` |
| RPC proof | `n42_twigProof` proof/root verified through `n42_verify_twig_account_proof` |

The run used a fresh genesis rather than reusing prior chain data. The local testnet
script was also hardened for the mac/reth 2.3 setup by assigning discovery v5 ports
separately from p2p/v4 ports and allowing `N42_ENABLE_MDNS` to be overridden.

## Step 6: Crash Recovery

Status: complete, covered by `cd35a1a`.

The recovery check killed validator-1 with `kill -9` during a snapshot gap:

| Item | Value |
|------|-------|
| Kill-time version | `531` |
| Kill-time root | `0xcd436390d268b94d7b6cbcb66a870e6862a342a0e36590b02ef789d758f2f78d` |
| On-disk snapshot before restart | `twig.snapshot` version `530` |
| WAL before restart | `twig.wal` size `20` bytes |
| Restart RPC result | version `531`, same root |
| Restart log | `Twig restored from snapshot/WAL` |

This covers the process-crash recovery path: snapshot plus unsnapshotted WAL replay
restores the exact committed Twig version and root.

## Step 7: Real 8s Slot Breakdown

Status: not run in this environment.

Current execution environment is macOS/Darwin. The real slot profiling task requires a
Linux testnet with at least four validators, mobile simulation, and leader-process
sampling with `perf`/`samply` to produce an accountable flamegraph. Therefore no
EVM/state-root/BLS/consensus/network millisecond or percentage breakdown is reported
from this machine.

The still-open task is `docs/task-real-slot-measurement.md`. Its required output remains:

- real testnet data, not CacheDB microbenchmarks;
- key-path versus background timing;
- EVM / state-access / state-root / BLS verify / consensus / network breakdown;
- leader flamegraph from the contract-heavy workload;
- recommendations based on measured bottlenecks.

## Consensus #5: Leader Drain Finalization

Status: complete in `a8b50e1`.

The consensus fix protects the block currently being finalized, and any active
`pending_finalization` hash, while inserting leader/follower block data into the bounded
`pending_block_data` cache. This closes the BTreeMap ordered-eviction race where the
smallest hash could be the exact leader payload needed for deferred finalization.

Validation:

- `cargo fmt --package n42-node --check`
- `cargo test -p n42-node test_cache_pending_block_data_preserves_protected_hash_when_full`
- `cargo test -p n42-node test_leader_payload_drain_preserves_finalizing_hash_when_full`
- `git diff --check`
- `cargo clippy -p n42-node --lib -- -D warnings`
