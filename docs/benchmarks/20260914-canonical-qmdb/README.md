# Canonical execution with QMDB-only parent reads

This experiment uses the production `N42EvmConfig`, tracking block executor,
`StateProviderDatabase`, QMDB-only adapter, delta conversion, and durable QMDB
root store. It is a local execution/storage baseline, not four-node TPS.

Unlike `parallel_profile` and the pure EVM benchmark, each block uses the
previous committed QMDB version and increasing sender nonces. There are distinct
senders and recipients, and their address sets are checked for overlap. The
fallback provider has no accounts or storage, so funded transfers cannot
succeed through a silent fallback. Every block checks receipts, gas, state-read
counters and a binary root computed from independent transfer/fee arithmetic.
All touched accounts are checked again after reopening the durable store.

The workload is EOA transfers under the N42 Cancun dev configuration, with
existing funded recipients and a funded beneficiary. It does not exercise
contract storage, Prague system calls, full Engine header validation, consensus
or Reth's canonical database writes. Headers are synthetic execution inputs;
their hashes are store identities, not native-header or H2 finality proofs.
The completed measurements in `system-results/` and the `system-*` snapshots
used the System allocator. The current Unix library test uses Jemalloc, matching
the node binary; its small correctness test passed (`jemalloc-correctness-test.log`),
but no Jemalloc performance run is reported. Unsuffixed source archives and
environment files describe the earlier System build. No result qualifies 1M TPS.

The clock includes provider pinning, canonical block execution and full output
construction, restored-slot lookup, delta conversion/sorting, binary root work,
immutable view construction, WAL encoding, write and fsync. Input generation,
signing, initial state, the independent oracle, result checks and restart
verification are outside it. Real signatures are recovered and checked before
execution; that separate sequential diagnostic is reported as
`recoveryMsExcluded`. Production Engine uses parallel recovery with streaming
and prewarming, so neither ignoring this diagnostic nor adding it serially
produces an Engine/fleet TPS measurement.

Build using the patched reth source and the workspace lock:

```sh
CARGO_BUILD_JOBS=2 cargo test -p n42-node --lib --release --locked --offline \
  canonical_qmdb_execution_commits_consecutive_blocks
```

Use the emitted test binary as `BIN`, then choose an actual ext4 temporary
directory for the WAL; `/tmp` on the measurement host is tmpfs:

```sh
python3 run.py --binary "$BIN" --out rerun --tmpdir /path/on/ext4 \
  --accounts 200000 2000000 --transactions 50000 --blocks 3 --runs 3
```

The ignored test can also be selected directly with
`qmdb_state_reader::canonical_bench::bench_canonical_qmdb_execution_and_commit`.
Settings are `N42_CANON_BENCH_ACCOUNTS`, `N42_CANON_BENCH_TXS`, and
`N42_CANON_BENCH_BLOCKS`. Each process commits a fresh three-block chain. The
runner rotates state-size order, verifies roots/hashes/WAL lengths across runs,
and records all samples plus the exact binary SHA. `run.json` is marked complete
only after all processes and comparisons pass.

`workspace-source.tar.gz` contains the actual workspace source inputs, including
uncommitted changes, with individual hashes in `workspace-source-sha256.json`.
It can be extracted as `n42-26` next to a `reth` checkout. The supplied two
patches apply to reth commit `23316e3ff8adca8c3bd5085ff0565fcae019202a`.
`reth-source-verification.json` records a byte-for-byte comparison of all 1810
patched reference files to the dependency source used for the build. Its Git
HEAD is an empty temporary build-metadata commit, not the source revision;
the source comparison, rather than that HEAD, establishes the baseline.
`environment.json` records the compiler, CPU affinity and measurement limits.
