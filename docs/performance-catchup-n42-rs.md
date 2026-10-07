# QMDB performance catch-up

## Baseline and scope

Date: 2026-10-06. Work branch: `perf/qmdb-catchup-20261006`, based on
`origin/main` at `007ffcf`. The old feature branch remains at `e44078d`, also
saved as `backup/pre-qmdb-performance-20261006`. Its seven unique commits
were preserved, not merged into this performance branch. A trial merge found
15 conflicting files, including older dependency and state-tree changes.

The live Engine state-root path is `Gov5QmdbStateRootStore` → `QmdbLeafTree`
(`crates/n42-node/src/qmdb_state_root.rs`). This change targets that split
QMDB commitment: frozen leaf roots, active bits and the live-entry index.
It does not modify the legacy `TwigTree` or migrate the state format.

Reference: local `../n42-rs`, branch `feat/native-fleet7`, commit `09a1284e4`.
Its `qmdb_compat.rs::apply_sorted_leaf_ops` separates batched leaf hashing
from structural writes and rehashes affected twigs afterwards; its `root`
folds shared upper ancestors once per level. The implementation here adapts
those principles to `QmdbLeafTree`'s sealed/open twig representation.

## Changes

- Hash block writes with the existing batched leaf-hash kernel before mutation.
- Fold the contiguous appended range once per twig, before sealing or root read.
  Existing prefix leaves and inactive frozen leaves keep their commitments.
- On undo, fold only removed leaves in the surviving open twig, instead of
  rebuilding its entire heap; deletion-only undo does not rehash frozen leaves.
- Fold dirty twig roots' shared upper ancestors once per level.

Key sorting, duplicate rejection before mutation, slot assignment, root
encoding, persistence layout and undo record format are unchanged.

## Local benchmark

Apple M1 Max, macOS arm64, Rust 1.97.1, release with `opt-level=3`, thin LTO,
default features. Synthetic tree with 100,000 initial live entries, 72-byte
values. Each round clones the candidate operations, applies with an undo
record, undoes, reads the restored root and checks it against the parent.
The measured loop includes sorting, allocations and undo, not network or WAL I/O.

| Operations per round | Rounds | Baseline | Changed | Speedup |
| --- | ---: | ---: | ---: | ---: |
| 1 | 1,000 | 207,311 µs | 6,536 µs | 31.7× |
| 128 | 1,000 | 460,398 µs | 135,800 µs | 3.39× |
| 25,000 | 20 | 876,864 µs | 361,407 µs | 2.43× |

These are single-run observations using identical benchmark source and
dependency lock versions for baseline and changed source. They do not prove
whole-chain TPS parity with n42-rs, nor x86 SIMD speedups.

Normal reproduction with the required Reth checkout installed:

```sh
cargo run --release -p n42-twig-core --example qmdb_apply_bench
cargo test --release -p n42-twig-core --lib
cargo test --release -p n42-twig-core --lib --features rayon
```

## Validation boundary

The top-level Cargo command currently fails while loading the workspace:
`../reth/crates/storage/storage-overlay/Cargo.toml` is absent from the local
`../reth` checkout (`n42-v2-upgrade`, `77e0b8c25c`). No shared Reth files were
changed. QMDB core verification uses copies of this crate's source and testdata
in a standalone temporary Cargo package, with workspace dependency versions
and the repository lockfile; unused profiler dependencies are omitted.

New tests compare against the independent eager split-QMDB implementation
through partial prefixes, exact seals, multiple seals, undo/reapply, mixed
deletes/writes, empty and long values, proofs and atomic duplicate rejection.
Standalone debug/default, release/default and release/Rayon suites all passed:
62 passed, 0 failed, 1 ignored in each configuration. Formatting and
`git diff --check` passed. No x86-only kernel was exercised on this ARM host.
The existing real-export test remains ignored because it requires an external
leaf-form file.

On 2026-10-06, an isolated worktree at the required Reth revision
`23316e3ff8adca8c3bd5085ff0565fcae019202a`, with the repository transaction-root
patch applied, successfully built the profiling node and stress binaries.
Fresh seven-node QMDB/H2 tests verified common commitments and measured
7,372 transfer TPS and 3,238 storage-call TPS in short runs. See
[the seven-node report](performance-seven-node-20261006.md) for raw evidence,
flamegraphs, load-generator errors and the improvement plan. These runs do
not certify long-term throughput or historical large-state performance;
no end-to-end baseline A/B was performed. The shared `../reth` remains unchanged.
