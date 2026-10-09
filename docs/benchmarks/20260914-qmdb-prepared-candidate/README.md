# Prepared QMDB candidate comparison — 2026-09-14

Synthetic account workload, real persistent store and fsync. This is not EVM or
four-node TPS. `discard` uses the production ephemeral candidate API; `retain`
uses the production owned prepared-candidate API. Both then execute the same
durable commit path with immutable read views enabled.

The harness contains production store definitions and selected tests verbatim,
with only imports adapted to omit unrelated reth engine integration. The full
node source and normalization caller are archived. `snapshot.py` reproduces this
extraction; it also refreshes the full source snapshot, so do not run it over
evidence you want to preserve after changing production code.

```sh
CARGO_TARGET_DIR="$PWD/target" cargo test --manifest-path \
  docs/benchmarks/20260914-qmdb-prepared-candidate/Cargo.toml \
  --locked --offline --release --no-run
```

Use the exact test executable path printed by Cargo:

```sh
python3 docs/benchmarks/20260914-qmdb-prepared-candidate/run.py \
  --binary /absolute/path/to/the/test-executable \
  --out /tmp/n42-prepared-reproduction \
  --tmpdir "$PWD/target/qmdb-prepared-temp" \
  --pairs 6 --blocks 3 --keys 200000 2000000
```

Choose the temporary WAL filesystem deliberately: the recorded run used ext4;
`/tmp` is tmpfs here. Initial trees, operation generation/copy, independent root
calculation and restart verification are outside timing. Both sorts, builder
operation release, candidate processing, commit, read-view updates, WAL encoding,
write and fsync are timed. Each retained sample asserts the candidate fits the
64 MiB retention accounting and was actually retained.

72 sample roots and WAL lengths match; every process reopens its store and checks
the final root and a QMDB account nonce. No CPU affinity or exclusive host was used.
The source/lock hashes and timing executable hash are recorded. The harness's
standalone dependency graph differs from the integrated node dependency graph;
the node tests and workspace clippy cover integration separately.

`core-source/` preserves the exact twig-core sources used. The manifest references
the repository's twig-core crate, so reproduce against that snapshot in an isolated
checkout if current core code has changed. No production private keys or live RPC
traffic were used, and real fleet cache-hit rate remains unmeasured.
