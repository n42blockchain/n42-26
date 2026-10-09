# QMDB read counter experiment

The harness copies the actual `qmdb_read_view.rs` and `qmdb_read_status.rs`
without edits. Its only node shim is the serialized mode enum. The twig core
is the current workspace crate; its source hash is in `experiment.json`.
This is an incremental counter experiment, not a complete provider/node benchmark.

Each sample performs 1,000,000 decoded account reads over 200,000 accounts,
checking the nonce sum and exact counter delta. Six pairs alternate counter
on/off order, for one and sixteen workers. Thread creation and joining are
inside the timer for both variants; tree setup and counter snapshots are outside.

The first run used unrestricted CPU affinity. Because its dispersion exceeded
the apparent counter cost, the same binary was rerun with process affinity
restricted to CPUs 0–15. Workers are not individually pinned and the host was
not isolated. All samples, including first rounds, remain in the summary.
No speedup should be inferred from a negative overhead estimate.

Reproduce from the repository root with the cached dependencies:

```bash
cargo test --release --locked --offline \
  --manifest-path docs/benchmarks/20260914-qmdb-read-status/Cargo.toml \
  bench_read_counter_overhead -- --ignored --nocapture
taskset -c 0-15 cargo test --release --locked --offline \
  --manifest-path docs/benchmarks/20260914-qmdb-read-status/Cargo.toml \
  bench_read_counter_overhead -- --ignored --nocapture
```

The saved runs invoked the built test executable directly, after compilation
finished. `experiment.json` includes the executable hash and source hashes.
The release build log records the original temporary source location; the
archived manifest changes only the twig dependency path for portability.

`node-tests.log`, `provider-rpc-test.log`, and `workspace-clippy.log` cover the
full node dependency graph in the fixed, patched reth workspace. `audit-tests.log`
uses a simulated transport to exercise strict qualification logic without sockets.
