# Persistent QMDB commit comparison

The archived harness compiles the original store definitions and tests, without
the unrelated reth adapter. `baseline-full.rs` and `optimized-full.rs` preserve
the complete source files; `baseline.rs` and `optimized.rs` contain the same store
definitions and tests verbatim, with only imports adapted. The directory-sync
helper is copied verbatim from the production helper and performs the same
filesystem calls. `qmdb-read-view.rs` is shared by both versions.

This measures store commit cost, not EVM or H2 throughput. Initial state, oracle
root calculation and restart verification are outside timing. Timed work includes
tree mutations, optional read-view derivation, frame encoding, WAL write and sync.

Build from the repository root:

```bash
cargo test --manifest-path docs/benchmarks/20260914-qmdb-single-apply/Cargo.toml \
  --locked --offline --release --no-run
```

Use the exact executable path printed by Cargo:

```bash
python3 docs/benchmarks/20260914-qmdb-single-apply/run.py \
  --binary /absolute/path/to/the/test-executable \
  --tmpdir "$PWD/target/qmdb-commit-temp" \
  --output /tmp/qmdb-commit-comparison --keys 2000000 --reads 1
```

Choose the WAL directory deliberately: `/tmp` was tmpfs in the recorded run,
while the repository workspace was ext4. The test creates and removes its own
temporary child directory. Build before measuring and keep the binary unchanged
throughout the run. Six alternating pairs and three blocks per process are the
defaults. The runner checks configuration, exit status, recovery confirmation,
and agreement of roots and WAL lengths for every generation.

`tmpfs-200k`, `ext4-200k` and `ext4-2m` contain the original per-process logs and
results. `experiment.json` records hashes, environment boundaries and upstream
references. The standalone dependency graph is not the full node graph; the
node tests and workspace clippy logs cover the integrated implementation.
