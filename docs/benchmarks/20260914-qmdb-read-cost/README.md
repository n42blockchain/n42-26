# Read-view cost experiment

See [devlog-163](../../devlog-163-qmdb-read-cost-20260914.md) for results and limits.
No experimental index variant was adopted in production.

`production-store.rs` and `production-read-view.rs` freeze the production inputs.
`store.rs` is the extracted store plus six prepared-candidate regression tests,
the persistent benchmark, a local metrics recorder, and WAL-encoding timing.
`variants.py --check` verifies that every read-index variant is exactly the
documented transformation of the frozen production reader. `core-source` freezes
the unchanged core library; its manifest is reduced to standalone dependencies.

Build and verify (from this directory):

```sh
python3 variants.py --check
CARGO_BUILD_JOBS=2 cargo test --release --locked --offline
```

Use the test binary reported by Cargo as `BIN`:

```sh
python3 run-views.py --binary "$BIN" --out rerun-account --pairs 6 --variants qmdb hash4096
python3 run-views.py --binary "$BIN" --out rerun-storage --pairs 6 --variants qmdb hash4096 --kind storage
python3 run-views.py --binary "$BIN" --out rerun-sparse --pairs 3 --variants qmdb hash4096 --updates 1
```

`shard-account` and `shard-storage` contain the 48 repeated dense samples.
`sparse-one` and `sparse-thousand` contain 24 additional samples. `pilot`,
`path-pilot`, and `shard-pilot` are exploratory single-process-per-variant runs.
Each `run.json` records the binary hash used for that run; variants were added
between pilot runs, so hashes differ. The runner verifies that the binary does
not change during a run. Each timing also measures 1M decoded reads, including
key hashing, with one and 16 threads. It excludes tree construction and checking.

The persistent profile additionally includes the real tree and durable WAL:

```sh
TMPDIR=/path/on/ext4 N42_COMMIT_BENCH_KEYS=2000000 \
N42_COMMIT_BENCH_BLOCKS=3 N42_COMMIT_BENCH_READS=1 \
N42_COMMIT_BENCH_PREPARATION=retain "$BIN" \
store::tests::bench_persistent_commit_path --ignored --exact --nocapture --test-threads=1
```

`profile.log`/`profile.json` are the repeated diagnostic with recorded binary
SHA and successful restart check. `initial-profile.log` is the initial diagnostic
whose binary SHA was not captured. Both use the original 64-shard reader.

Measurements used the original workspace core dependency path. The final bundle
uses identical frozen library source for independent reproduction. The original
lock is `measured.Cargo.lock`; the standalone lock only adds the optional `rayon`
dependency to the core package entry, with no dependency version changes. The
frozen source build was separately tested; it is not the recorded measured binary.
`production-hashes.json` records production inputs, and `core-hashes.json` records
the frozen library and standalone manifest.
