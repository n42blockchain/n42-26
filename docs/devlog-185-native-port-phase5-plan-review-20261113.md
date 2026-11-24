# Devlog 185 — Native-path port phase 5: plan review against the 5M target (2026-11-13)

Branch `feat/native-port`, on top of phase 4 (`80d5dad`,
[devlog-184](devlog-184-native-port-phase4-shared-exec-frame-refs-20261110.md)).
No fleet run in this step. Sources: the run41 logs (`.artifacts/native-port-p0/qf9c/logs`,
75,554 wall-clock TPS) and n42-rs at tag `fleet7-x22-5.2M-20261024`
(`docs/BREAKTHROUGH_DESIGN.md`, cited as BD).

## 1. What the 5.2M figure is

- One execution layer for seven validator keys (`F7_EL_MAP=0,0,0,0,0,0,0`,
  `N42_IMPORT_ONCE=1`, BD 10.76-10.81). BD calls the figures not comparable with independent
  execution; its record with three nodes that each execute is 1.40-1.42M (BD 10.71).
- 200,000 transfers per block, seal-to-seal 38.4-39.3 ms. TPS is counted from canonical
  blocks of the layer; persistence trails the chain.
- No signature check at the node (gateway-attested frames), a frame queue instead of the
  transaction pool, transactions root = tree over frame roots (~0.1 ms), transfers applied
  without the interpreter in 32 sender-partitioned threads, QMDB root deferred by two blocks,
  the next build starts at the seal, the layer never executes a block it built, one leader
  for 1,024 views.
- BD at the tag ends at 10.228; 10.125-10.175 are missing and there is no X22 run script.

n42-26 at 75.6k differs in every one of these. The gap is the cycle (1.22 s against 39 ms for
half the block), not the cost of a transfer.

## 2. Where the run41 cycle goes (view 49, leader node0, 95,238 transfers, 1,231 ms)

| Offset from view start | Event | Stage length |
|---|---|---|
| 0 ms | view change, build triggered | |
| 53 ms | reth creates the payload job | 53 |
| ~216 ms | builder entered (derived: built at 554 with `elapsed_ms=338`) | **163, unexplained** |
| 554 ms | payload built (drain ~51, packing 153, finish 92, cache 26) | 338 |
| 704 ms | Gov5 normalization roots (66) and serialization done | 150 |
| 733 ms | frame references encoded, block data sent | 29 |
| 1,070 ms | leader's own `new_payload` of the normalized block returns (cache hit, no execution) | **337** |
| 1,074 ms | proposal released | |
| ~1,212 ms | follower votes (import 426-462 ms, started at 733) | 138 |
| 1,231 ms | commit (R1 138, R2 18) | 19 |

Findings that change the plan:

1. **The leader takes 1.07 s to release a proposal; followers wait for it.** Follower import
   overlaps the leader's own import and ends ~100 ms after it. Devlog-184 treated the follower
   as the limit; the leader path is 88% of the cycle.
2. **The leader imports its own block for 337 ms without executing it.** Normalization gives
   the block a new hash, so it goes through `new_payload` like a foreign block: decode of
   95k transactions, transaction MPT root, receipt root, QMDB root job (48 ms), while the Gov5
   body encode (181 ms) competes for the same cores.
3. **163 ms pass between job creation and the builder's first line.** Cause unknown. A log
   line `N42_BUILD_ENTRY` now marks the entry.
4. Execution itself is 129 ms of the 1,231 (`evm_exec_ms`).

## 3. Revised plan

Ordered by measured milliseconds on the cycle; each step is measured on the round C
configuration (run41 flags) before the next.

**Stage A — shorten the chain inside the present structure (target ~0.45 s, ~200k TPS)**

1. Leader hands the built block to the engine as an executed block (reth
   `EngineApiRequest::InsertExecutedBlock`) with the normalized header, instead of
   `new_payload`. Removes ~340 ms from the leader. The QMDB root job must still commit the
   block to the store.
2. Follower import under 150 ms: decode in parallel, transaction and receipt roots off the
   single thread, no block clone in the Gov5 pre-execution check. After step 1 this is the
   critical path (430 ms from block data).
3. Find and remove the 163 ms before the builder starts.
4. Normalization inside the builder's finish (QMDB root and Gov5 receipts root produced once,
   header built in its final form): removes most of the 150 ms and the re-keying.
5. Gov5 body encoded only when a peer asks for it (181 ms of CPU per block on the leader).

**Stage B — the n42-rs structure (needed for anything above ~0.3M)**

6. One execution layer for the validators of one machine: seven consensus services in one
   node process over one `ExecutionLayer`, import once. Replaces `N42_SHARED_EXECUTION`
   (serialized output, measured as a loss); the wire form stays for separate machines.
7. Next build starts at the seal of the previous block; one leader for N views (protocol
   setting of this route).
8. Transactions root = tree over frame roots for frame-described blocks (genesis flag);
   frame store keeps decoded transactions with senders, so a referenced block needs neither
   decode nor MPT root.
9. Frame queue in place of the pool on this route; state root deferred by two blocks.
10. Larger blocks (200k), then output shards and persistence tuning as BD X3-X22.

Dropped from devlog-184's list: further work on serialized shared execution; frame-ordered
build stays as a flag (it becomes useful with step 8).

## 4. Stage A, first cut: the import path (leader and follower alike)

Order changed after a second look: the leader's own import (337 ms) and the follower's
(426-462 ms) overlap, so removing only the leader's (step 1) does not shorten the cycle.
Both run the same `new_payload` code, so the shared path goes first.

Attribution of the run41 follower profile (node3, samples on the import threads):

| Thread | Samples | What it does |
|---|---|---|
| `payload-convert` | 537 | transactions root while converting the payload (134), **the same root again in the pre-execution check (263)**, a clone of the whole block for that check (45) |
| `receipt-root` | 369 | Ethereum receipt trie; the Gov5 header commits to the native receipts root instead, the trie root is only fed back into a comparison with itself |
| `engine` | 618 | among others: clone of the block and senders for the post-execution check, serial sort of the QMDB operations, receipt trie again when the task gave no result (cached execution on the leader) |

Engine metrics for the same run (node3, per full block): `new_payload` ~420 ms = execution
stage 222 (parallel batch itself 94) + state root 100 + post-execution 30 + rest.

Changes (uncommitted):

- `n42-consensus/src/adapter.rs`, Gov5 profile:
  - `validate_block_pre_execution_with_tx_root` implemented. It uses a supplied root, or
    derives it from the real body, and runs the inner rules on a copy without transactions
    unless the block has blob transactions or Osaka is active (the two rules that read them).
  - `validate_block_post_execution` no longer builds the receipt trie or clones the block: the
    inner rules read only the header, so they get a header-only block, the header's own
    receipts root and the logs bloom (taken from the task if present, else folded from the
    receipts). Native receipts root, gas used, bloom, requests hash and access-list hash are
    checked as before.
  - `uses_receipt_root()` is `false` for the Gov5 profile.
- reth (to become the eighth patch):
  - `FullConsensus::uses_receipt_root()` (default `true`); when `false` the validator neither
    spawns the `receipt-root` task nor waits for it.
  - `spawn_convert_and_validate` passes the converted header's transactions root to the
    pre-execution check for payload input. Condition for this to be sound: a root in the
    payload root cache was computed by this node from the body.
- `n42-node/src/exec_cache.rs`: a peer's compact output no longer puts its transactions root
  into that cache (it was the one place a foreign root entered). A follower with shared
  execution derives the root in the conversion instead of in the check: same cost as before.
- QMDB operations sorted with rayon (root job and leader normalization).
- `N42_BUILD_ENTRY` log line at builder entry.

Tests: `adapter::tests::gov5_post_execution_checks_bloom_and_gas_without_a_receipt_trie`,
`adapter::tests::gov5_pre_execution_binds_the_transactions_root_to_the_header`,
`exec_cache::tests::peer_transaction_root_is_not_cached_and_a_local_root_is_evicted_by_hash`
(replaces the test that asserted the peer root was cached). Workspace clippy clean; unit
tests of n42-consensus, n42-node, n42-consensus-service and reth-engine-tree pass.

Considered and deferred: handing the leader's block to the engine as an executed block
(reth `InsertExecutedBlock`). It needs a new engine entry point, a QMDB commit outside the
root job and a hashed post state the builder skips today; it pays only once the follower
import is shorter than the leader's.

## 5. Measurement rounds D, E and F (2026-11-13)

Scope of every figure: 7 validators on one 256-core host, H2-v4 participant mode with QMDB,
2G gas (95,238 transfers per block), 1.6M gateway-attested 0x50 transfers sent with
`native-flood --broadcast`, node flags `--engine.disable-prewarming --cpu-cores=32`,
wall-clock TPS over the blocks that carry the load, all seven nodes agreeing on roots and
hashes. Raw files: `.artifacts/native-port-p0/runNN.out`, `runNN-tps.txt`,
`runNN-metrics-node{0,3}.txt`, `runNN-nodeN-pprof-*`, logs under `qfXX/logs`.

| Run | Configuration | Wall-clock TPS |
|-----|---------------|----------------|
| run41 (round C) | frame refs + fast propose | 75,554 |
| run44 / run45 (D) | same, with the import cuts of section 4 | 76,552 / 78,142 |
| run46 (D) | baseline (no frame refs), same build | 70,021 |
| run47 / run48 (E) | D + Gov5 copy encoded beside the send, no receipt copies | 80,832 / 80,317 |
| run49 / run50 (F) | E + `N42_FRAME_QUEUE=1` | 94,149 / 93,911 |
| run51 / run52 (G) | F + senders kept with held frames | 99,294 / 99,942 |
| run53 (H) | G + no broadcast clone of the built output | 102,996 |
| run54 (H) | run53 + `N42_PAYLOAD_PRECACHE=0` | 106,716 |
| run55 (I) | run54 + QMDB root in the builder, payload converted from a reference | 119,157 |
| run56 (I) | run55 with `N42_BUILDER_QMDB_ROOT=0` | 109,951 |
| run57 / run58 (J) | run55 + parallel QMDB operations, BLAKE3 registry key | 119,949 / 119,837 |
| run59 (K) | run57 + `N42_LEADER_TENURE=32` | 121,844 (load phase; the leg's check FAILED later, see 8.2) |
| run60 (K) | run59 + `N42_BUILD_ON_SEAL=1`, first version | 8,566 (broken: four blocks per tenure, then empty builds; see 8.2) |
| run61 (L) | run60 with the two fixes of 8.2 | 73,969 (PASS; 21 view timeouts, see 8.3) |
| run62 (L) | run59 repeated (tenure 32 only) | 121,559 (PASS) |
| run63 (M) | run61 repeated with the import failure logged | 93,414 (PASS) |
| run64 (N) | + batch import opens the parent through the validator | 178,437 over 12 blocks, then **FAILED** (see 8.5) |
| run65 (N) | tenure 32 only, same build | 120,828 (PASS) |
| run66 (O) | build ahead + gated frames held + validation not delayed | 59,821 (PASS) |
| run67 (O) | run66 repeated | 107,879 (PASS) |
| run68 (P) | build ahead, ingest high water 2M | 152,364 over 16 blocks, then **FAILED** (see 8.8) |
| run69 (P) | tenure 32 only, ingest high water 2M | 121,737 (PASS) |
| run70 (Q) | tenure 32 + build ahead, ingest high water 2M | 146,604 (PASS) |
| run71 (Q) | same as run70 | FAILED after the flood (partial window 150,786) |
| run72 (R) | tenure 32 + build ahead, high water 2M, cache prune + no duplicate import | 192,740 (PASS) |
| run73 (R) | same as run72 | 192,089 (PASS) |
| run74 / run75 (S) | R + built block applied to the frame queue at build time | 194,356 / 193,798 (PASS) |

Round D. Leader self-import fell from 337 to 87 ms and follower `new_payload` from 7.36 to
5.70 s over the load, but the proposal was now held by the serial Gov5 block encoding
(179 ms) that ran after the send.

Round E. `execution_bridge.rs` starts the Gov5 encoding in `spawn_blocking` as soon as the
leader validation is spawned and awaits it after the broadcast. reth's import path gained
stage timers (`n42_import_stage_ms`: `collect`, `recovered_copy`, `merge_transitions`,
`take_bundle`, `await_convert`) and skips the receipt copies when the consensus needs no
receipt root. Per full block on a follower: waiting for decoded and recovered transactions
76 ms, copying recovered transactions 16 ms, taking the bundle 8 ms.

The leader still spent 151 ms between reth's "New payload job created" and builder entry.
The node0 profile showed 2,190 of 2,781 tokio samples in the transaction pool (706 in the
canonical-update pass, 1,484 inserting). The first thing the builder does is read the pool
size, so it waited behind the pass that removes the parent block's 95k transactions.

Round F: the frame queue (`crates/n42-node/src/frame_queue.rs`, `N42_FRAME_QUEUE=1`, needs
`N42_FRAME_REF_BLOCKS=1`).

- Design. An admitted native frame is not inserted into the pool. It is queued in arrival
  order as ready `ValidPoolTransaction`s; the builder walks frames from the front
  (`QueuedFrameTransactions`, 64 frames per lock) and falls through to the pool for
  anything that did not arrive as a native frame. A task on the canonical-state channel
  marks each block's transactions as mined (`mark_mined`), locating them through the frame
  store. The builder waits (at most 500 ms) until its parent has been applied.
- Late frames. A frame can arrive after the block that carries its transactions. Mined
  hashes that no queued frame names are remembered for 64 blocks; `push` and `mark_mined`
  both locate under the queue lock so one always sees the other.
- Duplicates and stale frames. A transaction the store files under an earlier frame is
  queued once. A frame not used up within `N42_FRAME_QUEUE_TTL_SECS` (default 120) is
  dropped and counted in `n42_frame_queue_expired_transactions_total`.
- Ingest credit and the pool-depth diagnostics count queued transactions.
- Cost: queued transactions are invisible to pool RPCs (pending lookup, pending nonce).
- Alternative considered: keep the pool and shorten its update pass. It leaves the
  per-transaction insert on every node and still serialises the builder behind the pass.
- The payload builder is now bound to `N42PooledTransaction` instead of any pool
  transaction with an N42 consensus form (the node has only that pool type).

Result: job created to builder entry 151 ms -> 0 ms (parent wait under 0.01 ms), full-block
cycle 1.16 s -> 0.97 s. Cycle of view 46 in run49: build start to job 68 ms, build 300 ms,
normalization and serialization 121 ms, send, follower import about 400 ms, votes.

Tests: six unit tests in `frame_queue::tests`. Clippy clean on n42-node and n42-node-bin.

Round G: senders kept with held frames. On a follower 30% of the transactions in a block
missed the sender cache (1,123,791 hits, 476,209 misses in run49) and were verified one by
one; 729 of 928 samples of node0's `cpu` pool were Ed25519. The cache is a fixed table of
2^20 slots where a colliding insert replaces the entry, so with about 250k transactions
waiting a quarter of them are overwritten before their block arrives. `StoredFrame` now
carries the sender admission authenticated for each transaction
(`FrameStore::insert_with_senders`, `sender_of`), and `tx_iterator_for_payload` asks the
frame store on a cache miss before it verifies. A larger table was the alternative; it
costs about 1 GB per node for a 2% miss rate and stays lossy. Result in run51: 1,337,984
cache hits, 262,016 answered by the frame store, no verification; waiting for recovered
transactions 91 -> 43 ms per block, follower `new_payload` 346 -> 291 ms per full block,
cycle about 0.90 s. The leader path (about 645 ms to the proposal) is the longer one again.

Round H removes two leader-side copies. In H2-v4 participant mode the built execution
output is never sent to peers (unless shared execution is on), yet the builder cloned it
for the broadcast slot; normalization now borrows the copy kept for the node's own import
and puts it back (`take_gov5_normalization_roots`), `cache_ms` 25 -> 2.3 ms per block.
`N42_PAYLOAD_PRECACHE=0` turns off reth's copy of the parent's changed accounts into the
payload job's read cache (`BasicPayloadServiceBuilder::with_pre_cache_state`); the two
legs differ by 3.6%, which is one run each and within the spread seen between repeated
legs before, so the switch is kept as an option, not a default. All seven roots agree in
both (539 and 553 common blocks).

The leader timeline of a full block in run54 (view 49, node0), from the view change:
payload job created +25 ms (waits for the finalize forkchoice), builder entered +56,
packing done +224 (143 ms), finish 78 + cache 12, payload built +322, normalization
starts +354 and takes 73 (operations 29, QMDB root 41), serialized +445, frame references
+463, sent +471. Followers import in about 313 ms and the round closes at +856. The
proposal itself leaves at +590, after the leader's own import (86 ms) and the Gov5
encoding (149 ms), but the followers' import ends later than that, so it does not bind.

Round I takes two of these. The QMDB root is derived in the builder, on a second thread
beside the block assembly (`builder_qmdb_store`, set from `main.rs`, off with
`N42_BUILDER_QMDB_ROOT=0`); it depends only on the merged state changes, which are final
before the block is assembled. The builder files `(state root, receipts root)` under its
block hash and normalization picks them up (`take_built_gov5_roots`), falling back to
deriving them when nothing is filed or shared execution is on. The restored-slots key is
derived from the builder's own environment and context, the same inputs the tracking
executor uses; the native builder integration test now runs this path and compares the
root with an independently built tree. Second, the built block is converted to its payload
from a reference with the transactions encoded in parallel (`blobless_block_to_payload`;
blocks with blob transactions keep the old path because the sidecar's versioned hashes
come from the transactions).

Result: 119,157 TPS in run55 against 109,951 with the builder root switched off (run56,
same binary), all roots agree (544 / 565 common blocks). In run55 view 49 the block is
sent +384 ms after the view change (run54: +471); "payload built" to the first emit step
is 25 ms (run54: 104 ms including normalization); the root thread takes 16 ms per block
on average and does not lengthen finish (24 ms per block average over all blocks in both
legs). The cycle is about 0.77 s: leader 384 ms, hand-over about 50 ms, follower
`new_payload` about 304 ms, votes about 30 ms.

Round J: `gov5_qmdb_operations` derives large change sets on the rayon pool (each
operation hashes its key; about 25 ms on the importing thread by sample share in run55),
and `restored_slots_key`, which only names an entry of the in-process registry, uses
BLAKE3 instead of Keccak-256 over the block's 3 MB of transaction hashes (about 11 ms,
twice per import). Two identical legs: 119,949 and 119,837 TPS, follower `new_payload`
304 -> 294 ms per full block. Against run55 (119,157) that is 0.6%, a gain too small to
separate from run-to-run variation with one earlier run; the two legs themselves differ
by 0.1%.

## 6. Status

- [x] Phase 4 committed (`c09e332`, `80d5dad`); reth commit `a8490755` on
      `n42-v2.8.1-native-port`
- [x] Plan review, this document
- [x] Import-path cuts of section 4, Gov5 encoding beside the send, stage timers
      (uncommitted)
- [x] Frame queue, 94.1k TPS (uncommitted)
- [x] Senders kept with held frames, 99.9k TPS (run52, uncommitted)
- [x] No broadcast clone, payload pre-cache switch, 103.0k / 106.7k TPS (run53 / run54, uncommitted)
- [x] Builder-side QMDB root and reference payload conversion, 119.2k TPS (run55, uncommitted)
- [x] Parallel QMDB operations, BLAKE3 registry key, 119.9k / 119.8k TPS (run57 / run58, uncommitted)
- [ ] reth edits exported as the eighth patch
- [ ] Stage A step 1 (leader executed-block insert), deferred

## 7. Next

Follower `new_payload` (about 304 ms per full block) is now close to the leader's 384 ms
and both bound the cycle. Leader: 25 ms waiting for the finalize forkchoice before the job
exists, 30-45 ms in the builder waiting for the frame queue to see the parent applied,
packing 165 ms, finish 95 ms, serialization and frame references 45 ms. Follower:
execution, QMDB root, waiting for recovered transactions. Then stage B (leader tenure and
build-on-seal change the leader rotation and need the user's decision first).

## 8. Plan adjustment after round J: stage B in the n42-rs order

Stage A ended at 120k against its 200k target, and the last round showed that one more
cut inside the present structure is worth under 1%. The target is 40 times away. The
user asked for the plan to be adjusted (2026-11-13). What the present cycle consists of
(0.77 s): leader path to "sent" 384 ms, hand-over 50, follower import 294, votes 30 —
the leader builds only after the commit and the followers execute only after the leader
has finished, so the two long stages run one after the other.

How n42-rs arranges it at the 5.2M tag (read from the tag, not changed):

- Leader tenure is a chain rule, `(view / tenure) % n`, genesis
  `config.hotstuff.leaderTenure` (`h2-consensus/src/validator/selection.rs`), 1,024 in
  the record run. Timeouts are unchanged.
- Build-on-seal (`N42_BUILD_ON_SEAL`) does not relax consensus: right after "proposal
  sent" for N the leader requests N+1 on its own uncommitted block; N+1 is proposed in
  the next view under QC(N) like any block. Building two ahead was measured as a 5% loss
  and is not in the record.
- One execution layer for the seven keys: seven validator processes, one layer, an
  import-once registry by block hash; the six other keys vote on the owner's check.
  The leader's own block is handed to the engine as executed, never run again.
- State root, receipts root, bloom and gas of block N-2 in header N (genesis
  `deferredExecutionDepth: 2`); transactions root = tree over frame roots.

Revised order, each step behind a switch that is off by default and measured on the
run55 configuration:

1. **Leader tenure and build-on-seal** (this section, round K). Expected: the cycle
   becomes the longer of the leader's build path and the followers' import, not their
   sum.
2. **Build path per block**, which then is the cycle: packing 165 ms and finish 95 ms
   for 95k transfers against 39 ms for 200k in n42-rs. Frame-tree transactions root and
   the deferred root belong here; both change the header rules and need a genesis flag.
3. **Import once per host** (the six co-located validators vote on one import), which
   also frees the cores the leader's build competes for.
4. Larger blocks.

### 8.1 Implementation of step 1

- `N42_LEADER_TENURE` (default 1): `leader_index(view, n, tenure)` in
  `n42-consensus/src/validator/selection.rs`, used by `LeaderSelector::leader_for_view`,
  the H2 wrong-leader check (`h2_finality.rs`) and the orchestrator's diagnostics. It is
  read from the environment for the measurement route only; all validators must agree,
  so a network that adopts it has to carry it in genesis.
- `N42_BUILD_ON_SEAL=1` (H2-v4 participant mode). The leader's own `new_payload` of block
  N reports on the build channel as soon as it returns Valid
  (`PayloadBuildReady::own_block_validated`), about 200 ms before the proposal is released
  (the Gov5 copy is encoded and pushed in between). If this validator also leads the next
  view, `build_ahead_on` starts the build for `(view + 1, parent N)` through
  `do_trigger_payload_build_on`. The finished payload normalizes, is sent to the
  followers (their eager import has no view condition) and validated as usual; its
  `PayloadBuildReady` is held in `ahead_ready` while its view has not started and handed
  to consensus by `take_ahead_build` at the view change, if the LockedQC then names its
  parent. Otherwise it is dropped and the normal build runs. A block validated ahead of
  its own view waits in `ahead_validated`: the leader runs at most one block ahead.
- Forkchoice: the execution layer has the uncommitted own block as head while the child
  is built. `finalize_head_for` keeps that head when the block's parent is finalized
  (safe and finalized still name the committed block), so the layer is not moved back
  under the running build.
- Not changed: proposal validation, justify rule, votes, timeouts. Known limits of the
  switch: with the frame queue, the transactions of an own block that is not committed
  are not offered again; non-participant (native optimistic) mode does not build ahead.
- `cargo fmt` on the two crates reformatted files this work did not touch; those were
  restored, the reformatting inside touched files stays.

### 8.2 Round K (run59 / run60, 2026-11-14): what the first version got wrong

Raw output: `.artifacts/native-port-p0/run59*`, `run60*`, logs `qfHa/logs`, `qfHb/logs`.
One host, seven validators, 1.6M transfers, wall clock.

- **run59, tenure 32 only: 121,844 TPS** over blocks 38..55 (run57: 119,949), all hashes
  agree at block 55. The leg is still reported FAILED: long after the load, at view 342
  (empty blocks every 114 ms), node4 rejected one block with "gov5 payload disagrees with
  its remembered native header on parentHash" and stopped executing. The other five
  followers accepted the same 1,369 bytes. Not seen in any earlier round (checked qfEa,
  qfFa, qfGa, qfGb). Cause not found; the check now logs both parent hashes and numbers.
- **run60, tenure 32 + build ahead: 8,566 TPS**, 1,428,570 of 1.6M transfers committed,
  roots agree. For the first four views of a tenure the mechanism did what it should:
  views 64..67 committed 481–530 ms apart with full blocks (proposal at +201..257 ms,
  first vote round ~255 ms, second ~20 ms). Then two faults in the new code took over:
  1. *One guard for two builds.* Starting the build for view N+1 overwrote
     `building_on_parent`, which still guarded view N's build until its proposal was
     released. A pending leader-build wait for view N then passed the guard and built
     view N a second time, on the stale parent. That build finds every queued
     transaction ahead of its state, takes 1.5 s and yields an empty block; views time
     out behind it.
  2. *The frame queue forgot reorged blocks.* Blocks built during that confusion became
     canonical in the leader's execution layer and were then replaced. The queue had
     already dropped their transactions (the limit noted in 8.1), so every later build
     found only nonce gaps: empty 1.5 s builds for the rest of the run.

Fixes (uncommitted, measured in round L):
- `ahead_building` holds the context of the build started ahead; `building_on_parent`
  keeps guarding the current view. A trigger for a context the ahead build already
  covers is dropped; a failed ahead FCU schedules no retry (the view's own build starts
  with the view).
- `FrameQueue::mark_mined` keeps what each of the last 8 canonical blocks took
  (`Taken`: sequence, root, position, frame length, transaction).
  `FrameQueue::revert_mined(number)` puts it back in its place, rebuilding a used-up
  frame entry if needed; the canonical task calls it for `notification.reverted()`
  before marking the new chain. Test
  `a_reorged_block_gives_its_transactions_back_in_their_place`. This removes the limit
  noted in 8.1 for blocks the execution layer made canonical.

What the four good views say about the next limit: the build for N+1 starts only when
the leader's own `new_payload(N)` returns (~150 ms after N is built), then waits ~60 ms
for the canonical notification of N to clear the queue. With the build at ~300 ms the
proposal for N+1 is ready ~200 ms into its view, and the followers' import (~255 ms)
follows it. Starting the next build when N is *built* (the block inserted as executed,
its transactions taken from the queue at build time) would overlap those 200 ms too.

### 8.3 Round L (run61 / run62, 2026-11-14)

Raw output: `.artifacts/native-port-p0/run61*`, `run62*`, logs `qfIa/logs`, `qfIb/logs`.

- **run62, tenure 32 only: 121,559 TPS, PASS** (521 common blocks, all roots agree). The
  run59 rejection did not recur, so it is intermittent; still unexplained.
- **run61, tenure 32 + build ahead: 73,969 TPS, PASS**, 1,534,048 transfers inside the
  measured block range, 21 view timeouts. The duplicate build of the current view is
  gone. What remains: a follower's eager import of a block *with transactions* does not
  come back Valid when the block arrives before its parent is committed. First case:
  view 42 (4,000 transfers) reached the followers 28 ms before the Decide of view 41;
  no follower voted, the view timed out after 2 s, and the leader's builds on its own
  uncommitted blocks were discarded one after another. Empty blocks in the same order
  import fine (the idle phase does it every view), and in run60 the full blocks of views
  64..67 arrived after their parents' commit. The follower logged nothing: the
  non-Valid and error branches of the eager import were `debug!`. Both are `warn!` now,
  with the parent hash and elapsed time; round M (run63) repeats the leg to read them.

### 8.4 Round M (run63, 2026-11-14): why the follower import failed

Raw output: `.artifacts/native-port-p0/run63*`, logs `qfJa/logs`. One host, seven
validators, 1.6M transfers, wall clock: **93,414 TPS, PASS** (1,548,896 transfers in
blocks 40..56, 16.58 s). The leg differs from run61 only in logging; the spread between
the two (74.0k / 93.4k) is the number of views lost to the fault below.

The warning lines name the cause. Two kinds, always in this order:

1. `follower eager import: failed ... error=the parent's state could not be opened`
   (views 40, 58, 60; 3-56 ms after the payload arrived);
2. `not accepted ... status=Syncing` for the blocks built on the failed one.

The first is `NotParallel::NoState` from the transfer batch import. The batch opens one
more view of the parent per worker batch, and it did so through
`BlockchainProvider::state_by_block_hash(parent)`. That lookup knows canonical blocks,
the database, and the single *pending* block - a tree block whose parent was the
canonical head when it was inserted. With build ahead, block N+1 reaches a follower
while N is in the engine tree and N-1 is not yet canonical (its FCU follows the Decide),
so N is neither canonical nor pending and cannot be opened by hash. Empty blocks never
take the batch path, which is why the idle phase was fine. Without build ahead the
parent is always committed first, so earlier rounds never met it.

**Fix (reth worktree + n42-execution, uncommitted).** The payload validator already
holds an overlay state provider factory for the exact parent, tree blocks included; it
is what the block's own executor reads. `ConfigureEvm::try_execute_import_batch` takes
a new argument `open_parent: &(dyn Fn() -> Option<EvmStateProviderBox> + Sync)`;
`execute_block` passes `make_state_provider(false)`, `BasicBlockExecutor` passes
`|| None`. `N42EvmConfig` asks `open_parent` first and falls back to the by-hash
provider. This belongs to the pending eighth reth patch.

Round N (`roundN.sh`): run64 = tenure 32 + build ahead, run65 = tenure 32 only (checks
that the batch import through the validator's provider costs nothing).

### 8.5 Round N (run64 / run65, 2026-11-14)

Raw output: `.artifacts/native-port-p0/run64*`, `run65*`, logs `qfKa/logs`, `qfKb/logs`.

- **run65, tenure 32 only: 120,828 TPS, PASS** (1.6M transfers, blocks 40..58, 13.24 s).
  Within the spread of run59/run62: opening the parent through the validator costs
  nothing.
- **run64, tenure 32 + build ahead: the leg FAILED** ("a committed QC has no executed
  block after the FCU grace period"). Before it broke, blocks 43..54 carried 1,038,452
  transfers in 5.82 s wall clock = 178,437 TPS, with full blocks committing 485-512 ms
  apart. This is a partial window of a failed leg, not a result; it shows what the
  cycle is once the parent-state fault is gone. No "state could not be opened" remains.

What broke it, on node2 only:

1. `N42_FRAME_REF: cannot rebuild block payload ... 32 of 97 referenced frames are not
   held` at view 53, growing to all frames of later blocks.
2. The ingest gate refuses a native frame when the node's own pending count
   (`FrameQueue::pending` under the frame queue) is above the high-water mark. A
   follower's count falls only when a block becomes canonical *there*, a cycle or two
   after the leader's. node2 refused frames the leader had admitted; the leader's
   blocks named them; node2 could not rebuild those blocks and had to wait about a
   second for the full copy each time; its chain stopped advancing, so its queue never
   drained and the gate stayed closed (`accepted=1,293,000`, `deferred` growing by
   50k/s to 9.4M while the other six accepted all 1.6M).
3. At view 64 the tenure passed to node2, whose engine was still syncing
   (`fork_choice_updated did not return payload_id`). Every view from 64 on timed out
   with a growing timeout, for the rest of the run.

Point 3 is the price of a tenure: one leader that cannot build holds 32 views. It is
left as is for now (n42-rs runs a tenure of 1024); it has to be bounded before tenure
is used outside the benchmark - e.g. a leader that knows its engine is behind gives the
view up at once.

**Fix for point 2 (`crates/n42-node/src/ingest.rs`, uncommitted).** With the frame
queue and frame references both on, a frame the gate refuses is still authenticated
and put into the `FrameStore` (`admit_native_frame(.., hold_only = true)`), but not
queued; the ack still reports it as deferred, so the sender offers it again. Its root
is remembered in `HELD_FRAMES`, and the later admission queues it although the store
already holds it (`stored || held_before`). Transactions mined in between are filtered
by the queue's `early` set as before. Cost: a refused frame is authenticated on every
offer; the store's own capacity bounds the memory. Alternative considered: a higher
gate for followers than for the leader - rejected, ingest does not know the role and
the margin would be a guess.

Round O (`roundO.sh`): run66 and run67, both tenure 32 + build ahead (two legs because
the failures so far were intermittent).


### 8.6 Leader cycle in run64 and the validation that started late

Leader timeline of a full ahead-built block (node1, views 48-50 of run64, ms after the
builder finished): normalization done and payload serialized +34, broadcast +81,
`new_payload` reaches the engine +81, Valid +152, FCU and payload job +180, builder
entered +198, built +500. The cycle is the 300 ms build plus about 190 ms between
"built" and the next builder entry.

The leader's validation of its own block is spawned *before* serialization so that the
two overlap, yet the engine logged the payload only after the broadcast. A task spawned
from a tokio worker goes to that worker's own (LIFO) slot, which other workers do not
steal from; the emitting task then serialized and encoded for 50-80 ms without
yielding. `handle_built_payload` now yields once after spawning the validation
(`tokio::task::yield_now()`), so the engine gets the block while the payload is being
serialized. Expected: about 50-80 ms off the cycle. Round O's binary includes it (the
edit landed three seconds before the round's build started).

Next in the same path: 31 ms `compress_ms` on a full block although the frame-reference
encoding is what is sent; FCU + job creation + builder entry (~45 ms).

### 8.7 Round O (run66 / run67, 2026-11-14): the frame supply is the limit

Raw output: `.artifacts/native-port-p0/run66*`, `run67*`, logs `qfLa/logs`, `qfLb/logs`.
Both legs tenure 32 + build ahead, one host, 1.6M transfers, wall clock:
**run66 59,821 TPS, run67 107,879 TPS, both PASS**, every node accepted all 1.6M
transfers. Below tenure alone (121k), and unstable.

- The yield works: the leader's own block reaches the engine 12 ms after the builder
  finishes (81 ms in run64) and the next build starts at +80 ms (+152). A good cycle is
  about 450 ms built-to-built.
- No node falls out any more, but about every second full block cannot be rebuilt by
  *any* follower ("24 of 96 referenced frames are not held", all six at the same
  instant); the full copy takes ~640 ms more, the commit of that view takes ~1 s, and
  because the leader builds one block ahead at most, its next build waits for it.
- Cause: the gate itself. The sender does not go on while a node defers a frame, so a
  follower's stream stops whenever its queue is over the high-water mark (about 254k
  here: block cap plus headroom). The leader's queue drains one or two cycles earlier,
  its stream keeps going, and it builds from frames the followers have not been sent.
  The ingest counters show it: 76k transfers/s admitted per node during the run, and
  the leader 57k transfers ahead of the followers. Holding refused frames (8.5) cannot
  help with frames that were never sent.

With a cycle under 0.5 s the chain takes transfers faster than the gated ingest admits
them, so the admission limit sized for one block per second is the wrong size. Round P
(`roundP.sh`) lifts it by configuration, no code change: `N42_INGEST_HIGH_WATER=2000000`
on both legs (run68 build ahead, run69 tenure only). Scope note for these legs: the
whole 1.6M load may sit in the queue before it is mined; the TPS figure is still
committed transfers over the wall-clock span of the blocks that carry them.

To do after the measurement: a gate rule that cannot starve followers of frames the
leader takes (the leader, not each node, should decide what is admitted), instead of a
large constant.

### 8.8 Round P (run68 / run69, 2026-11-14): a second duplicate build

Raw output: `.artifacts/native-port-p0/run68*`, `run69*`, logs `qfMa/logs`, `qfMb/logs`.
Both legs with `N42_INGEST_HIGH_WATER=2000000`.

- **run69, tenure 32 only: 121,737 TPS, PASS** (1,074,618 transfers in blocks 36..48,
  8.83 s). The lifted gate changes nothing for the 0.77 s cycle: there the supply was
  not the limit.
- **run68, tenure 32 + build ahead: the leg FAILED** (same checker error as run64).
  Before that, blocks 42..57 carried 1,313,432 transfers in 8.62 s = 152,364 TPS; a
  partial window of a failed leg, not a result. No follower failed to rebuild a block
  any more, so the supply problem of round O is gone. Views 44..50 committed 449-487 ms
  apart.

From view 51 the cadence alternated between ~1 s and ~0.37 s, then forks and the
checker failure. Cause: the guard against a duplicate build holds one context. When
block N+1 (built ahead) is accepted by the leader's engine, the build of N+2 starts and
`ahead_building` moves on to it - while N+1's proposal is still being prepared and is
not in `ahead_ready` yet. If view N+1 begins in that window, the ordinary trigger finds
neither and builds view N+1 a second time on the same parent. That build finds every
queued transaction already taken, waits 1.5 s, and blocks the payload builder (one job
at a time) for the real next block; later its empty result competes with the first
block (two blocks per view, reorgs, `no state found for block`).

Fix: `ahead_emitting: Option<PayloadBuildContext>` on the orchestrator, set in
`build_ahead_on` when the own block is validated, cleared when its `PayloadBuildReady`
or its build-completion signal arrives (the latter always arrives, so it cannot stick),
and on a view change that makes this node a non-leader. `do_trigger_payload_build_on`
skips a non-ahead trigger whose context equals it.

Round Q (`roundQ.sh`): run70 and run71, both tenure 32 + build ahead + high water 2M.


### 8.9 Round Q (run70 / run71, 2026-11-14): the cache wipe behind the 1 s views

Raw output: `.artifacts/native-port-p0/run70*`, `run71*`, logs `qfNa/logs`, `qfNb/logs`.
One host, seven validators, 1.6M transfers, wall clock, same switches on both legs.

- **run70: 146,604 TPS, PASS** (1,230,112 transfers in blocks 41..55 over 8.39 s; 542
  common blocks, all seven roots and hashes agree). No duplicate build any more.
- **run71: FAILED** the final check (`a committed QC has no executed block after the FCU
  grace period`) after the flood; its window (150,786 TPS over blocks 41..55) is a
  partial window of a failed leg, not a result.

Both legs show the same cadence: every second view takes about 1 s
(run70 leader: v46 943 ms, v47 386, v48 706, v49 339, v50 966, v51 358, ...).

Cause of the 1 s views. With build ahead, block N+1 reaches a follower before the
follower finalizes block N. `finalize_committed_block` then ran
`pending_block_data.clear()` and threw block N+1 away. The proposal for N+1 found no
data (`ExecuteBlock requested pending_data=false`), the follower fell back to the full
Gov5 copy of the block (14 MB, handled on the consensus loop), and the loop did not
get to the finished eager import until that copy had been processed: the vote left
about 250 ms after the import was done (node0, view 46: import accepted 07.279, vote
07.524).

Fix 1: `prune_pending_block_data_through(view)` keeps cached blocks of later views
(view taken from `recent_block_data`); the four finalize paths use it instead of
`clear()`.

Cause of the run71 failure. A follower that has committed a block it is still
importing gets `ExecutionReady deadline expired` after 150 ms and starts a background
import of the same block. Blocks take ~330 ms to import here, so the block was executed
twice at once (import time 330 -> 650-800 ms on node5/node6), the node fell behind,
and the late background import then sent a forkchoice update for a block behind the
head (`too deep reorg`), which left one commit without a finalized execution.

Fix 2: `eager_imports_in_flight` (a shared set with a drop guard in the eager import
task). When the deadline fires for a block whose eager import is still running, the
deadline is armed again instead of starting a second import. A failed eager import
drops the guard, so the fallback still runs.

Not fixed: a background import that completes after the head has moved past its block
still sends a backward forkchoice update. It should be treated as done when the block
is already an ancestor of the head.

Round R (`roundR.sh`): run72 and run73, same switches, with both fixes.

### 8.10 Round R (run72 / run73, 2026-11-14): 192k on both legs

Raw output: `.artifacts/native-port-p0/run72*`, `run73*`, logs `qfOa/logs`, `qfOb/logs`.
One host, seven validators, 1.6M transfers, wall clock.

- **run72: 192,740 TPS, PASS** (1,600,000 transfers in blocks 40..58 over 8.30 s; 562
  common blocks, all seven roots and hashes agree).
- **run73: 192,089 TPS, PASS** (1,600,000 transfers in blocks 41..59 over 8.33 s; 563
  common blocks agree).

No 1 s views, no `ExecutionReady deadline expired`, no backward forkchoice update, no
rebuild failure. Full views commit 360-510 ms apart (mostly 420-480).

Leader cycle in run73 (views 49..51), about 460 ms per block:

| Step | ms |
|---|---|
| build (packing ~160, finish ~85) | ~300 |
| built -> own block accepted by the engine | ~78 |
| accepted -> next payload job created (forkchoice update) | ~32 |
| job created -> builder entered | ~30 |

Follower side: import 315-390 ms per block, then about 115 ms for the two vote rounds.

So one block still costs build + ~140 ms on the leader, and the follower path
(import + votes) is about as long. Next: start the next build when the block is built
instead of when the own engine has accepted it, then the build and import cost itself
(steps 2 and 3).

### 8.11 Round S (run74 / run75, 2026-11-14): what sets the cycle now

Raw output: `.artifacts/native-port-p0/run74*`, `run75*`, logs `qfPa/logs`, `qfPb/logs`.

- **run74: 194,356 TPS, PASS** (1.6M transfers in blocks 41..58 over 8.23 s, 558 common
  blocks agree). **run75: 193,798 TPS, PASS** (blocks 41..59, 8.26 s, 551 common blocks).

Change: the builder applies its own block to the frame queue when the block is built
(`FrameQueue::mark_mined` from a rayon task), so the next build does not wait for the
canonical notification. A block is identified by its transaction list (count, first and
last hash; the proposal header is finalized after the build, so its hash cannot be
used). The canonical notification of the same block is then a no-op; a different block
at that height first gives back what the earlier one took. Job created -> builder
entered fell from ~30 ms to under 0.1 ms.

The gain is only about 1%, and that shows what sets the cycle. The child of block N is
built once view N is the current one, that is once block N-1 is committed. So two
blocks take

    follower chain of one block: emit ~54 + transfer/rebuild ~64 + decode ~38
                                 + import ~330 + votes ~45          = ~530 ms
    leader chain of one block:   forkchoice/job ~30 + build ~300    = ~330 ms

together ~860 ms, 430-460 ms per block. Every millisecond removed from either chain
buys half a millisecond per block. Building two blocks ahead would only bring the
cycle to max(build 300 + own validation and forkchoice 110, follower import 330 + ~30)
= ~410 ms, about 5-10%, for a much larger state machine - not taken.

What is left is the cost per transaction: build ~3.1 us, follower import ~3.5 us, and
~150 ms per block that does not scale with the block. Order of work from here:
1. import once per host (plan step 3): removes most of the 330 ms from the follower
   chain on this one-host fleet;
2. build path (plan step 2): packing ~160 ms, finish ~85 ms, own validation ~78 ms
   (the QMDB root is computed again on a cache hit, 25 ms);
3. larger blocks: the full Gov5 copy of a 2G block is 14 MB against 16 MB message
   limits, so this needs the limits raised or the full copy dropped from the live path.

### 8.12 Round T and the plan change after comparing with n42-rs again (2026-11-14)

The user asked whether the target is still far and to catch up using n42-rs. It is 27 times
away (194k against 5.2M). Read again from the n42-rs tag and from our own profile:

- n42-rs builds a 200k block in about 39 ms (0.2 µs per transfer); ours takes about 300 ms
  for 95k (3.1 µs). The cycle structure (tenure, build ahead) was the smaller part.
- The run75 profile (10 s at 499 Hz) shows node0 using about 1.1 cores in total. The build
  executor's CPU is a fraction of its wall time: the path waits or runs on one thread.
- n42-rs's record configuration reads state from QMDB (`N42_QMDB_READS=on`) and runs with
  import once (`N42_IMPORT_ONCE=1`, seven keys over one execution layer). Its build and
  queue code is about 40k lines tuned over several hundred rounds (output shards, plan
  ahead, frame-tree root, deferred root); it is not portable in one step.

Round T (same configuration as round S; raw files `run76*`, `run77*`, fleets `qfQa`, `qfQb`
in `.artifacts/native-port-p0/`; one host, 7 validators, 1.6M transfers, wall clock):

| run | change | TPS | check |
|---|---|---|---|
| run76 | build phase timers only | 188,256 | pass (blocks 41..60, 8.50 s, 550 common blocks) |
| run77 | run76 + `--qmdb-reads on` | 197,953 | pass (blocks 42..60, 8.08 s, 559 common blocks) |

The two legs differ by 5%, which is inside the spread between rounds; QMDB reads are not
shown to matter here. The timers (new fields in `parallel_transfer::Phases`, logged with
"parallel transfer prefix executed") split the 160 ms of `evm_exec_ms`:

- `execute_for_build` 51-84 ms: partition 4-9, the 32 batches 40-74 wall (run 300-800 ms
  summed over 16 threads, each batch's own transition merge 80-110 ms summed), collect 3-10.
- About 100 ms after it, on one thread: the fee loop and `commit_transfer_batch`, whose
  `graft_bundles` inserts 190k accounts into the state's cache and into its bundle.

A local timing test (`parallel_transfer::tests::graft_timing`, ignored by default; `cargo
test --profile profiling -p n42-execution --lib graft_timing -- --ignored --nocapture`)
reproduces the graft: 84 ms with the cache copy, 63 ms without.

Changes for round U, both behind switches that are off by default:

- `N42_GRAFT_NO_CACHE=1`: a batch that fills the block (`accepted >= max_txs_per_block`),
  in a block without withdrawals, is grafted into the bundle only
  (`commit_transfer_batch(.., keep_cache)`); the serial loop is then not entered
  (`serial_allowed`), since the state would answer a read of a grafted account from the
  parent. The follower's batch import still keeps the cache (its hook cannot see the
  block's withdrawals).
- `N42_COLOCATED_VOTE=1`: in H2 participant mode the follower releases its vote when the
  block data is cached, as native mode already does, and imports behind the vote. This is
  the cycle effect of n42-rs's import once (the other keys vote on the builder's check)
  without moving seven consensus services into one process. Scope: only for validators of
  one operator on one machine; a follower no longer executes before voting. Its sustained
  rate is bounded by the follower's import time per block, which the 8-second window may
  not show; the report has to say so.

Considered and not done now: seven orchestrators and network services in one process over
one engine (scoped: per-key loop over `main.rs` 2709-2795 and 3024-3546, per-key data
directories, finalize FCUs from a lagging orchestrator returning `too_deep_reorg`, shared
sinks run once per orchestrator). It removes the six extra executions as well, but is a
refactor of the node's start-up; the vote switch is measured first. One orchestrator
holding seven keys would mean rewriting the state machine's single identity and is ruled
out.

Order from here: (1) measure the vote switch; (2) the serial tail of the build (graft,
receipts, finish 85 ms, own validation 78 ms) following n42-rs's output shards where a
merge can be avoided; (3) follower import cost if it becomes the bound; (4) larger blocks.

### 8.13 第二十一轮（run78/run79）与构块循环的串行等待

**结果**（单机 7 验证者，H2-v4 + QMDB，2G gas，160 万笔，墙钟口径；原始文件
`.artifacts/native-port-p0/run78*`、`run79*`，日志 `qfRa/`、`qfRb/`）：

| 轮次 | 配置（在 BASE 之上） | 墙钟 TPS | 校验 |
|------|----------------------|----------|------|
| run78 | `N42_GRAFT_NO_CACHE=1` | 200,205（块 42..60，7.99 s） | PASS，564 个公共块 |
| run79 | 再加 `N42_COLOCATED_VOTE=1` | 216,217（块 45..63，7.40 s） | PASS，561 个公共块 |

run79 的范围限制：先投票后导入只适用于同一运营方同机部署的验证者；8 秒窗口内
follower 的 `Committed execution_ready=false` 出现 114 次，持续速率仍受 follower
导入时间约束。

**run79 leader 逐块时间线**（node1，view 55）：构块循环约 404 ms，是当前的上限，
共识两轮只占约 65 ms：

| 阶段 | 耗时 |
|------|------|
| 上一块自验证结束 → 构块任务创建（其中链头更新 35 ms） | 约 48 ms |
| 构块（并行执行 55 + 合并/收据 67–85 + finish 83 + 缓存 10） | 268 ms |
| 自验证 new_payload（QMDB 根 27 ms） | 54 ms |
| 帧引用编码 | 20 ms |
| Gov5 完整副本编码 + 推送（提案在其后放出） | 约 150 ms |

**本阶段新增开关（默认全关）**：

- `N42_GRAFT_STREAM=1`：各批执行完即由构块线程合并，不等全部批次
  （`execute_for_build_streamed` + `Graft::absorb`；单测
  `bundles_grafted_one_by_one_as_they_arrive_match_one_graft_of_all`）。
- `N42_GOV5_COPY_LATE=1`：leader 的 Gov5 完整副本编码与推送移到提案之后的后台任务。
- `N42_CANON_NOTIFY_LATE=1`（reth `engine/tree`）：链头更新时
  `to_chain_notification` 会在引擎线程深拷贝整块执行结果（约 35 ms）；开关打开后
  纯追加的链更新由单个按序工作线程构造并发送通知，重组仍走原路径。属于待定的第八个 reth 补丁。
- 构块 finish 阶段新增日志 `N42_PAYLOAD_FINISH`（executor_finish / merge /
  receipts_root / assemble / root_wait）。

**后续**：第二十二轮 run80（加 `GOV5_COPY_LATE`）、run81（再加 `GRAFT_STREAM`）已排队；
之后测 `CANON_NOTIFY_LATE`，并按 finish 分段结果继续削减构块循环；
构块与上一块收尾重叠（n42-rs 的 plan ahead）是下一项结构性改动。

### 8.14 第二十二轮（run80/run81）：乱序最终化与交易根

**结果**（范围同 8.13；原始文件 `.artifacts/native-port-p0/run80*`、`run81*`，日志 `qfSa/`、`qfSb/`）：

| 轮次 | 配置（在 run79 之上） | 墙钟 TPS | 校验 |
|------|----------------------|----------|------|
| run80 | `N42_GOV5_COPY_LATE=1` | 215,151（块 42..60，7.44 s） | PASS，549 个公共块 |
| run81 | 再加 `N42_GRAFT_STREAM=1` | 212,880（块 42..60，7.52 s） | **FAIL**：`a committed QC has no executed block after the FCU grace period` |

- Gov5 副本后置使提案在构块结束后约 20–30 ms 放出（原约 350 ms），但 TPS 不变：
  上限是 leader 构块循环，不是共识。
- 边执行边合并把并行前缀段从约 160 ms 降到约 127 ms，整块构建 280 ms 左右。
- `N42_PAYLOAD_FINISH` 分段：finish 的 75–86 ms 几乎全部是 `assemble_block` 里串行计算的
  交易根（95,238 笔的 MPT），QMDB 根等待为 0。

**run81 失败原因**（node6，run80 中未触发属偶然）：先投票后导入让提交先于导入。
63 块的后台导入先完成并把 63 最终化，随后 62 块的最终化 FCU 被 reth 以
`too deep reorg` 拒绝；62 被当作“等待执行祖先”送去追块，最终化队列从此停在 63，
共识继续到 121。这就是此前记录的“链头已越过时后台导入发回退 FCU”问题，
在 `N42_COLOCATED_VOTE` 下变成必现风险。

**修复**（`consensus_loop.rs`）：编排器新增 `finalized_view_mark`（已最终化的最高 view）。
异步最终化与后台导入的 FCU 成功时推进它；FCU 报 `too deep reorg` 且标记已高于本块 view 时，
按“已由后代最终化”处理（`finalized_by_descendant`）。已提交块在同一条链上，
后代最终化即蕴含祖先最终化。n42-consensus-service 252 个单测通过。

**新增开关** `N42_TX_ROOT_EARLY=1`：区块恰为并行前缀（不保留缓存副本、无串行交易）时，
在前缀提交的同时由辅助线程计算交易根，finish 直接传给 `EthBlockAssembler::assemble_block`；
交易数不一致或线程异常时回落到原路径。

**第二十三轮已排队**：run82 = run81 配置 + `TX_ROOT_EARLY`；run83 再加 `CANON_NOTIFY_LATE`。

### 8.15 第二十三轮（run82/run83）：测量被注入速率封顶

**结果**（范围同 8.13；原始文件 `.artifacts/native-port-p0/run82*`、`run83*`，日志 `qfTa/`、`qfTb/`）：

| 轮次 | 配置（在 run81 之上） | 墙钟 TPS | 校验 |
|------|----------------------|----------|------|
| run82 | `N42_TX_ROOT_EARLY=1` | 212,402（块 42..61，7.53 s） | PASS，556 个公共块 |
| run83 | 再加 `N42_CANON_NOTIFY_LATE=1` | 217,885（块 43..61，7.34 s） | PASS，556 个公共块 |

- 乱序最终化修复生效：两轮各出现 1 次“已由后代最终化”，run83 的 5 次 `too deep reorg`
  均被吸收，7 节点根一致。
- `CANON_NOTIFY_LATE` 把链头更新从约 35 ms 降到微秒级。
- `TX_ROOT_EARLY` 首版在前缀提交时才启动，交易根约 75 ms 长于剩余的提交时间，
  finish 仍在等它（assemble 72–77 ms）。已改为在并行执行开始前对候选交易启动
  （执行结果保持候选顺序；有交易被跳过时作废），并新增 `root_join_ms`、`early_tx_root` 日志字段。

**关键发现：最近各轮的 TPS 上限是压测注入，不是链。**
`native-flood` 默认每节点 1 条连接，节点对每条连接按帧串行处理（读帧 → 认证 → 入池 → 回执），
约 4 ms/帧（1000 笔），即每节点约 23 万笔/秒；各轮 `accepted ... in 7.00s/8.00s` 与测得的
20–22 万 TPS 吻合，run83 中多个块未装满（90,112 / 92,928 / 91,904）。同时 node0 的 10 秒 profile
全进程仅约 1.1 核，帧认证只占 0.13 核，节点并未吃满。因此 run74 之后各开关的增益
（先投票后导入、Gov5 副本后置、边执行边合并、链头通知后置）都被这个上限压平，需在放开注入后重测。

**第二十四轮已排队**：flood 加 `--connections 8`。run84 = run83 全部开关；
run85 = 仅 `GRAFT_NO_CACHE`（run78 配置）作对照。

### 8.16 第二十四轮（run84/run85）：放开注入后的链上限

flood 加 `--connections 8` 后注入为每节点约 80 万笔/秒（160 万笔 2 秒注完），链成为瓶颈。
原始文件 `.artifacts/native-port-p0/run84*`、`run85*`，日志 `qfUa/`、`qfUb/`；范围同 8.13。

| 轮次 | 配置 | 脚本报告的墙钟 TPS | 满载出块间隔（中位） | 校验 |
|------|------|--------------------|----------------------|------|
| run84 | run83 全部开关 | 238,771（块 42..49，723,498 笔，3.03 s） | 361 ms（≈26 万 TPS） | PASS，566 个公共块 |
| run85 | 仅 `GRAFT_NO_CACHE` | 188,580（块 41..50，941,878 笔，4.99 s） | 472 ms（≈20 万 TPS） | PASS，552 个公共块 |

注意：`window-tps.py` 这两轮只统计到前 8–10 个块（积压在其窗口之后才清完），
脚本 TPS 是部分窗口的值；160 万笔全部提交（各节点 pending 归零），根一致。
“满载出块间隔”取自各节点 `inter_block_commit_ms`（pool_pending ≥ 1 万的块）。
后续轮次需修正统计窗口或加大负载。

**结论**：本阶段各开关合计把满载出块间隔从 472 ms 降到 361 ms（约 −24%）。
`TX_ROOT_EARLY` 第二版生效（`root_join_ms=0`，assemble 9–15 ms）。

**run84 leader 逐块时间线**（node1，块 51，循环约 375–390 ms）：

| 阶段 | 耗时 |
|------|------|
| 构块入口 → 开始打包（取池、候选、预状态；池内 78 万笔） | 约 60 ms |
| 并行前缀（执行 + 流式合并 + 收据） | 125 ms |
| finish（其中等 QMDB 根 71 ms，assemble 9 ms） | 94 ms |
| 缓存执行结果 + 封装 payload | 约 28 ms |
| 自验证 new_payload（其中再次推导 QMDB 根 31–56 ms） | 62–101 ms |

提案（BlockReady）与下一块的构建都在自验证结束后才开始。

**后续（按收益排序）**：
1. QMDB 根在构块与自验证中各算一遍，合计约 100–125 ms：自验证复用构块已备好的候选根。
2. 构块入口的 60 ms 准备段：分段计时后削减（大池下取候选的开销）。
3. 提案与下一块构建不等自验证；再往后是下一块执行与上一块收尾重叠（n42-rs plan ahead）。
4. 修正 `window-tps.py` 统计窗口；负载加到 480 万笔以上以便得到稳定的整窗 TPS。

### 8.17 提交前收尾：第八个 reth 补丁与导入父状态视图顺序（2026-11-15）

**reth 第八个补丁 `reth-native-import-path`**（适配 commit `2545c4de8e6ed05f322fbbad7a87d24613583645`）：

- `FullConsensus::uses_receipt_root()`：共识实现声明不使用收据 trie 根时，引擎不启动收据根任务，也不逐条复制收据。
- payload 转成区块后，预执行检查直接取区块头里的交易根，不再对交易体重算一遍。
- `try_execute_import_batch` 新增 `open_parent` 参数，由引擎提供父状态视图。
- 导入各阶段计时 `n42_import_stage_ms`。
- `N42_CANON_NOTIFY_LATE=1`：普通链延伸的 canonical 通知交给单个有序工作线程构造和发送。

`reth-source.lock`、`scripts/apply-reth-patches.sh`、`CLAUDE.md` 已同步为八个补丁，`scripts/check-reth-source.sh` 通过。

**问题：QMDB-only 读模式下导入读到空余额。** 全仓测试中 `native_cancun_first_child_survives_wire_and_engine_import` 失败，报 `LackOfFundForMaxFee { balance: 0 }`。原因是导入批处理改成了优先使用引擎的父状态视图，而该视图来自 `database_provider_ro()`，不经过 `BlockchainProvider::state_by_block_hash` 上的 QMDB 读路由。改为按哈希打开的 provider 优先，引擎视图只在按哈希打不开时兜底（父块在引擎树内但尚未 canonical 的情况）。

**口径说明：** run84/run85 是在"引擎视图优先"的顺序下测的。顺序改回后尚未重测，下一轮对照腿需要确认满载出块间隔没有回退。

**验证：** `cargo clippy --all-targets -- -D warnings` 通过；`cargo test --workspace` 无失败。

**后续：** 见 8.16 的排序，首项为自验证复用构块已算好的 QMDB 根。

### 8.18 Y 轮：自验证复用构块的 QMDB 操作列表（2026-11-15）

**设计。** 构块端 `prepare_candidate` 早已把树的应用结果留给导入端，`compute_and_commit` 命中后不再重算树。自验证里重复的是从执行输出重新推导操作列表、排序、再与候选逐条比对。新增：

- `Gov5QmdbStateRootStore::prepare_bound_candidate(parent, operations, binding)`：`binding` 即已有的 restored-slots 键，覆盖父块哈希、区块环境、beacon root、withdrawals 和有序交易哈希。
- `commit_bound_candidate(parent, block_hash, expected_root, binding)`：候选的父块与绑定键都相符时直接提交候选（操作列表从候选中移出，不再比对）；否则返回 `None`，调用方照旧推导。区块头的根不符仍报 `RootMismatch`，候选被撤销。
- 根任务在 `N42_QMDB_ADOPT_BUILT=1` 时先走这条路。
- 计时：`n42_qmdb_root_job_ms{stage=derive|sort|commit|bound_commit}`；`N42_PAYLOAD_PACK` 增加 `setup_*` 字段。

考虑过按区块哈希绑定，但 Gov5 归一化会改变区块哈希，构块时拿不到最终哈希，所以用执行输入的键。

**结果**（单机 7 验证者，H2-v4 + QMDB，2G gas，160 万笔，每节点 8 条注入连接；原始文件 `.artifacts/native-port-p0/run86*`、`run87*`；两轮均 PASS，7 节点根一致）：

| 轮次 | 配置 | leader 循环中位（构块入口到下一次入口） | 根任务（每个满块） | 自验证 |
|------|------|------|------|------|
| run86 | run84 配置（父状态按哈希优先） | 361 ms | 推导 4 + 排序 2 + 提交 30 ms | 67 ms |
| run87 | 加 `N42_QMDB_ADOPT_BUILT=1` | 369 ms | 约 11.5 ms | 65 ms |

- run86 的循环与 run84 的 361 ms 一致，说明 8.17 的父状态视图顺序修复没有造成回退。
- 新开关把根任务缩短约 25 ms，但自验证和整个循环没有变短，差值在噪声内。说明自验证的时长不由根任务决定。
- leader（node1）各段中位：入口到打包 34–37 ms（全部在 `apply_pre_execution_changes`，provider、环境、取交易迭代器均为 0）、打包 142–148、finish 70（等 QMDB 候选 47）、缓存 12、到 new_payload 19–30、自验证 65。
- leader 指标里与 QMDB 根无关的 reth 工作量：`hash-post-state` 每满块约 50 ms，状态叠加层计算约 70 ms，`overlay_wait` 合计 1.56 s，`AccountsHistory` 查询 165 万次（约 1.3 s CPU）。

**后续。** Z 轮（run88）抓 leader 的 pprof，并在 reth 校验器里加了 `pre_execute / post_execution / state_root / validate_total` 分段计时，用来定位自验证剩余的约 50 ms 和构块前 35 ms 的阻塞点。

### 8.19 Z 轮：自验证分段与构块前等待的第一个假设（2026-11-15）

**结果**（范围同 8.18；原始文件 `.artifacts/native-port-p0/run88*`，日志 `qfWa/`；PASS，7 节点根一致）：run88 配置同 run87，脚本窗口 249,983 TPS（块 43..50，窗口不完整），leader（node1）相邻满块打包完成的间隔中位 378 ms（15 个间隔，343–434）。pprof 只落在 node0、node3 上，没有覆盖 leader。

**leader 自验证分段**（`n42_import_stage_ms`，63 次导入、其中 17 个满块；按满块摊的估计值）：

| 段 | 合计 | 每满块约 |
|------|------|------|
| `pre_execute`（入口到执行前） | 171 ms | 10 ms |
| `await_convert`（等 payload 转区块） | 177 ms | 10 ms |
| `post_execution` | 52 ms | 3 ms |
| `state_root` | 605 ms | 35 ms |
| `validate_total` | 1,327 ms | 约 70 ms |

自验证没有单一的大头，是四五段各 10 ms 左右叠起来的。

**构块前的 22–53 ms：假设。** `setup_pre_execution_ms` 证实等待全在 `apply_pre_execution_changes`。当时的假设是：QMDB 读适配器把字节码读取委托给 reth 内层 provider，内层 provider 的第一次读要等父块的执行叠加层（leader 的 `overlay_wait` 59 次共 1.31 s）。据此给 `QmdbStateAdapter` 加了按代码哈希的进程内缓存（`CodeCache`，上限 4,096 条；`verify` 模式不走缓存；计数 `n42_qmdb_code_reads_total`）。

**新增计时。** reth 校验器 `pre_provider_factory / pre_root_prepare / pre_spawn_processor`（均为自入口起的累计值）；根任务 `n42_qmdb_root_job_ms{stage=key}`。

### 8.20 AA 轮：假设不成立，这些轮次的 QMDB 读一直是关的（2026-11-15）

**结果**（范围同 8.18；原始文件 `.artifacts/native-port-p0/run89*`，日志 `qfXa/`；PASS，7 节点根一致）：脚本窗口 237,639 TPS（块 42..49，窗口不完整），leader 间隔中位 382 ms（14 个间隔，339–424），构块前等待仍是 28–59 ms。字节码缓存没有任何效果。

**原因。** leader 的指标里没有任何 QMDB 读计数。`chain94-fleet.py start` 的 `--qmdb-reads` 默认是 `off`，而本文档里的所有轮次都没有传它（`qfXa/run.json` 记录 `"qmdb_reads": "off"`）。也就是说：

- 构块和导入的账户、存储读取一直走 reth 的叠加层 provider，QMDB 只用来算根；
- 每个块的第一次读要等父块的执行叠加层算完，这就是构块前的等待（AA 轮 `overlay_wait` 88 次共 1.21 s）；
- 叠加层未命中的读取落到历史索引上（8.18 记录的 `AccountsHistory` 165 万次查询），这很可能也在拖慢 145 ms 的打包。

8.17 里“按哈希 provider 才有 QMDB 读路由”的说法只对开了 QMDB 读的配置成立；那个顺序修复解决的集成测试失败是真实的，但它在这些测量轮次里不改变读路径。

**AA 轮的细分计时**（leader，17 个满块）：`pre_execute` 158 ms 中，建 provider 工厂 22 ms、到根任务准备 40 ms、其余约 117 ms（每满块约 7 ms）在 `spawn_payload_processor` 之后；`state_root` 628 ms 中，键 80 ms、绑定提交 203 ms、空块提交 210 ms，约 135 ms 未归属。

**后续。** AB 轮（run90，`qfYa`）在 run89 配置上加 `--qmdb-reads on`：账户和存储从 QMDB 读视图取，字节码走缓存，看构块前等待、打包时长和历史索引查询是否下降。之后再决定“封块后立即在未验证的自有块上构下一块”的做法。

### 8.21 AB 轮：打开 QMDB 读（2026-11-15）

**结果**（范围同 8.18；原始文件 `.artifacts/native-port-p0/run90*`，日志 `qfYa/`；PASS，7 节点根一致，572 个共同块）：run90 = run89 配置 + `--qmdb-reads on`。

| 轮次 | QMDB 读 | 脚本窗口 TPS | leader 间隔中位 | 构块前等待 |
|------|------|------|------|------|
| run89 | off | 237,639（块 42..49） | 382 ms（339–424） | 28–59 ms |
| run90 | on | 270,981（块 43..48） | 339 ms（301–431，15 个间隔） | 0 ms |

脚本窗口两轮都不完整，只作参考；循环以 leader 间隔为准，339 ms 对应约 28 万 TPS。

- 构块前等待消失，`execution_overlay` 的等待次数为 0。leader 上 QMDB 账户读 191 万次，638 个 provider 全部命中读视图。
- 打包时长没有变化（137–208 ms，中位约 143），所以 8.20 里“历史索引查询拖慢打包”的猜测不成立；`AccountsHistory` 的 161 万次 get 仍在，来源不是读路径。
- 自验证合计不变（1,335 ms / 63 次）；绑定提交从每满块约 12 ms 升到约 22 ms（多了读视图发布）。
- leader 每满块的 finish 约 77 ms，其中等 QMDB 候选树约 57 ms（`qmdb_candidate_compute_ms{operation=prepare}` 964 ms / 17）。

**现在的循环（每满块，leader）：** 打包约 145、finish 约 77、缓存 13、到 new_payload 约 25、自验证约 70，合计约 330–340 ms。

**后续，按预期收益：**
1. 封块后立即在未验证的自有块上构下一块：QMDB 读打开后，父状态可以直接取自候选树的读视图，可重叠缓存、new_payload 和自验证约 100 ms。
2. QMDB 候选树的 57 ms：操作列表在打包结束时才有，要么分片并行应用，要么边打包边喂。
3. 打包本身 145 ms（19 万次 QMDB 账户读 + 9.5 万笔转账）。

### 8.22 AC 轮：leader 的 pprof 与三处修改（2026-11-15）

**结果**（范围同 8.18，`--qmdb-reads on`；原始文件 `.artifacts/native-port-p0/run91*`，日志 `qfZa/`；PASS，7 节点根一致）：配置同 run90，pprof 改抓 leader（node1）和 node3。脚本窗口 252,127 TPS（块 43..49，窗口不完整）；leader 间隔中位 339 ms（15 个间隔，317–383）。pprof 开着的节点会慢一些，这一轮只用来看分布。

**leader pprof（3,815 个样本，10 s，只看比例）：**

- 构块线程 `payload-builder` 356 个样本里，144 个在 `QmdbLeafTree::set_with_leaf`：QMDB 候选树是在这个线程上串行应用的，每个操作对活跃索引先 `remove` 再 `insert`，索引远大于缓存，主要是内存延迟。这就是 finish 里等候选树的约 57 ms。
- 执行线程 `n42-build` 162 个样本里约 45 个在 `metrics_util` 注册表查找：`QmdbStateProvider` 每次读都调一次 `metrics::counter!`。
- 引擎线程 168 个样本里 91 个在 `imbl` HAMT 节点的克隆与释放：读视图缓存淘汰旧视图时，在引擎线程上、持着缓存写锁逐节点释放。这解释了 8.21 里绑定提交变慢的 10 ms。
- `cpu` 线程 147 个样本在校验器的交易迭代器（缓存命中时用不上）；`handle_built_payload` 275 个样本里 96 个在重新解码 0x50 交易、90 个在 Gov5 编码。

**修改：**

1. `QmdbStateProvider` 的账户、存储读不再逐次查指标注册表；读次数仍由按线程分片的 `QmdbReadCounters` 计，`n42_qmdb_state_reads_total` 在每次创建 provider 时用 `absolute` 发布。
2. `QmdbReadViewCache::insert` 返回被淘汰的视图，由 `release_read_views` 交给 rayon 后台释放，不再占引擎线程和缓存锁。
3. `QmdbLeafTree::set_with_leaf` 用一次 `insert` 同时写入新条目并取回旧条目（原来是 `remove` 加 `insert` 两次探测）。槽位分配、撤销记录、位图更新的顺序不变；`n42-twig-core` 70 个测试和 `n42-node` 74 个 QMDB 测试通过。

**后续。** AD 轮（run92，`qgAa`）测量这三处。之后：候选树应用并行化或提前到打包期间；`handle_built_payload` 里的重复解码；校验器在缓存命中时跳过交易迭代器。

### 8.23 AD 轮与打包里的串行段（2026-11-15）

**结果**（范围同 8.22；原始文件 `.artifacts/native-port-p0/run92*`，日志 `qgAa/`；PASS，7 节点根一致，570 个共同块）：脚本窗口 253,280 TPS（块 42..48，窗口不完整）；leader 间隔中位 321 ms（15 个间隔，290–380），run91 是 339 ms。pprof 同样开在 leader 上。

| leader 合计（17 个满块） | run91 | run92 |
|------|------|------|
| QMDB 候选树 `prepare` | 938 ms | 858 ms |
| 构块 finish | 1,311 ms | 1,241 ms |
| 绑定提交 | 382 ms | 376 ms |
| 自验证 `validate_total` | 1,376 ms | 1,396 ms |

单次探测让候选树快了约 8%；绑定提交没有变化，说明 8.22 里“淘汰旧视图拖慢绑定提交”的推测不成立（释放确实移走了，但它不在这 22 ms 里）。打包中位 145 ms 没变。

**打包 145 ms 的构成**（`parallel transfer prefix executed` 日志，满块）：32 个批次各自最长 14–18 ms，但 `groups_ms` 是 37–47 ms（16 个线程跑 32 个批次，分两波）；流式 graft 在构块线程上串行，每块 54–69 ms；`commit_ms` 23–42 ms。也就是说真正的并行执行约 15 ms，其余是把 19 万个账户逐个并进一张 `BundleState` 表的串行簿记。

**修改。** `graft_bundles_inner` 对区块 bundle 表改用 `entry`：一次探测既判断是否已有条目，又拿到插入位置（原来是 `get_mut` 未命中后再 `insert`）。`n42-execution` 134 个测试通过。

**后续。** AE 轮：run93（`qgBa`）测单次探测的 graft；run94（`qgBb`）再加 `N42_PARALLEL_BUILD_THREADS=32`，让 32 个批次一波跑完。

### 8.24 AE 轮：单次探测的 graft 与 32 个构块线程（2026-11-15）

**结果**（范围同 8.22，pprof 开在 leader 上；原始文件 `.artifacts/native-port-p0/run93*`、`run94*`，日志 `qgBa/`、`qgBb/`；两轮均 PASS，7 节点根一致）：

| 轮次 | 配置 | 脚本窗口 TPS（块 44..49） | leader 间隔中位 | 打包中位 | graft | 批次整体 |
|------|------|------|------|------|------|------|
| run92 | 8.22 的三处修改 | 253,280 | 321 ms | 145 ms | 54–69 ms | 37–47 ms |
| run93 | 加单次探测 graft | 275,927 | 323 ms（277–392） | 137 ms | 48 ms | 40 ms |
| run94 | 再加 32 个构块线程 | 275,504 | 322 ms（267–381） | 139 ms | 54 ms | 33 ms |

- graft 每块快了约 10 ms，打包快了约 8 ms，但 leader 间隔没有变化（差值在噪声内）。
- 32 个线程让批次整体从 40 降到 33 ms，但打包由串行 graft 决定，没有收益；默认 16 不改。
- 脚本窗口仍不完整，只作参考。

**判断。** leader 每满块：打包约 137、finish 约 73、缓存 13、到 new_payload 约 25、自验证约 70，合计约 320 ms，全部串行。继续逐项削每次只有几个百分点。下一步改结构：封块后立即在未验证的自有块上构下一块，让缓存、new_payload、自验证（约 108 ms）与下一块的打包重叠。

### 8.25 leader 把自有块作为已执行块交给引擎（2026-11-15）

**设计。** 比较了两条让下一块更早开始的路：

- 在未进引擎树的自有块上直接构块：要绕过 FCU（引擎对未知 head 返回 Syncing）、给 `BasicPayloadJobGenerator` 补父块头、给构块端做“父块的父状态 + 父块 bundle”的叠加 provider，QMDB 只允许一个候选、叠第二个会丢掉第一个。改动面大。
- 走 reth 已有的 `EngineApiRequest::InsertExecutedBlock`（本地构块者的常规路径）：自有块几毫秒内进引擎树，之后 FCU、构块、provider 全部沿用现有路径。选这条。

8.5 节当时搁置它的三个理由现在都有了解法：引擎入口用一个进程内收件箱补上；QMDB 提交用 8.18 的绑定候选在根任务之外完成；hashed post state 用 rayon 与 QMDB 提交并行算。

**实施。**

- reth（待做成第九个补丁）：`crates/node/builder/src/n42_executed.rs` 提供 `submit_executed_block(Box<dyn Any + Send>)`，引擎启动循环多一个 select 分支，把提交的块转成 `InsertExecutedBlock` 入队后应答。应答之后发出的 FCU 一定排在插入之后。
- `engine_validator.rs`：把 gov5 H2 头部绑定（当前形态、旧 difficulty、记住的原生头）抽成 `bind_gov5_h2_header`；新增 `gov5_block_from_built_body(payload, body, transactions_root)`，用 payload 的头字段加构块时已解码的 body 重建区块，不再解码 9.5 万笔交易。头必须哈希到 payload 的区块哈希，所以不是这个 payload 的 body 过不了。
- `qmdb_state_root.rs`：`prepared_binding(parent, root)` 返回立在该父块上且根相符的候选的绑定键。
- `el/own_block.rs`（`N42_LEADER_INSERT_EXECUTED=1`）：`resolve_payload` 记下刚构出的块；`new_payload` 遇到同父块、同高度、同交易数的 payload 时：重建区块、取出缓存的执行输出、并行做 QMDB 绑定提交和 hashed post state、提交给引擎，然后直接返回 Valid。任何一步不成立就把执行输出放回缓存，照旧走 `newPayload`。计数 `n42_own_block_insert_total{outcome}`，计时 `n42_own_block_insert_ms{stage}`。

**跳过了什么。** leader 不再对自有块做头部共识校验、执行后校验（收据根、gas）和根任务比对；QMDB 根仍与头里的状态根核对。follower 投票前照旧完整校验。提案放出的条件不变。

**测试。** `built_body_rebuilds_the_payload_block_and_a_foreign_root_is_refused`（两种 difficulty 下重建结果与完整解码一致；换一个交易根就被拒）；`n42-node` clippy 通过。

**后续。** AF 轮：run95（`qgCa`）同一二进制不开开关，run96（`qgCb`）打开。

### 8.26 AF 轮：插入路径可用，但第一版没有变快（2026-11-15）

**结果**（范围同 8.22，pprof 开在 leader 上；原始文件 `.artifacts/native-port-p0/run95*`、`run96*`，日志 `qgCa/`、`qgCb/`；两轮均 PASS，7 节点根一致）：

| 轮次 | `N42_LEADER_INSERT_EXECUTED` | 脚本窗口 TPS | leader 间隔中位 |
|------|------|------|------|
| run95 | 关 | 256,348（块 42..47） | 320 ms（297–388，14 个间隔） |
| run96 | 开 | 261,060（块 43..48） | 331 ms（300–454，15 个间隔） |

- 功能上成立：leader 的 17 个满块全部走了插入（`n42_own_block_insert_total{outcome=queued}` 17），561 个共同块 7 节点根和哈希一致。另有 15 次 `refused` 是空块（没有 QMDB 候选），照旧走 `newPayload`。
- 没有变快：准备阶段合计 1,174 ms，每满块约 69 ms，与原来的自验证一样长。reth 的 `HashedPostState::from_bundle_state` 是串行的，19 万个账户的 keccak 加建表约 50 ms，盖过了并行的 QMDB 提交。

**修改。** `own_block.rs` 自带 `hashed_post_state`：keccak 分到 rayon 线程池，再串行收进表里；测试 `parallel_hashed_post_state_matches_reth` 与 reth 的结果逐项比对。准备阶段加分段计时 `n42_own_block_insert_ms{stage=rebuild|qmdb_commit|hashed_state}`。

**后续。** AG 轮（run97，`qgDa`）。

### 8.27 AG/AH 轮：leader 循环 300 ms，follower 导入成为上限（2026-11-16）

**结果**（范围同 8.22，`N42_LEADER_INSERT_EXECUTED=1`，pprof 开在 leader 上；原始文件 `.artifacts/native-port-p0/run97*`、`run98*`、`run99*`，日志 `qgDa/`、`qgEa/`、`qgEb/`）：

| 轮次 | 收尾校验 | 脚本窗口 TPS | leader 间隔中位 |
|------|------|------|------|
| run97 | **失败** | 251,852（块 40..44） | 300 ms（274–373） |
| run98 | **失败** | 263,883（块 43..47） | 301 ms（266–412） |
| run99 | PASS，570 个共同块根一致 | 277,720（块 42..47） | 304 ms（276–384） |

失败信息都是 `a committed QC has no executed block after the FCU grace period`。三轮里 leader 的 17 个满块都走了插入路径。

**插入前的准备**（run97，每满块）：约 37 ms，其中重建区块约 14、QMDB 提交约 20、hashed post state 约 14（后两项并行）。对比 8.26 的 69 ms。leader 循环从 320 降到约 300 ms。

**失败原因。** 不是插入路径算错：run98 结束时 node1–node6 都 finalize 到 view 65，只有 node0 停在 view 60，它的 finalize 比共识晚 2.8 s。所有轮次（包括 PASS 的 run95、run96）里 follower 的 finalize 最坏都落后 4–5 s：follower 每个满块的导入约 350 ms（run99 的 node2：`validate_total` 5,962 ms / 17），比 leader 的循环长。follower 在导入前投票，所以共识不等它们；leader 越快，积压越多，收尾校验的宽限期就不够了。

**follower 导入的构成**（run99，node2，每满块）：执行到根任务开始约 263 ms（其中并行批次 107、等交易收集 70、拷贝 17、取 bundle 12），状态根约 76 ms（QMDB 提交 60）。

**判断。** 现在的上限是 follower 导入，不是 leader。再缩短 leader 循环只会加大积压，不会提高可持续的吞吐。8.11 的模型（两块 = follower 链 + leader 链）里 follower 一侧重新成为大头。

**后续，按顺序：**
1. follower 导入：交易收集的 70 ms（frame-ref 块先把帧重新拼成 14 MB 的 payload 再解码，发送者在帧入库时已认证过）、并行批次 107 ms（与 leader 的 graft 同一类串行合并）、QMDB 提交 60 ms（follower 没有预先应用的候选）。
2. 收尾校验的宽限期要么拉长，要么改成先等所有节点 finalize 到同一高度；否则 leader 越快越容易误报。
3. 之后才轮到 leader 一侧剩下的 finish 73 ms 和封块到 new_payload 的 25 ms。

### 8.28 AI 轮：follower 侧小修，重复提交的判断是错的（2026-11-16）

**结果**（范围同 8.27，另加 `N42_ALTSIG_SENDER_CACHE=4194304`；原始文件 `.artifacts/native-port-p0/run100*`、`run101*`，日志 `qgFa/`、`qgFb/`）：

| 轮次 | 收尾校验 | 脚本窗口 TPS | leader 间隔中位 | follower finalize 最坏落后 |
|------|------|------|------|------|
| run100 | PASS，551 个共同块 | 259,221（块 42..48） | 304.5 ms | 3.6–3.9 s |
| run101 | PASS，558 个共同块 | 265,311（块 43..49） | 285 ms | 3.8–4.2 s |

两轮都过了，但 8.27 是三轮过一轮，样本太少，不能说问题已解决。follower 每满块的 `validate_total` 仍约 345 ms（run100 node2：5,905 ms / 17），没有变。

**保留的三处改动：**
- 帧库查询改读写锁、发送者缓存开到 4M 条：交易收集从约 70 ms 降到约 53 ms 每满块，发送者未命中为 0。
- 发送者来源计数改为每个 payload 发布一次。
- `execution_parent_and_number` 按广播内容（blake3）记住结果，共识循环不再反复整块解码。

**撤掉的一处，判断错误。** 我从 pprof 看到 payload 转换既出现在 `payload-convert` 线程又出现在引擎线程，又看到 49 次 FCU 对 58 次 newPayload，据此认为 follower 把同一块提交了两次、第二次在引擎线程白算一遍交易根，于是加了按 envelope 内容复用交易根的缓存。实测满块只有 18 次推导、没有一次满块复用（43 次复用全是空块，空块内容相同），`standard_header` 总耗时反而从 1,170 ms 升到 1,433 ms（多出来的是算内容键）。转换次数本来就是每块一次，重复提交不存在。该改动已撤回。引擎线程上为什么会出现转换，还没查清。

**后续：** follower 每满块 345 ms 里最大的仍是并行批次约 120 ms（串行合并结果）和状态根约 79 ms（QMDB 叶子树现算）。下一步动这两处。

### 8.29 AJ 轮：导入合并不写缓存没有收益，两条腿都没过校验（2026-11-16）

**结果**（范围同 8.28；原始文件 `.artifacts/native-port-p0/run102*`、`run103*`，日志 `qgGa/`、`qgGb/`）：

| 轮次 | 配置 | 收尾校验 | 脚本窗口 TPS | leader 间隔中位 | follower 并行批次 | follower 状态根 |
|------|------|------|------|------|------|------|
| run102 | `N42_IMPORT_GRAFT_NO_CACHE=1` | **失败** | 257,628 | 330 ms | 104 ms/满块 | 98 ms |
| run103 | 对照 | **失败** | 249,801 | 309 ms | 110 ms/满块 | 87 ms |

失败信息同 8.27（已提交的块在宽限期内未执行）。到这里，开着 `N42_LEADER_INSERT_EXECUTED` 的 7 轮里 4 轮失败。

**判断。** 导入时不写状态缓存只让并行批次快了约 6 ms，在噪声内，开关已撤回。此前按 pprof 采样数估算“批次几乎全是串行合并”的依据不足：采样频率我没有核实。改为直接计时：`try_commit_import_batch` 现在按阶段记录 `n42_parallel_import_batch_stage_ms`（keys、prestate、execute 及其 partition/groups/collect、graft、receipts）。

**follower 每满块的构成**（run103 node2，合计 339 ms）：并行批次 110、状态根 87（其中 QMDB 提交 70）、交易收集 52、拷贝 17、取 bundle 12、执行前准备 11，其余约 50 未分项。

**下一处改动：QMDB 叶子树的有序批量写入。** leader 的候选树和 follower 的提交走的是同一条 `apply_ordered_refs`，它逐个算叶子哈希，而另一条（拥有所有权的）路径早就是批量 SIMD 哈希。现在两条一致；另外在串行写入前用 8 个线程把这批键在 live 索引里读一遍，把桶带进共享缓存。加了 2 万条操作的等价性测试。AK 轮（run104、run105）验证。

### 8.30 AK 轮：导入批次的分段计时，叶子树批量哈希收益很小（2026-11-16）

**结果**（范围同 8.28；原始文件 `.artifacts/native-port-p0/run104*`、`run105*`，日志 `qgHa/`、`qgHb/`）：

| 轮次 | 收尾校验 | 脚本窗口 TPS | leader 间隔中位 |
|------|------|------|------|
| run104 | **失败** | 177,488（块 32..38，窗口含一次停顿） | 305 ms |
| run105 | PASS，545 个共同块 | 267,460（块 40..45） | 301 ms |

开着 `N42_LEADER_INSERT_EXECUTED` 的 9 轮里 5 轮失败。

**follower 并行批次的构成**（node2，每满块，两轮）：

| 段 | run104 | run105 |
|------|------|------|
| 批次合计 | 127 ms | 119 ms |
| 执行（其中各组并行执行 55 / 45） | 60 | 51 |
| 合并 bundle（graft，串行） | 58 | 59 |
| 回执 | 5 | 6 |
| 取键、分组 | 6 | 6 |

QMDB 提交 64 / 70 ms（8.29 为 70），叶子批量哈希加索引预读的收益在噪声内。follower 每满块仍约 350 ms。

**判断。** follower 的批次和 leader 的 pack 是同一个函数（`execute_for_build`），两侧各有约 110 ms 花在“各组执行”和“串行合并 bundle”这两段上，小修动不了它。

**更正。** 我一度把执行慢归因于“按连通分量分组、随机转账连成一个大分量”。这是错的：那是 `graft_bundles_inner` 里一段旧注释描述的旧实现，现在的分组早已是 `partition_by_sender`（只按发送者分组，收款方的入账可交换）。执行 45–55 ms 的真实原因还没定位；leader 上 16 线程和 32 线程无差别（8.24），说明不是线程数。

**对照 n42-rs**（只读子任务的报告，未改动 n42-rs）：
- 分组：同样是 `partition_by_sender`，n42-26 已有。
- 合并：n42-rs 不再把各批结果并进一张表。`OutputShards` 让每批的表原样交出，只建一个按地址前缀分片的“地址 → 批次”索引；QMDB 根直接读分片视图；连续的 `BundleState` 放到封块之后、关键路径之外（`N42_MERGE_ELIDE`）。它记录的串行合并是 68–75 ms / 14.7 万账户，与这里的 58 ms / 19 万账户同量级。
- 状态树：仍是一棵与 gov5 兼容的树，但叶子哈希、旧槽位查找、索引插入、twig 重算都在 rayon 上，只有槽位追加顺序是串行；键 → 槽位索引分 256 片。它记录的整根 34–40 ms / 20 万笔。
- follower：区块不带执行提示；follower 用和 leader 相同的机制执行（`N42_FOLLOWER_BUILD_PATH`），并且在只做 O(1) 可包含性检查后就投票（延后执行）。
- 口径：n42-rs 的 3.9–5M TPS 是 E=1（七个验证者密钥共用一个执行层，每台主机只执行一次），与这里七个节点各自执行不可直接比。

**后续方案，按可独立验证的顺序：**
1. QMDB 叶子树的并行批量写入（分片索引 + 并行查旧槽位 + 并行 twig 重算）。局限在 `n42-twig-core` 一个 crate，有现成实现作对照，根必须逐位相等。预期 leader 的候选树（50–57 ms）和 follower 的提交（64–70 ms）各省一半以上。
2. 定位各组执行的 45–55 ms（按线程计时：读状态、执行、建 bundle）。
3. 输出分片与合并省略。改动面最大（下一块的读、QMDB 根、reth 持久化都要认分片视图），放在前两项之后。

### 8.31 AL 轮：叶子树 live 索引分片，两侧各省约 20 ms（2026-11-16）

**设计。** `QmdbLeafTree` 的 live 索引从一张 `HashMap` 改成 256 片（`LiveIndex`）。分片由每进程随机种子与键的前 8 字节混合决定：键是发送者可以刻意构造的哈希，按键自身的位分片会被人打满一片。一个块的操作键互不相同，每个 set 的槽位由它在批次里的位置决定，所以索引的插入、删除可以按分片在 8 个线程上做（`apply_batch`，8,192 条以上才并行）；之后按顺序串行的只剩退役被替换的条目和追加叶子（`append_slot`）。叶子哈希在这之前已经成批算好。考虑过整体移植 n42-rs 的指纹索引（每键 11–21 字节）：省内存，但要连带改条目存储，这一步先不做。

**验证。** `large_ordered_batches_match_the_owned_path`：四轮 2 万条的批次（替换、删除存在与不存在的键），与拥有所有权的路径和 `QmdbCompatTree` 比根、条目数、撤销记录，再撤销一块比对。

**结果**（范围同 8.28；原始文件 `.artifacts/native-port-p0/run106*`、`run107*`，日志 `qgIa/`、`qgIb/`）：

| 轮次 | 收尾校验 | 脚本窗口 TPS | leader 间隔中位 | follower QMDB 提交 | follower `validate_total` | leader 候选树 prepare |
|------|------|------|------|------|------|------|
| run106 | PASS，549 个共同块 | 271,299（块 44..50） | 300.5 ms | 51 ms | 326 ms | 34 ms |
| run107 | PASS，558 个共同块 | 257,568（块 42..46） | 299 ms | 49 ms | 337 ms | 34 ms |

对比 8.30：follower QMDB 提交 64–70 → 49–51 ms，leader 候选树 50–57 → 34 ms。leader 间隔没有跟着降，说明 finish 里这一段不在 leader 的关键路径上（它和别的工作并行）。

**leader 批次的现状**（run106 的 leader 日志）：32 批、各批最长 12 ms、各组执行墙钟 33 ms（CPU 合计约 250 ms，即每笔约 2.6 µs）；边执行边合并的 graft 48 ms 是串行的，整段 run 71–74 ms 由它决定。follower 的 graft（58 ms）是执行完才做的，没有和执行重叠。

**下一步：** follower 导入也边执行边合并（`N42_IMPORT_GRAFT_STREAM=1`，默认关）。批次被拒时要退回普通路径，所以合并可能动到的三样东西（空的 bundle、缓存、transition）先留底，被拒就恢复；装了状态钩子时不走这条路。AM 轮（run108 开、run109 对照）验证。

### 8.32 AM 轮：follower 边执行边合并没有收益（2026-11-16）

**结果**（范围同 8.28；原始文件 `.artifacts/native-port-p0/run108*`、`run109*`，日志 `qgJa/`、`qgJb/`）：

| 轮次 | 配置 | 收尾校验 | 脚本窗口 TPS | leader 间隔中位 | follower 批次合计 | 其中 graft | follower `validate_total` |
|------|------|------|------|------|------|------|------|
| run108 | `N42_IMPORT_GRAFT_STREAM=1` | PASS，545 个共同块 | 283,631（块 43..49） | 300 ms | 111 ms | 71 ms | 354 ms |
| run109 | 对照 | PASS，560 个共同块 | 277,360（块 43..48） | 294 ms | 102 ms | 59 ms | 345 ms |

**判断。** 对照腿里执行只要 36 ms，graft 59 ms；让两者重叠后 graft 变成 71 ms（与执行线程争内存），整段由 graft 决定，没有变短。开关已撤回。结论与 n42-rs 记录的一致：这一段只能靠不合并（输出分片、合并省略）去掉，流式或分片插入都不行。

**leader 一个周期的时间线**（run109，view 51，约 295 ms）：打包 129 → finish 64（等候选树 39、组装 13）→ 缓存执行结果 19（取 bundle、追加并排序 19 万条 revert）→ 到 leader_emit 约 22（区块转 payload）→ 自有块插入约 40（重建 14、QMDB 提交 21）→ 下一个构块任务创建。后三段合计约 80 ms 是纯串行的转换与拷贝。

**follower 还有约 65 ms 没有计时覆盖**（`execute_to_root_start` 266 ms，已分项的收集 57、批次 102、拷贝 17、取 bundle 12、执行前 11 只有约 200）。在 reth 校验器里补了计时：`exec_setup`、`transactions`、`executor_finish`、`exec_metrics`、`execute_block_call`（含返回时释放交易向量）、`after_execute`。AN 轮（run110、run111）取数。

### 8.33 AN/AO 轮：follower 执行阶段的未计时部分，后台释放交易向量（2026-11-16）

**AN 轮**（只加计时；`run110*`、`run111*`，`qgKa/`、`qgKb/`；两轮 PASS）：follower 每满块 `transactions` 阶段 254–262 ms，已分项的收集 60、拷贝 20–25、批次 112–134 合计约 200–215，余 45–60 ms。其余新计时都小：`executor_finish` 4.5、`exec_metrics` 约 1、`exec_setup` 与 `after_execute` 约 0。按代码结构，余下部分落在批次成功后：收集到的交易向量和为批次再拷贝的一份在引擎线程上就地释放。

**AO 轮**（reth 校验器：批次成功后 `spawn_drop((transactions, recovered))`，为此给 `execute_block` 的交易类型加 `Send + 'static` 约束；`run112*`、`run113*`，`qgLa/`、`qgLb/`）：

| 轮次 | 收尾校验 | 脚本窗口 TPS | leader 间隔中位 | follower `transactions` | follower `validate_total` |
|------|------|------|------|------|------|
| run112 | PASS，547 个共同块 | 264,825（块 41..46） | 286 ms | 224 ms | 337 ms |
| run113 | **失败** | 282,146（块 42..48） | 290 ms | 237 ms | 353 ms |

`transactions` 阶段少了约 25–30 ms，与推断方向一致，但 `validate_total` 没有跟着降（本轮批次 122–130 ms，比 AM 轮的 102–112 高），单节点、每轮 17 个满块的数据噪声在 ±15 ms，分不清。把拷贝改成并行没做成：交易类型不是 `Sync`。

**小结（8.27–8.33，开着 `N42_LEADER_INSERT_EXECUTED` 共 17 轮，7 轮收尾校验失败）。** leader 间隔 285–305 ms；follower 每满块导入 330–355 ms，始终比 leader 长。这七轮里真正有效的只有两处：叶子树分片索引（两侧各约 20 ms）和后台释放（约 25 ms，待确认）。其余（去重、锁、缓存、流式合并）要么无效要么判断错了。follower 剩下的大头是收集 55、批次里的执行 36–50 与合并 59、状态根 65–75，每一项都要结构性改动才能动。

**方向。** 本文第 3 节 Stage B 第 6 条早已列出：同一台机器上的验证者共用一个执行层、只导入一次（n42-rs 的 `N42_IMPORT_ONCE`）。它直接去掉 follower 的整段导入，是 n42-rs 头条数字的前提；这里一直用 `N42_COLOCATED_VOTE` 近似。继续逐项削 follower 的收益是每轮 5% 左右，而单机七节点的上限由 follower 决定。下一阶段做 import-once，口径随之变为 E=1，报告时必须与“七节点各自执行”的数字分开标注。

### 8.34 同机只导入一次，第一步：委托执行层（2026-11-16）

**设计。** n42-rs 的形状是“七个验证者进程、一个执行层”。这里共识服务和 reth 在同一进程里，拆成独立的执行层进程或把七个编排器放进一个进程（8.12 估过，要重排 `main.rs` 约 800 行的装配）都太大。第一步取最小可测的形式：
- 一个节点是执行者（owner），照常构块、执行；
- 同机其余密钥设 `N42_EL_OWNER_RPC=127.0.0.1:<owner http 端口>`，用 `DelegatedExecutionLayer`（`crates/n42-node/src/el/delegated.rs`）代替 `RethExecutionLayer`：`new_payload` 轮询执行者的 `eth_getHeaderByHash`，块在执行者的规范链上就返回 Valid，超时（`N42_EL_OWNER_WAIT_MS`，默认 2 s）返回 Syncing；FCU 同样只问执行者；不构块。
- 需要 `N42_COLOCATED_VOTE=1`：块要到提交后才进执行者的规范链，委托密钥若等执行结果再投票就死锁。

**没做的（下一步）。** 委托密钥当 leader 时要把构块请求转给执行者；现在不支持，所以本轮把 `N42_LEADER_TENURE` 设到 1,000,000，让 node0 一直出块。委托节点仍然启动自己的 reth（停在创世），仍然解码收到的块数据。

**范围。** 仅限同一运营者在同一台机器上的密钥：委托密钥是在执行者的检查上加签名，不是独立执行。结果口径是 E=1，与七节点各自执行的数字分开报告。

**脚本。** `chain94-fleet.py start --execution-owner N`（记入 `run.json`）；`sample`/`verify` 在这种拓扑下只向执行者要区块，并要求每个密钥已提交的块都在执行者的链上，输出标明 “one execution layer”。轮次脚本 `run-legO.sh`、`run-qroundO.sh` 只向执行者注入。

**AP 轮**（`run114*`、`run115*`，`qgMa/`、`qgMb/`；单机 7 密钥、1 个执行层，leader 固定 node0，只向 node0 注入 56 条连接）：

| 轮次 | 收尾校验 | leader 满块间隔中位 | 16 个满块跨度内 TPS |
|------|------|------|------|
| run114 | PASS（one execution layer，771 块） | 780 ms | 约 125,000 |
| run115 | PASS（one execution layer，766 块） | 778.5 ms | 约 110,000 |

功能通，但比各自执行还慢。原因在日志里：区块按帧引用传播，委托节点没有收到帧（只向执行者注入），报 `cannot rebuild block payload; waiting for the full copy`，投票要等 14 MB 的完整副本。下一轮先让帧照旧广播到所有节点；之后让委托密钥不解码块体就能投票。另：这一轮跑的时候我同时在本机跑了 clippy 和测试，数字本身也不干净。`wall-tps.py` 在这种拓扑下报 `KeyError`，要修。
