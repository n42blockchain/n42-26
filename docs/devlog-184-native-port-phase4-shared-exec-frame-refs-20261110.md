# Devlog 184 — Native-path port phase 4: cycle redesign, shared execution, frame-referenced blocks (2026-11-10)

Branch `feat/native-port`, on top of phase 3b (`b052f96`,
[devlog-183](devlog-183-native-port-phase3b-qmdb-20261108.md)). Bench as in devlog-183:
7 validators on one host, H2-v4 participant mode, QMDB, 2 G gas, 95,238 transfers per full
block, 1.6 M attested 0x50 transfers. Scratch: `.artifacts/native-port-p0/` (`qf7g..`,
`run17..`, `run17.sh`, `round26.sh`).

## 1. Runs 17-23: per-transaction cuts (before the redesign)

| Run | Change | Wall-clock TPS |
|---|---|---|
| run20 | builder `finish` without the MPT post-state hash and receipts trie in H2 mode; Gov5 serve cache keeps gossiped bodies unchecked until a peer asks | 58.3k |
| run21 | repeat | 61.0k |
| run22 | + metrics (`n42_parallel_import_batch_ms`, `n42_payload_builder_finish_ms`, sender cache hit/miss) | **65.5k** |
| run23 | run22 + `--cpu-cores=16 --engine.disable-prewarming` (`chain94-fleet.py --node-arg`) | 63.7k |

run23 shows no gain from the thread cap. All runs: identical hashes on 7 nodes.

- `n42-node/src/parallel_payload.rs`: with `builder_state_root_replaced()` the builder mirrors
  `BasicBlockBuilder::finish` without `hashed_post_state` and the MPT receipts root; the
  header carries the Gov5 flat receipts root, which normalization would write anyway.
  Amsterdam (block access lists) keeps the upstream path.
- `n42-network/src/service.rs`: `ServedGov5Block { rlp, verified }`; a body seen on gossip,
  direct push or local broadcast is retained without decoding and checked by
  `servable_gov5_block` only when a peer requests it; the first body for a hash is kept.
- `scripts/chain94-fleet.py`: `start --node-arg=--flag=value`, recorded in `run.json`.

## 2. Where a full-block cycle goes (run22, view 36, 95,238 transfers, ~1.39 s)

| Step | Time |
|---|---|
| commit → slot boundary | 172 ms |
| build (packing 133, finish 98, cache 25) | 306 ms |
| built → ready (QMDB root, receipts root, tx hashes, normalization) | 188 ms |
| compress 33 + transfer + follower decode | ~140 ms |
| follower `new_payload` (parallel batch ~82, QMDB root 85, rest unsplit) | 538 ms |
| two vote rounds | 40 ms |

The steps run one after another, so 65k TPS is 95,238 / 1.39 s. Followers miss the sender
cache for 93% of transactions (1,489,783 of 1,600,000 on node0), because a frame reaches one
node: each follower verifies ~89k Ed25519 signatures per block inside `new_payload`.
Note: the in-process pprof sample counts undercount CPU (5,372 samples in 10 s while the
process used 153 CPU-seconds over the run); they rank functions, they do not measure cores.

Per-transaction CPU cuts do not change this structure. The redesign targets the structure:

1. One execution per machine (this devlog, section 3).
2. Blocks sent as frame references (section 4), then built in frame order.
3. No slot wait under load (`N42_FAST_PROPOSE=1`, existing), frames to every node
   (`native-flood --broadcast`, existing).
4. Later: next leader builds before the commit, lazy Gov5 copy, sharded execution and QMDB
   root, larger blocks at shorter cycles.

## 3. Shared execution for co-located validators (`N42_SHARED_EXECUTION=1`, default off)

Decision (user, 2026-11-10): build it. docs/04 section 3 names it as the route's deployment:
validators on one machine vote independently and execute a block once.

- **What is shared.** The leader's execution output (bundle state, receipts, senders,
  transaction root) travels next to the block in `BlockDataBroadcast.execution_output`
  (the existing compact-block field, MessagePack + zstd). The follower loads it into
  reth's payload cache; `new_payload` then skips execution.
- **What the follower still checks.** The QMDB state root computed from the output
  (`Gov5QmdbStateRootJob`), the Gov5 receipts root, gas used and bloom, all against the
  header, before it votes. It does not check that the output is what the transactions
  produce, nor the senders or the bundle's revert data: that is the trust placed in the
  co-located leader. The setting is for validators of one operator on one machine.
- **Restored slots.** The QMDB root counts slots a block changed and set back as writes;
  only a node that executed the block knows them (`n42_execution::restored_slots`). The
  compact output gains `restored_slots`; the follower files the list under a key it derives
  from the payload in hand (`gov5_restored_slots_key_for_payload`), never from the wire. An
  output without the list is refused on the QMDB route.
- **Leader bookkeeping.** `take_gov5_normalization_roots` consumed the broadcast copy. With
  the flag on it puts the copy back and records the restored slots;
  `rekey_gov5_normalized` moves both to the normalized hash, where `take_serialized` finds
  them.
- **Gates.** `should_broadcast_execution_output(h2) = compact && (!h2 || shared)`;
  `ExecutionOutputCache::inject_for_payload` (new, default = `inject`) is the only entry
  that accepts an output on the QMDB route, and only with the flag; plain `inject` and the
  trusted-root mode still refuse. The fleet sets `N42_COMPACT_BLOCK=0`; a shared run passes
  `--env N42_SHARED_EXECUTION=1 --env N42_COMPACT_BLOCK=1`.
- Also in this step: the leader's normalization hashes transactions and the receipts root
  side by side on rayon and logs `N42_GOV5_NORMALIZATION_ROOTS` (hashed / operations / root
  ms).

- **Refusal falls back to execution.** Reth patch `reth-cached-execution-refusal`: a
  validation failure reached with a cached output (post-execution checks, hashed-state
  check, state-root job, root mismatch) becomes `ConsensusError::Other(
  CachedExecutionRefused)`; `on_insert_block_error` reports the payload Invalid but does not
  put the hash into `invalid_headers`. The follower's eager import then evicts the output
  and submits the same payload once more, which executes it; only that second verdict can
  mark the block bad (`n42_shared_execution_refused_total`).

Known limits (before this could be on by default):
- If a follower handles the Gov5 copy of a block before the native copy, it executes in full
  (the Gov5-derived envelope carries no output).
- The follower still decodes transactions and recovers senders in the background of a
  cache hit, and builds the hashed post state.
- Only the live eager-import path takes shared outputs; sync and observers execute.

Tests: `orchestrator::tests::refused_peer_output_is_dropped_and_the_block_executed_locally`,
`exec_cache::tests::shared_output_carries_restored_slots_to_the_follower_or_is_refused`,
codec round trip with `restored_slots`, gate test
`normalized_gov5_payload_broadcasts_builder_output_only_when_shared`.

## 4. Frame-referenced blocks (`N42_FRAME_REF_BLOCKS=1`, default off)

Decision (user, 2026-11-10): build it. First step is a transport form only; block hash and
transactions root are unchanged (n42-rs compact body v1 in spirit, with frame runs instead
of hashes). The n42-rs consensus form (transactions root = frame tree) is a later step.

- `n42-tx-types/src/frame_store.rs`: `FrameStore` (global, `N42_FRAME_STORE_FRAMES`, default
  4,096 frames, oldest leaves first) keeps each admitted frame's exact transaction bytes by
  frame root, with a transaction-hash → (frame, position) index. Ingest inserts after
  authentication (`admit_native_frame`).
- `n42-consensus-service/src/orchestrator/frame_ref.rs`: payload form `N42EPF01` = binary-v1
  header + frame roots + runs `(frame, start, len)` in block order + loose transactions.
  `encode_frame_ref_payload` / `expand_frame_ref_payload`; expansion is byte-identical to
  the binary-v1 payload, checks every run against its frame and that runs cover the block.
- Leader (`broadcast_block_data`): in H2-v4 mode with the flag, peers get the referenced
  form (uncompressed; roots do not compress); the leader's own cache and sync serving keep
  the full payload.
- Follower (`handle_block_data`): expands before anything is cached, so all later readers
  see an ordinary block. With a frame missing it caches nothing and takes the Gov5 full
  copy when that arrives.
- Requirements: every validator runs this build; frames reach every validator
  (`native-flood --broadcast`). With the default sender-sharded drain a block is one run per
  transaction (~12 B/tx); `N42_SENDER_SHARDED_DRAIN=0` (arrival order) is the experiment for
  frame-long runs until the builder selects whole frames.

- **Frame-ordered building** (`N42_FRAME_ORDER_BUILD=1`, needs the frame store):
  `n42-node/src/frame_order.rs` `FrameOrderedTransactions` yields pool transactions frame by
  frame in arrival order (`FrameStore::unbuilt`), looking each frame up with
  `pool.get_all`; a frame with nothing left in the pool is retired from the walk. The
  pool-wide order (sender-sharded drain) is built only when the frames run out, with
  already-yielded transactions filtered, so a full block from frames never scans the pool.
  Only the order changes: the builder still executes every transaction and skips one that
  does not fit. A block is then ~96 runs instead of one run per transaction.

Known limits: the Gov5 full body is still encoded and sent for every block (lazy serving
is next); deferred frames (pool gate) are not stored until admitted; observers do not
expand; a reference list that does not match the block hash is refused by the engine like
any malformed payload.

Tests: `frame_order::tests::frames_come_first_in_arrival_order_then_the_rest_without_repeats`,
`frame_store::tests::*`, `frame_ref::tests::*` (blocks cut across frames, loose
transactions, missing frame, truncation and bad runs).

## 5. Measurements

Seven validators on one 256-core host, 2G gas (95,238 transfers per block), 1.6M attested
transfers, `native-flood` 20,000 senders x 80 frames of 1,000. TPS is wall clock, from the commit
of the block before the first loaded block to the commit of the last one in node0's log
(`.artifacts/native-port-p0/wall-tps.py`). Raw output: `.artifacts/native-port-p0/runNN-tps.txt`,
`runNN.out`, `runNN-metrics-node{0,3}.txt`, fleets `qf7n..qf7v`, `qf8a..qf8f`.

**The node flags differ between rounds, so compare within a round only.** run22 (65.5k) ran with
`--engine.disable-prewarming --cpu-cores=32`. Round A passed neither flag, round B only the first.
Median follower `new_payload` for a full block: 541 ms with both flags (run22), 786 ms with
prewarming off only (run33), 939 ms with neither (run24). No code regression: run20 (old binary,
no flags) measured 948 ms.

Round A (no node flags, binary sha256 `df173ac0...`):

| Run | Config | Wall-clock TPS | Verify |
|---|---|---|---|
| run24 | baseline | 49,992 | pass |
| run25 | `--broadcast` | 54,951 | pass |
| run26 | + `N42_FAST_PROPOSE=1` | 54,140 | pass |
| run27 | shared execution | 50,544 | pass |
| run28 | shared + fast propose + `--broadcast` | 51,274 | pass |
| run29 | frame refs + fast propose + `--broadcast` | 57,012 | pass |
| run30 | run29 + `N42_SENDER_SHARDED_DRAIN=0` | 55,848 | pass |
| run31, run32 | frame-ordered build | none | `/home` filled up; nodes exited on ENOSPC |

Round B (same binary, `--engine.disable-prewarming`):

| Run | Config | Wall-clock TPS | Verify |
|---|---|---|---|
| run33 | baseline | 55,722 | pass |
| run34 | shared execution | 52,674 | pass |
| run35 | fast propose + `--broadcast` | 59,648 | pass |
| run36 | frame refs + fast propose + `--broadcast` | 63,642 | pass |
| run37 | run36 + frame-ordered build | 62,109 | **fail** (see 5.3) |
| run38 | run37 + shared execution | 57,340 | pass |

Round C (rebuilt with the decode retry of 5.3, sha256 `4dab2b03...`, run22's flags
`--engine.disable-prewarming --cpu-cores=32`; fleets `qf9a..qf9e`). This is the round to compare
with run22 (65,530):

| Run | Config | Wall-clock TPS | Follower `new_payload`, full block, median | Verify |
|---|---|---|---|---|
| run39 | baseline | 66,293 | 534 ms | pass |
| run40 | fast propose + `--broadcast` | 69,763 | 406 ms | pass |
| run41 | frame refs + fast propose + `--broadcast` | **75,554** | 416 ms | pass |
| run42 | run41 + frame-ordered build | 73,609 | 457 ms | pass |
| run43 | run42 + shared execution | 70,196 | 368 ms | pass |

run41 is +14% over the same-round baseline. No payload decode failure or retry occurred in
round C, so the change of 5.3 is neither confirmed nor refuted by it.

### 5.1 What the flags did

- **Frame references** engage and are the best configuration in both rounds. Without frame
  order a full block is one run per transaction: 14.29 MB -> 1.18 MB, expand 23-32 ms. With the
  frame-ordered builder a full block is 96-97 runs: 14.29 MB -> 5.9 KB, encode 22-26 ms.
- **Frame-ordered build** shrinks the wire form as designed but did not move TPS (62.1k vs
  63.6k, inside run-to-run spread): transfer was no longer the limit once the form was 1.2 MB.
- **Shared execution** engages (`compact_injected=true` on every full block, zero refusals) and
  is a net loss. The follower skips the parallel batch (~80-100 ms) but pays 150-230 ms to
  decompress and deserialize a 23 MB output (`deser_ms` 73-108), and `new_payload` still takes
  560-690 ms. Execution was never the follower's main cost.
- **Fast propose** shows no gain on its own (run26 vs run25).
- With `--broadcast` every node admits every frame, so `native-flood` is held to the commit
  rate by pool back-pressure (23-28 s to deliver, 4,500-6,300 deferred frames). Blocks stay full.

### 5.2 Where the follower's `new_payload` goes (pprof, node3, run33/36/37)

- Without `--broadcast`: Ed25519 batch verification for sender recovery is the largest item
  (about a quarter of all samples), also on a shared-output hit, where the recovered senders
  are then thrown away.
- With `--broadcast` the sender cache removes most of that, and what is left on the path is
  serial: `payload-convert` decodes the 95k transactions one by one and then computes the
  transaction MPT root on the same thread (two halves of about equal size); the receipt MPT
  root runs on one more thread for a comparable time. The Gov5 pre-execution check also clones
  the block before sealing it again.
- Off the path but on the same cores: pool admission of broadcast frames
  (`admit_native_frame`, pool insert) and pool maintenance are now the largest CPU consumers.

### 5.3 run37 verification failure: a block that decodes on five nodes and not on two

Not caused by frame order. View 579 (an empty block after the load) was decoded by five
validators; node4 and the leader node5 failed with `odd number of digits at line 1 column 1351`
in the JSON header of the same 1,370-byte message, and the background import 200 ms later
failed the same way, leaving a committed block unexecuted. The same warning (`control
character`) appears once in a run of 2026-11-08 (fleet `qf7b`), before any of this work. Same
bytes, node-dependent result: the suspect is the thread-local pooled zstd decompressor, which
the code already resets after an error only if an independent retry succeeds. Root cause not
proven. Change made: the pooled context is dropped on every failure, and both decode sites
decode once more from an independent decompression before giving up
(`n42_payload_decode_retry_total{outcome}` and a warning show whether the retry recovered).

### 5.4 Disk

`/home` reached 100% during run31. Each leg writes 5-7 GB of node databases; the round scripts
now copy the pprof captures out and delete the node directories after each leg.

## 6. Status

- [x] Cycle breakdown and redesign
- [x] Shared execution behind a flag, unit tests, clippy clean
- [x] Frame-referenced transport behind a flag, unit tests, clippy clean
- [x] Fleet measurements, rounds A, B and C (section 5); best 75,554 TPS (run41)
- [x] Fallback to full execution when a shared output is refused: seventh reth patch
      `patches/reth-cached-execution-refusal.patch`, reth commit `a8490755` on branch
      `n42-v2.8.1-native-port` (branch `n42-v2.8.1` stays at `5a8b035c`, which `main` pins)
- [x] Frame-ordered builder behind a flag
- [ ] Lazy Gov5 copy

## 7. Next

1. Done: round C, 75,554 TPS with frame refs + fast propose + `--broadcast`.
2. Follower import: decode transactions in parallel, take the transaction and receipt roots off
   the single thread, drop the block clone in the Gov5 pre-execution check.
3. Shared execution stays off; it only pays if the output is handed over in memory instead of
   serialized, and if sender recovery is skipped on a hit.
4. Pool admission cost under `--broadcast`.
5. Done: the reth change is committed and locked as the seventh patch. It edits lines the
   `reth-gov5-cached-state-root` patch introduced, so a patch can no longer be reversed on its
   own against the final tree. `scripts/apply-reth-patches.sh` now reverses the whole series
   in opposite order on a temporary index and compares the result with the base tree;
   `scripts/check-reth-source.sh` calls it with `N42_RETH_CHECK_ONLY=1`. Checked on the adapted
   source and on a fresh v2.8.1 checkout (apply, apply again, check, check after an edit).
6. Gov5 body served on request only; next leader builds before the commit; millisecond timestamps.
