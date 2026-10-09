# QMDB dirty-twig marking and upper folding — 2026-09-14

This measures a single process applying synthetic, real-encoded Gov5 account
updates. It is not fleet TPS. All timed variants include the same empty-tree
cache correctness fix; `baseline-qmdb_leaf_tree.rs` preserves the original bug.

- `before/`: BTreeSet marking, individual ancestor paths.
- `batch/`: BTreeSet marking, shared ancestors folded once.
- `after/`: bitmap membership, reusable ID buffer, shared ancestor folding.

Build before timing, using the same target directory for the commands below:

```sh
CARGO_TARGET_DIR="$PWD/target" cargo build --manifest-path \
  docs/benchmarks/20260914-qmdb-dirty-twigs/Cargo.toml --locked --offline --release
python3 docs/benchmarks/20260914-qmdb-dirty-twigs/run.py \
  --binary "$PWD/target/release/dirty-twigs" --output /tmp/n42-dirty-reproduction \
  --pairs 6 --blocks 3 --keys 200000 2000000 --ordered 0
```

The runner rotates three process orders; every variant includes identical caller
sorting and updates 147,000 accounts per block. Root and complete undo hashes
must match across all corresponding samples. Each process verifies rollback,
cursor restoration, reapplication and all updated values outside timing.

`dirty-twigs-allocations` is a separate instrumented binary; its times must not
be included in timing statistics. Set `N42_UPDATE_VERSION=before|batch|after`,
`N42_UPDATE_KEYS`, `N42_UPDATE_BLOCKS=3` and `N42_UPDATE_SORTED=0` to reproduce its
allocation logs. Counts are cumulative allocation requests, not live memory.

`baseline-empty-regression.log` records the new empty-rewind/refill test failing
against the original source. `core-tests.log` records it passing with the final
implementation, alongside the complete core suite. The regression's source is
`empty_rewind_then_same_capacity_refill_matches_full_tree` in the archived final
file. Run the integrated tests in the pinned patched reth tree described in
devlog 161; this standalone timing workspace omits the core test dependencies.

The full-state oracle, snapshot/proof and node WAL/read-view tests are separate
from the timing binary. No EVM, WAL, read-view construction, network or H2 work
is counted by these timings. No CPU affinity or exclusive host was used.
