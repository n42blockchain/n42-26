# Read-index experiment

This is an archived standalone harness, separate from the node workspace. It
compiles the actual baseline/candidate read-view source snapshots against the
repository's QMDB tree. It does not benchmark node or fleet TPS.

`summary.json` and `results.json` contain six alternating baseline/candidate
pairs for each of four shapes. Each sample ran in a fresh test process. Raw logs
are named `<keys>-<kind>-<pair>-<version>.log`. RSS is the entire test process's
maximum, including its QMDB tree, input and two views. The earlier 15-byte-inline
experiment is retained separately; it is not the final candidate.

Build the archived harness from the repository root:

```bash
cargo test --manifest-path docs/benchmarks/20260914-qmdb-read-shards/Cargo.toml \
  --locked --offline --release --no-run
```

Cargo prints the test executable path. Pass that exact path to the runner:

```bash
python3 docs/benchmarks/20260914-qmdb-read-shards/run.py \
  --binary /absolute/path/to/the/test-executable \
  --output /tmp/qmdb-read-comparison
```

The runner uses `/usr/bin/time`, executes only the ignored benchmark, verifies
each exit status, and records the executable hash and per-sample metrics. Build
before running; avoid compiling or replacing the executable during a run.

The retained intermediate modules match the original harness, but the final
runner selects only `baseline` and `qmdb_read_view`. Building at another path
can change the binary hash. `experiment.json` records the source hashes and
host/measurement boundaries; the repository tree source must match its recorded
hash to reproduce this experiment's inputs. Exact timings depend on the host.

To exercise the current node source in its normal dependency graph instead:

```bash
N42_READ_BENCH_KEYS=2000000 N42_READ_BENCH_KIND=storage \
  cargo test --release --locked --offline -p n42-node --lib \
  bench_immutable_read_views -- --ignored --nocapture --test-threads=1
```
