# Audit: n42-26 `n42-twig-core` vs Go QMDB defect family (2026-07-12)

Scope: `D:\n42\n42-26` (Rust repo) — `crates/n42-twig-core` (engine),
`crates/n42-jmt/src/twig.rs` + `persistent.rs` (StateDiff bridge + snapshot/WAL
persistence), `crates/n42-consensus-service/src/orchestrator/consensus_loop.rs`
(StateSink integration), `crates/n42-node/src/sinks/mod.rs`,
`bin/n42-node/src/main.rs` (wiring + genesis seeding). Read-only audit; no code
changed.

Reference defect family: the Go `lib/qmdb` incidents fixed on branch
`feat/eth-el-snapshot-direct` (deadFlushed cross-reload residue, ApplyUndo index
restore, index/live-bit reconciliation, reload fast path, undo read-miss
poisoning).

Architecture summary (relevant to every verdict below): the Rust twig is an
**all-DRAM engine with no in-place persistent rows**. Durability =
full-state snapshot (bincode+zstd, atomic tmp+rename) + append-only per-block
`StateDiff` WAL (fsync per record), WAL truncated only after a durable
synchronous snapshot. Reload = deserialize snapshot → rebuild a **fresh**
`TwigTree` per shard → replay contiguous WAL records. The tree is a
consensus **sidecar** (does not gate HotStuff finality; feeds mobile proofs,
`n42_twigRoot` / `n42_twigProof` RPC, and metrics), applied asynchronously
after commit. Reload happens only at process startup
(`bin/n42-node/src/main.rs:668`); there is no mid-run reload.

## Findings table

| # | Go-side lesson | Verdict | Evidence |
|---|----------------|---------|----------|
| 1 | deadFlushed residue across reload | **Structurally immune** (core mechanism) — but **2 same-family gaps at the WAL boundary (F1a, F1b)** | see below |
| 2 | Revert/ApplyUndo index restore | **Not applicable** — no revert/rollback/undo path exists at all; HotStuff commits are final | `grep -i "revert\|rollback\|unwind\|undo"` = 0 hits in `n42-twig-core/src` and `n42-jmt/src` |
| 3 | index count == live-bit count reconciliation | **Missing** — no equivalent self-check anywhere; recommended (F3) | `n42-twig-core/src/lib.rs` (`FlatIndex.live` vs `Twig.live` never cross-checked) |
| 4 | Reload fast path (sealed-twig meta) | **Same cost shape as Go slow path, O(append history)**; several optimization openings (F4) | `n42-twig-core/src/lib.rs:457-483`, `persistent.rs:240-260` |
| 5 | Poison on read-miss during record/snapshot | **Same-family defect exists** — silent `EMPTY_CODE_HASH` fallback on a read that must succeed (F5) | `n42-jmt/src/twig.rs:133-135`, `n42-jmt/src/tree.rs:149-158` |
| — | (bonus) reload-path idempotence | **Bug: genesis reseed is not idempotent under the append-slot model (F6)** | `bin/n42-node/src/main.rs:876-897` |
| — | (bonus) out-of-order arrival (Go bbf0828e family) | **Hazard: per-commit `spawn_blocking` gives no FIFO guarantee; twig root is append-order-dependent (F7)** | `consensus_loop.rs:516-545` |
| — | (bonus) catch-up realign (Go BestPeers family) | **Missing-diff = permanent silent divergence; the "(will catch up)" log is aspirational, no mechanism exists (F8)** | `consensus_loop.rs:546-548` |

---

## Item 1 — deadFlushed cross-reload residue

**Verdict: structurally immune in the core mechanism.**

- There is **no deferred deletion of persisted rows**. Nothing like Go's
  deadFlushed list exists: deactivation only nulls an in-memory leaf
  (`TwigTree::deactivate`, `n42-twig-core/src/lib.rs:277-285`); persistence is a
  full-state dump (`snapshot()`, lib.rs:433-453) written atomically
  (tmp + rename + fsync, `n42-jmt/src/persistent.rs:203-238`).
- **Reload carries zero in-memory bookkeeping across generations.**
  `TwigTree::restore` (lib.rs:457-483) constructs a brand-new tree — fresh
  `FlatIndex`, fresh twigs, fresh `touched`, fresh arena — from the snapshot's
  positional entry log. `ShardedTwig::from_snapshot` (lib.rs:799-807) builds a
  new object and folds roots once. Nothing survives a reload that could refer
  to a previous tree's slots.
- **Crash windows around the checkpoint are safe**: `flush()` saves the
  snapshot durably *before* truncating the WAL (persistent.rs:573-579); a crash
  between the two leaves WAL records ≤ snapshot version, which replay skips via
  `version < expected` (persistent.rs:474-476). WAL replay refuses
  non-contiguous versions loudly (persistent.rs:477-482) — good.

### F1a — WAL append failure is not rolled back; recovery silently discards the tail (MEDIUM)

`append_wal` (persistent.rs:548-555) does three `write_all` calls + fsync. If
it fails partway (disk full, IO error), a **partial record** remains at the end
of the WAL file, and the file handle stays open in append mode. The error
propagates to the orchestrator, which only logs `warn!` (see F1b) — the next
committed block then appends a *new* record **after the partial bytes**.

On the next reopen, `read_wal_entries` (persistent.rs:176-197) parses the
partial record's header, reads `len` bytes that now **span into the following
record**, and on bincode failure `break`s — a policy designed for a *truncated
trailing* record, here silently discarding **every later, fully durable
record**. Worst case, bincode succeeds on the misaligned bytes and a garbage
diff is replayed. Either way: silent state divergence with no diagnostics.

Fix direction: record the file offset before appending; on any append error,
`set_len(offset)` to roll the file back (and re-fsync), or mark the sink
poisoned so no further appends occur.

### F1b — apply failure is warn-and-continue; version→block mapping silently shifts (HIGH within sidecar scope)

`consensus_loop.rs:540-543`:

```rust
Err(e) => {
    warn!(target: "n42::twig", error = %e, "Twig apply_diff/persist failed");
}
```

If block N's `apply_diff` fails (WAL IO error — the only fallible step), the
in-memory tree is *not* mutated, and block N+1's diff is then applied **as
version N**. The WAL stays contiguous (versions are derived from
`inner.version() + 1` under the mutex), so the recovery-time contiguity check
can never catch it. Result: block N's state change is permanently missing,
every subsequent version number is off by one relative to blocks, and the twig
root silently diverges from every healthy node — the exact "silent continue
after a failed step" pathology the Go side just eliminated. There is no
poisoned flag, no metric, no halt.

Fix direction: on `apply_diff` error, latch a poisoned/halted state on the sink
(stop accepting diffs, surface via metric + RPC), exactly like Go's poisoning
fix. A sidecar that silently keeps producing wrong roots is worse than one that
stops.

## Item 2 — revert/rollback index consistency

**Not applicable.** Neither `n42-twig-core` nor `PersistentTwig` has any
revert/undo capability (zero grep hits for revert/rollback/unwind/undo). The
twig only consumes **committed** HotStuff blocks (final; pre-commit reorgs
happen inside reth's engine tree before the diff is extracted), so the Go
ApplyUndo defect class has no surface here. Design note: if single-block unwind
is ever added, the append-slot model makes it non-trivial (deactivations must
be re-activated, appended slots truncated, index entries restored — the exact
Go pitfalls); the current snapshot+WAL "rebuild from history" model is the safe
substitute.

## Item 3 — index vs live-bit reconciliation

**Missing.** Three independent live counters exist and are never cross-checked:

- `FlatIndex.live` (flat.rs:22, exposed as `TwigTree::len()`),
- `Twig.live` per twig (lib.rs:62),
- the count of `Entry.active == true` in the slot log.

An invariant check `index.len() == Σ twig.live == #active entries` (per shard)
would have caught the Go incident class and would also catch the one real
(if cryptographically negligible) inconsistency source in this design: a
16-byte **prefix collision** in `FlatIndex` (lib.rs:196-199). On collision,
`set` of key B fails to find B (full-key filter), then `index.insert` at
lib.rs:328 **overwrites key A's mapping** — A's leaf stays live and committed
but becomes unreachable, and a later re-set of A appends a second live leaf for
the same logical key. `index.len()` and `Σ live` diverge by exactly 1 — the
reconciliation check detects it immediately.

Recommendation (F3): add `TwigTree::check_consistency()` and call it at the end
of `restore()` and (debug builds / opt-in env) after each `apply_batch`.
Cheap: all three counters are O(1)/O(shards) to read.

## Item 4 — reload cost structure

Reload happens **only at startup** (`PersistentTwig::open`,
`bin/n42-node/src/main.rs:668`); no mid-run reload exists.

Cost shape (all in `restore()`, lib.rs:457-483, driven by
`load_twig_snapshot`, persistent.rs:240-260):

1. Read + zstd-decode + bincode-deserialize the **entire** snapshot — one
   `Vec<u8>` heap allocation per entry (`EntrySnapshot.value`), i.e. millions
   of allocations at production scale.
2. `hash_leaf` (blake3) recomputed for **every active entry** — the analog of
   Go's "one point-op per live slot" slow path, though DRAM-only (no MDBX point
   reads), and leaf hashing is SIMD-batched only on the apply path, not here.
3. `root()` folds every touched twig; dense twigs take the full-recompute
   SIMD path — fast, but still O(all twigs).
4. Iteration is over the **whole append history including dead slots**:
   `snapshot()` persists every dead slot's 32-byte key forever, so snapshot
   size and reload time grow with history, not with the live set.
   `TwigTree::compact` (lib.rs:400-431) would bound this but is **dead code in
   production**: not exposed through `ShardedTwig`/`TwigState`, never called
   outside twig-core's own tests.

Optimization openings, in rough value order:

- **Persist per-entry leaf hashes (or per-twig leaf-hash arrays) in the
  snapshot** — removes step 2 entirely; this is the closest analog of Go's
  "sealed twig read-only meta" 5s→0.4s fast path. 32 bytes/entry of snapshot
  growth, or store only for twigs below a churn threshold.
- **Wire compaction** to bound dead-slot accumulation — but see the warning
  below before doing so.
- Batch step 2's leaf hashing through the existing `simd::hash_leaves` kernel
  (trivial, helps even without snapshot format changes).

**Warning (latent, blocks naive compaction wiring):** `compact()` changes the
world root but is **not representable in the WAL** (the WAL holds only
`StateDiff`s). If compaction is ever invoked between checkpoints, a crash +
snapshot/WAL replay reconstructs the *uncompacted* tree → different root than
the pre-crash live tree, i.e. exactly a "recovery path disagrees with the live
tree" incident. Wiring compaction requires either a WAL record type for it or
the rule "compact ⇒ immediate synchronous `flush()` before the next block, with
compaction deterministically scheduled at block boundaries on every node."

## Item 5 — read-miss handling while building records

**Same-family defect exists.** `TwigState::prepare`
(`n42-jmt/src/twig.rs:133-135`):

```rust
let code_hash = match &account_diff.code_change {
    Some(change) => change.to.unwrap_or(EMPTY_CODE_HASH),
    None => self.inner.get(&key).map(decode_code_hash).unwrap_or(EMPTY_CODE_HASH),
};
```

For a **`Modified`** account with no code change, the current entry **must**
exist in the tree. If `get` returns `None` (index corruption, lost WAL tail per
F1a, skipped block per F1b/F8, prefix-collision eviction per F3), the code
silently substitutes `EMPTY_CODE_HASH`, commits a wrong leaf, and the root
diverges **with zero diagnostics** — precisely the Go "undo record read-miss
must poison, not continue" lesson. `Created` accounts legitimately default
(newly created EOAs arrive with `code_change = None`); the fix must distinguish
by `change_type`, which is available at that point. The identical pattern
exists on the SBMT path (`n42-jmt/src/tree.rs:149-158`).

Adjacent minor instance: ZK input extraction failure logs a warning and
proceeds with an empty bundle (`consensus_loop.rs:561-567`) — flagged for
awareness, out of twig scope.

## F6 (bonus) — genesis reseed is not idempotent under the append-slot model

`bin/n42-node/src/main.rs:876`: after `PersistentTwig::open`, the node reseeds
the genesis alloc whenever `tree.version() == 0`. But `seed_genesis_account`
does not bump the version, and `flush()` persists a **version-0 snapshot**. So:

1. First boot: seed genesis (slots 0..N), `flush()` succeeds, version stays 0.
2. Operator restarts the node **before the first block commits** (routine
   during network bring-up).
3. Reopen: snapshot restores version 0 → `version() == 0` → **reseed runs
   again on the restored state**. In the append-slot model `set` of an existing
   key deactivates the old slot and appends a new one — same values, different
   slots, **different root**.
4. This node's twig root now silently disagrees with every node that did not
   restart, forever (all later diffs apply on top of the skewed history).

The SBMT path is immune (map semantics: re-set of identical values is a no-op
for the root); this is twig-specific. Ironically the *failure* path is fine
(if the genesis `flush()` failed and the snapshot is absent, reseeding an empty
tree reproduces the same root); the *success* path is broken.

Fix direction: persist an explicit "genesis seeded" marker — simplest is to
skip reseed when the snapshot file existed at open (plumb a
`restored_from_disk: bool` out of `PersistentTwig::open`), or check
`next_slot() > 0` on shard 0 / total entry count instead of `version == 0`.

## F7 (bonus) — per-commit `spawn_blocking` has no ordering guarantee

`consensus_loop.rs:516-545` spawns one `tokio::task::spawn_blocking` per
committed block; each task locks the `Mutex<PersistentTwig>`
(`n42-node/src/sinks/mod.rs:32-40`). Tokio's blocking pool does not guarantee
FIFO execution across tasks and the mutex is not fair, so under load block
N+1's task can acquire the lock before block N's. The twig root is a function
of **append order** (lib.rs:188-191), so an order swap silently diverges the
root from every other node — and because WAL versions are assigned *under the
mutex* (`inner.version() + 1`), the WAL stays contiguous and the recovery
check cannot detect it. This is the Rust analog of the out-of-order-arrival
class fixed on the Go side (bbf0828e).

Window today: apply is ms-scale vs seconds-scale block time, so overlap is
rare — but the 48K-load comment in this very file (consensus_loop.rs:436)
shows the regime where it stops being rare.

Fix direction: a single dedicated apply thread per sink fed by an ordered
mpsc channel (block-commit order), instead of per-commit `spawn_blocking`;
optionally assert in `PersistentTwig::apply_diff` that the incoming diff's
expected version matches (requires passing the committed block count through
`StateSink::apply_diff`, which also fixes the version↔block drift of F1b).

## F8 (bonus) — no catch-up for a missed diff

`consensus_loop.rs:546-548`: when `pending_block_data` lacks the committed
block (evicted under memory pressure, mod.rs:465-483, or never received), the
state-tree update is skipped with a debug log "(will catch up)". **No catch-up
mechanism exists** (the string appears nowhere else). The twig permanently
misses that block's diff; all later roots are wrong; combined with F1b-style
silent continuation there is no signal. At minimum this should be a `warn!` +
a monotonic gap counter metric + the same poisoned/degraded flag as F1b; a
real fix replays the diff from reth once available.

---

## Summary of judgments

- The Go **deadFlushed** bug class itself cannot occur: no deferred persistent
  deletions, full-object reload, atomic checkpointing. The engine core is clean.
- The **failure-path discipline** around the engine is where the same *family*
  reappears: F1a (unrolled-back WAL append), F1b (warn-and-continue on apply
  failure), F5 (silent default on a must-succeed read), F8 (missed diff with no
  catch-up). All four share the Go root cause: *a failed or impossible step is
  logged and skipped instead of poisoning the component.*
- F6 (genesis reseed) and F7 (apply ordering) are twig-specific consequences of
  the append-order-dependent root that the map-semantics SBMT tolerates.
- Item 3's reconciliation check and Item 4's leaf-hash-persisting fast path are
  recommended hardening/perf work, not active bugs.

---

## Codex task

> Self-contained task brief for a follow-up implementation session in
> `D:\n42\n42-26`. Commit convention: author **Nyxen**, no AI attribution in
> messages or trailers; one logical change per commit; English commit messages,
> conventional-commit style (`fix(twig): …`, `feat(twig): …`).

### Background

The twig state tree (all-DRAM QMDB port) is the default proof sidecar
(`N42_TWIG` defaults on, `bin/n42-node/src/main.rs:631-689`). Its root is a
function of append history, so any skipped, reordered, or wrongly-defaulted
operation silently and permanently diverges the root across nodes. An audit
(this document) found five fixable defects: WAL append not rolled back on
error (F1a), warn-and-continue on apply failure (F1b), silent
`EMPTY_CODE_HASH` fallback on a must-succeed read (F5), non-idempotent genesis
reseed after restart (F6), unordered per-commit `spawn_blocking` apply (F7);
plus one missing invariant check (F3).

### Files

- `crates/n42-jmt/src/persistent.rs` — `PersistentTwig` (and mirror fixes in
  `PersistentSbmt`): WAL append rollback, poisoned state.
- `crates/n42-jmt/src/twig.rs` — `TwigState::prepare` code-hash read-miss;
  `apply_diff` becomes fallible.
- `crates/n42-jmt/src/tree.rs` — same read-miss fix on the SBMT path.
- `crates/n42-twig-core/src/lib.rs` — `check_consistency()`.
- `crates/n42-consensus-service/src/orchestrator/consensus_loop.rs` and
  `crates/n42-consensus-service/src/orchestrator/mod.rs` — ordered apply
  channel replacing per-commit `spawn_blocking`; degraded-state metric.
- `crates/n42-node/src/sinks/mod.rs` — `StateSink` error propagation stays;
  no interface change expected beyond error typing.
- `bin/n42-node/src/main.rs` — genesis reseed guard.

### Expected changes

1. **WAL append rollback (F1a)** — in `append_wal` (both `PersistentTwig` and
   `PersistentSbmt`): capture the current WAL length before writing; on any
   write/sync error, attempt `File::set_len(prev_len)` + `sync_data` to remove
   the partial record; if the rollback itself fails, transition the sink to a
   poisoned state. Never leave a partial record followed by later appends.
2. **Poisoned sink instead of silent skip (F1b)** — add a `poisoned:
   Option<String>` (or enum) field to `PersistentTwig`/`PersistentSbmt`. Any
   `apply_diff` error sets it (with version + block context); once poisoned,
   `apply_diff` returns an error immediately without touching WAL or memory.
   In `consensus_loop.rs`, on sink error: log at `error!`, increment a
   `n42_twig_sink_poisoned` counter, and stop publishing `update_twig_root`.
   The node keeps running (sidecar), but never publishes wrong roots.
3. **Read-miss is loud (F5)** — in `TwigState::prepare` and the SBMT
   equivalent: for `AccountChangeType::Modified` with `code_change == None`,
   a `get` miss is an **error** carrying the address and derived key (this
   makes `prepare`/`apply_diff` fallible — thread `Result` through; the
   `StateSink` impls already return `Result<_, String>`). `Created` keeps the
   `EMPTY_CODE_HASH` default. The error path feeds the poisoning of (2).
4. **Genesis reseed guard (F6)** — `PersistentTwig::open` returns (or exposes)
   whether state was restored from disk (snapshot present or WAL replayed >0
   records). `main.rs` skips reseeding when restored, even at version 0.
   Alternative acceptable check: total appended slots > 0.
5. **Ordered apply (F7)** — replace the per-commit `spawn_blocking` for
   jmt/twig sinks with one long-lived apply worker per sink (std thread or
   dedicated `spawn_blocking` loop) consuming an unbounded ordered
   `mpsc::UnboundedSender<(u64 /*block*/, StateDiff)>` filled in commit order.
   Optionally pass the block count into `apply_diff` and reject
   version-mismatched diffs (defense in depth against reordering).
6. **Consistency check (F3)** — `TwigTree::check_consistency() -> Result<(),
   String>` verifying `index.len() == Σ twigs.live == #active entries`; call it
   at the end of `restore()` (return error → `from_snapshot` propagates →
   `PersistentTwig::open` fails loudly), and behind `debug_assert!` after
   `apply_batch_indexed`.

### Acceptance criteria (tests required)

- `cargo test -p n42-twig-core -p n42-jmt -p n42-consensus-service -p n42-node`
  green; `cargo clippy` clean on touched crates.
- New tests, each asserting the *behavior*, not just absence of panic:
  - **F1a**: simulate a partial trailing WAL record followed by valid records
    (craft the file bytes directly); assert reopen either recovers all
    post-corruption records after rollback-on-error is in place, or — for a
    crafted pre-existing corruption — fails loudly instead of silently
    truncating (pick and document one policy; silent tail-drop is the bug).
  - **F1b**: force an `apply_diff` error (e.g. WAL file handle replaced by a
    read-only file); assert the sink is poisoned, subsequent applies fail
    fast, and the in-memory version did not advance.
  - **F5**: build a `StateDiff` with a `Modified` account whose key is absent
    from the tree; assert `apply_diff` errors (and does not commit a leaf with
    `EMPTY_CODE_HASH`). Companion test: `Created` with no code change still
    succeeds with `EMPTY_CODE_HASH`.
  - **F6**: seed genesis → `flush()` → drop → reopen → assert root unchanged
    and reseed does not run (root equality is the assertion; with the bug it
    differs). Test at the `PersistentTwig` + seed-guard level so it does not
    require the full node.
  - **F7**: unit-test the ordered worker: feed diffs for blocks 1..N from
    multiple producer tasks completing out of order; assert applied version
    sequence is exactly 1..N and the final root equals a serially-applied
    reference tree.
  - **F3**: corrupt a restored tree's index (test-only hook or
    `#[cfg(test)]` accessor); assert `check_consistency` reports the mismatch;
    assert a clean 5k-op tree passes.
- No change to any committed root for healthy paths: the existing
  cross-language tests (`cross_check_root_vs_gov5_go`,
  `cross_check_sharded16_root_vs_gov5_go`, `cross_check_sorted_batch_vs_gov5_go`)
  must pass unmodified.
- Commits: separate commits for (F1a+F1b), (F5), (F6), (F7), (F3); author
  Nyxen; no AI/assistant attribution anywhere.
