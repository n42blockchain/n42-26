# QMDB recorded-update value transfer

The `before` and `after` directories contain complete core source snapshots.
Only `qmdb_leaf_tree.rs::set_inner` differs: the new path moves the replaced
entry's value into undo instead of copying it before replacing the entry.
Their standalone manifests retain the needed production dependencies. The
root workspace manifest and lock file were not changed for this experiment.

Build both experiment binaries with the archived dependency lock:

```sh
CARGO_TARGET_DIR=/tmp/n42-value-transfer-archive-target CARGO_BUILD_JOBS=2 \
  cargo build --release --locked --offline --manifest-path \
  docs/benchmarks/20260914-qmdb-value-transfer/Cargo.toml --bins
```

`main.rs` measures the actual recorded tree update, including the caller's
cloned iterator and sorting. It verifies rollback, cursor, reapplied root and
all updated values outside timing. To run one configuration:

```sh
N42_UPDATE_VERSION=after N42_UPDATE_KEYS=2000000 N42_UPDATE_BLOCKS=3 \
  /tmp/n42-value-transfer-archive-target/release/value-transfer
```

Use `before` for the control. The archived `run.py` executes six alternating
pairs for each state size and checks roots and serialized undo digests across
all samples. It expects a binary at `target/release/value-transfer` alongside
the script and writes results beside itself; copy the harness to a scratch
directory before rerunning it so the original evidence is preserved.

`counted.rs` adds a forwarding System allocator wrapper. Run the
`value-transfer-allocations` binary with the same environment to count allocation
requests and cumulative requested bytes. Its instrumented timings are excluded
from `summary.json`. Allocation requests include reallocations; they are not
live allocations, RSS or a memory limit.

The original timing dependency lock is `timing-Cargo.lock`. Moving the core
snapshot into this standalone workspace adds its optional `rayon` dependency
edge to the archived `Cargo.lock`; versions and default feature selection are
unchanged. The original binary hashes and source/log hashes are in the evidence.

This benchmark excludes WAL, read views, EVM and H2. Tests at the full node
dependency graph are recorded separately. No CPU affinity or host isolation
was used; node test compilation/execution overlapped the beginning of timing.
