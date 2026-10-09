# Concurrent candidate computation and read-view construction

See [devlog-164](../../devlog-164-qmdb-overlap-20260914.md) for results and limits.
The binary commitment, read-index implementation, exact-parent identity, root
check, WAL encoding, fsync and publication ordering are preserved.

`baseline-full.rs` and `baseline-read-view.rs` freeze the production inputs.
`optimized-full.rs` and `qmdb-read-view.rs` freeze the final production sources.
`store.rs` extracts the store and selected tests from the production file using
`snapshot.py`. `core-source` is an unchanged core library source snapshot with a
standalone dependency manifest. `exec_cache.rs` is reference context, unchanged
in this round. `directory-sync.rs` supplies the existing directory-sync helper.

From this directory:

```sh
CARGO_BUILD_JOBS=2 cargo test --release --locked --offline
```

Use Cargo's emitted test binary as `BIN`. Use an actual ext4 directory for the
temporary durable store; `/tmp` was tmpfs on the measurement host.

```sh
python3 run.py --binary "$BIN" --out ordinary-rerun --tmpdir /path/on/ext4 --pairs 6
python3 run.py --binary "$BIN" --out prepared-rerun --tmpdir /path/on/ext4 --pairs 6 --preparation retain
```

The runner alternates serial/concurrent execution order in fresh processes,
with three blocks per process and 200k/2M initial accounts. It records all
samples, corresponding roots, WAL lengths and candidate/commit durations. Every
process reopens its WAL and verifies the final root and an account value.
Every final sample also asserts whether a worker actually joined, using a local
metrics recorder outside timed regions. In the prepared run no worker joins.
The recorder drains counters, so the reported joined count is per block.

The serial switch and injected thread-spawn failure are `cfg(test)` fields, not
production environment controls. Both benchmark modes use the final candidate
and persistence code; the serial mode disables only the concurrent read view.
The ordinary mode includes input sorting in commit timing. The prepared mode
includes both input sorts in candidate timing. EVM execution, delta generation,
oracle construction, initial state/view construction and restart checks are
outside the timed region. These are storage timings, not transaction TPS.

`ordinary` and `prepared` are the final comparisons. `pilot` is the exploratory
run before the added overlap counters. `*-pre-final` contains an interrupted
earlier measurement, retained for transparency and excluded from the final
summary. `pre-final-full.rs` preserves that earlier store source. It omitted
exact-delta matching from the candidate metric; the final version restores that
metric's coverage. Each run records its own binary SHA; do not substitute a
different run's binary or claim the interrupted run completed.

`upstream-account-prefetch.patch` records the relevant fixed Gov5 commit's
reader implementation and tests. Node and clippy logs use the fixed patched
reth revision recorded in `evidence.json`, not the adjacent development reth.
