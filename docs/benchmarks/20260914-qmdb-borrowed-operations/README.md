# Borrowed QMDB operations

`before` and `after` preserve complete core sources. `node-source` records the
integration: owned operation buffers are sorted by the builder/root job before
the store lock; the store borrows them during tree application. Unsorted input
uses the original owned sorting path, including duplicate rejection.

Build from the repository root, then run into a fresh output directory:

```sh
CARGO_TARGET_DIR=/tmp/n42-borrowed-ops-archive-target CARGO_BUILD_JOBS=2 \
  cargo build --release --locked --offline --manifest-path \
  docs/benchmarks/20260914-qmdb-borrowed-operations/Cargo.toml --bins
python3 docs/benchmarks/20260914-qmdb-borrowed-operations/run.py \
  --binary /tmp/n42-borrowed-ops-archive-target/release/borrowed-ops \
  --output /tmp/n42-borrowed-ops-rerun
```

The runner alternates six process pairs at each state size/input order, with
three recorded updates per process. New-path timings include the caller's
in-place sort. `--ordered 1` supplies already ordered input to both versions;
`--ordered 0` supplies the deterministic hashed-address order. Input preparation,
initial tree, rollback checks and reapplication are outside timing. Roots and
serialized undo digests must match for each state size/generation.

`borrowed-ops-allocations` forwards to System through a counting allocator.
Run it with `N42_UPDATE_VERSION=before|after`, `N42_UPDATE_KEYS=200000`,
`N42_UPDATE_BLOCKS=3`, `N42_UPDATE_SORTED=0|1`. These runs count allocation and
reallocation requests and cumulative requested bytes, not RSS. Their timings
are excluded from the summary.

`rejected-reference-sort` preserves the earlier, slower reference-sorting
implementation and its experiment sources/results. To reproduce it, copy this
harness to a scratch directory, replace `after/src/qmdb_leaf_tree.rs`, `main.rs`
and `counted.rs` with the rejected copies, and rebuild. Its timings do not
represent the final implementation.

`timing-Cargo.lock` is the original experiment lock. The standalone archive lock
adds the optional rayon dependency edge for the relocated core member; package
versions and default feature selection are unchanged. No repository root lock
or manifest changes were made for this experiment.

The final run started after this turn's tests/builds completed, without CPU
affinity or host isolation. It measures tree updates, not persistence, EVM,
read-view derivation, network traffic or consensus. Full-node verification logs
are separate from this standalone release dependency graph.
