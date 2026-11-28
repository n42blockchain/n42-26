# devlog-195：Range 追块提速 Stage 0（分段计时）与 Stage 1（流水线导入）

日期：2026-11-26　分支：`perf/profile1-catchup-pipeline`（基于 origin/main 80ecf52）

## 1. 背景

S1 从创世追 2982 块用时 66.9 s，即 **22.4 ms/块**，其中 `new_payload` 占 17.1 ms（QMDB root 3.4 ms）。剩下约 5.3 ms 是共识层（CL）侧的准备和串行等待，此前没有计时：
- 第二次完整解码 `decode_wire_block`；
- `build_gov5_gossip_execution_data` 重新编码、重新哈希；
- 逐块串行的 new_payload → FCU → 下一块。

设计说明 `docs-drafts/catchup-overlap-design-20261126.md` 原文附在本文末尾（附录）。本轮只做其中的 Stage 0 和 Stage 1（b1 + a）。

## 2. Stage 0：分段计时

新模块 `crates/n42-consensus-service/src/orchestrator/range_import.rs` 中的 `CatchupTimings`：
- 每个分段既打直方图 `n42_range_catchup_stage_ms{stage}`，也累加进本次 run 的总量。
- 一次 run 从第一块开始导入算起，到 `range catch-up stopped` 时输出一行 info `range catch-up import timings`，内容为 blocks、batches、fcus、wall_ms、ms_per_block、attributed_ms，以及每段的总量和每块均值。随后清零，开始下一次 run。

分段定义：

| stage | 所在位置 | 含义 |
|---|---|---|
| decode | loop（串行路径） | 再次解码缓冲的 RLP |
| admit | loop | Profile-1 头部准入 |
| build | loop（串行路径） | 重建 `ExecutionData` |
| new_payload | loop（串行路径） | 等待 `new_payload`（含通道往返与 engine 排队） |
| wait | loop（流水线路径） | loop 等下一个 engine 结果或下一个已准备好的 payload，即关键路径 |
| fcu | loop | 等待 `forkchoice_updated`（串行逐块，流水线每批一次） |
| post | loop | 头部前移后的逐块收尾：vote release、deferred finalization、lineage 证明与状态文件、客户端记账 |
| other | loop | 批内未被上述分段覆盖的剩余时间 |
| gap | loop | 两批之间：orchestrator 空闲或在处理其他事件（含等待抓取） |
| prep | worker（与 loop 重叠） | 流水线路径中，在 worker 上解码并构建 payload |
| engine | submitter（与 loop 重叠） | 流水线路径中，submitter 看到的 `new_payload` 耗时 |

`other` 按批内墙钟时间减去各分段之和得出，`gap` 为两批之间的时间。因此 loop 侧各段相加按构造等于 run 的墙钟时间，22.4 ms 可以完整拆分。prep 和 engine 与 loop 重叠，单独列出，不计入这个和。

reth 侧的各项拆分（payload_validation、执行、QMDB root 各阶段）沿用已有直方图，见设计说明 §1。n42-rs 建议单独测量 QMDB WAL 的 fdatasync，结论如下：
- **fdatasync 已有独立计时**：`n42_qmdb_wal_fsync_ms`（`qmdb_state_root.rs` 的 `append_wal_frame`），嵌套在 `n42_qmdb_wal_append_ms` 和 `n42_qmdb_root_job_ms{stage=commit}` 之内，无需再加计时。
- **本机 /data 盘（NVMe）探针**，各重复 300 次：
  - 一个 37.6 KB 的 WAL 帧（S1 每块平均）写入加 fdatasync：0.06 ms；
  - consensus state 保存（写临时文件、fsync、rename、目录 fsync）：0.17 ms；
  - lineage 追加加 fdatasync：0.02 ms。

  可见在这台机器上 fsync 不是 3.4 ms 的主要部分。按 n42-rs 的建议，每次 run 只 fsync 一次属于 Stage 3，需要 kill -9 崩溃测试，本轮不改。
- **n42-rs P1_RUN_ROOT_DESIGN 指出的两项开销，n42-26 都没有**：
  - 整个 twig 重哈希：每块追加叶子走 `OpenTwig::fold_pending_leaves`，只重算追加区间以上的路径。整 twig 的 `recompute` 只出现在 reopen、重建和校验路径上。
  - 线程池切换：`LiveIndex` 小于 8192 个操作的批次在调用线程上执行，S1 每块 743 个操作。

  所以 3.4 ms 更可能来自 derive/sort、读视图发布或锁。需要在 S1 重跑时看 `n42_qmdb_root_job_ms{key,derive,sort,commit}` 和 `n42_qmdb_read_view_build_ms` 来确认。

## 3. Stage 1：流水线导入（b1 + a）

`drive_range_catchup` 取到就绪批次（最多 32 块）后：`N42_RANGE_PIPELINE=0` 走原来的逐块路径（`import_range_batch_serial`，不改语义，只加计时）；默认（深度 4，上限 32）走 `import_range_batch_pipelined`。

- **payload 构建离开 loop**：一个 `spawn_blocking` worker 按高度顺序对批内每块做解码和 `build_gov5_gossip_execution_data`，经容量为 depth 的有界通道交给 loop，最多领先 loop depth 块。loop 结束后，worker 在下一次 send 时停止。
- **准入仍在 loop 上**：Profile-1 准入只检查头部与父头部，不依赖执行结果，因此可以在父块执行完成之前进行，按顺序逐块完成。
- **有序 submitter**：一个 tokio 任务按收到的顺序逐个调用 `new_payload_for(HISTORICAL_SEQUENTIAL)`，遇到第一个非 Valid 结果即停止，之后的块不会提交。最多有 depth 个 payload 已准备并准入、排在 engine 之前。

  没有让多个 `new_payload` 同时在途，原因是适配器在把消息送进 engine 之前还有 await（import gate、parallel import 的 spawn_blocking），并发调用不能保证进入 engine 的顺序；而 reth engine 本来就逐块串行验证。逐个提交只多一次唤醒（微秒级），CL 侧工作已与 engine 的执行重叠。
- **每批一次 FCU**：
  - 在 Valid 前缀中，逐块判断该块是否推进头部：其 view 必须高于 execution-validated view 以及批内此前各块的 view。这与串行路径逐块的判断完全相同。
  - 只对最后一个推进头部的块发一次 FCU，它会把下面的祖先一起变成 canonical。
  - FCU 成功后，按块顺序逐块执行串行路径在 FCU 之后做的事：
    - 推进头部的块：更新时间戳、`advance_execution_validated_head`（lineage 证明与状态文件）、`head_block_number`、`mark_catchup_commits_finalized`、prune、`complete_deferred_finalization`；
    - 所有块：`BlockImportedWithParent`、`range_catchup.imported`、`note_committed_block`、计数。
  - 因此头部和 execution-validated view 只在 FCU 后移动；vote release 与 deferred finalization 仍然每块一次，按顺序执行。
- **批中途 INVALID**：
  - submitter 在出错块处停止，loop 处理完之前的 Valid 结果；
  - FCU 只发到出错块之前最后一个推进头部的块，不越过出错块；
  - 出错块按串行路径的失败流程处理：warn；对 strict anchor 以下的块，Profile 1 做 E12 检查；`import_failed` 重试或停止。
  - FCU 失败时，第一个推进头部的块按失败处理，它之前的块照常收尾。
- **不变量**：
  - 严格按序 release；
  - k=1 deferred root 不变：engine 仍逐块串行，root job 仍在 `new_payload` 内完成；
  - 每批末尾的 head/finalized FCU（E14 tags 由 `fcu_state` 生成）；
  - `CaughtUp` 交接与 staged 后缀的 release 路径不变；
  - persistence threshold 仍为 0。

## 4. 测量（本机，`range_catchup_ms_per_block_bench`，release）

条件：真实的 1,200 笔转账块；mock engine 每块固定耗时（tokio sleep 17 ms，实测约 18.1 ms），一次只处理一块；300 块和 600 块两组。

| 路径 | engine | ms/块（墙钟） | 分段（每块） | FCU 次数 |
|---|---|---|---|---|
| 串行（depth 0） | 17 ms | 20.20（600 块：20.17） | decode 1.12、build 0.97、new_payload 18.10、other 0.01 | 300（每块一次） |
| 流水线（depth 4） | 17 ms | 18.21（600 块：18.25） | wait 18.19、other 0.02；与之重叠：prep 2.17、engine 18.12 | 11（600 块：20） |
| 串行 | 0 | 1.85 | decode 0.84、build 0.96 | 300 |
| 流水线 | 0 | 2.00 | 受 prep 限制（单 worker 2.0 ms/块） | 11 |

结论：
- 流水线后，墙钟时间等于 engine 时间。本机 CL 准备（解码约 1.0 ms + 构建约 1.0 ms）完全移出关键路径，FCU 从每块一次降为每 32 块一次。
- mock 中 FCU 与收尾几乎不耗时。真实节点上，S1 的 5.3 ms 里还有 FCU 和 post（状态文件、lineage、finalization），这些在 Stage 0 的汇总行里会单独显示。其中 FCU 降到每批一次；post 仍每块执行，但集中在批末。
- 按设计说明，S1 目标约 17–18 ms/块。需用 S1 300 块重跑确认：wall/块 ≈ engine 时间，`fcus` ≈ 块数/32，头部哈希与 QMDB root 逐位一致。

## 5. 测试

`n42-consensus-service` 新增：
- `pipelined_range_import_matches_the_serial_import_with_one_fcu_per_batch`：新节点追 300 块，串行与流水线各跑一次：
  - 每个高度恰好导入一次且按序；
  - 最终 head、hash 与 FCU head 正确；
  - 两条路径的 execution-validated view 相同；
  - engine 同一时刻只处理一个 payload；
  - FCU 次数：串行 300 次，流水线不超过 11 次。
- `an_invalid_block_mid_batch_stops_the_batch_without_an_update_past_it`：第一批的第 10 块返回 INVALID：
  - 之后的块不再提交；
  - 只发一次 FCU，指向第 9 块；
  - head 停在 9，客户端仍处于活动状态；
  - 重试时导入第 10 块及之后的块，每块一次。
- `range_catchup_ms_per_block_bench`（ignored）：第 4 节的测量；可用 `N42_RANGE_BENCH_ENGINE_MS` 和 `N42_RANGE_BENCH_BLOCKS` 调整参数。
- 原有 range 测试全部通过：默认走流水线路径。

结果：
- `cargo clippy -p n42-consensus-service -p n42-node -p n42-network --all-targets -- -D warnings`：通过。
- `cargo test -p n42-consensus-service -p n42-node -p n42-network`：全部通过。

## 6. 后续

1. S1 重跑 300 块：用 Stage 0 汇总行补齐设计说明 §1 的估计行，确认 Stage 1 达到约 17–18 ms/块。
2. 如果 post 偏高，把批末的逐块收尾与下一批的 prep、执行重叠。
3. prep 单线程为 2 ms/块，engine 更快时会成为上限。到那时改为并行转换（n42-rs 的做法）。
4. Stage 2（预读窗口内批量验证发送者）、Stage 3（异步 own root，需先通过 kill -9 崩溃测试；WAL fsync 合批也在这一阶段）。

## 附录：设计说明原文（docs-drafts/catchup-overlap-design-20261126.md）

# Range catch-up: overlapping execution (design note, 2026-11-26)

Status: draft, no code changes. Code read at `origin/main` 80ecf52 and reth `n42/v2.11.0`.
Input: scale run S1, ~1200 tx/block, 2982 blocks from genesis in 66.9 s = **22.4 ms/block wall**
(44.6 blocks/s). `new_payload` 17.1 ms/block (51.2 s), of which the QMDB root job is 3.4 ms. Fetching
is pipelined and does not limit the run.

## 1. Where the time goes

The import is fully serial. `drive_range_catchup` (orchestrator/range_sync.rs) takes up to
`RANGE_IMPORT_BATCH = 32` ready blocks and, for each one, on the orchestrator task:
`decode_wire_block` (the second full decode: the decode lane on the blocking pool keeps only the
metadata and the RLP) → `profile1_admit_block` → `build_gov5_gossip_execution_data` (clones the body,
encodes every tx into the payload, `try_into_block` decodes them again, up to two `hash_slow` and the
native hash) → **await** `new_payload` → **await** `forkchoice_updated` (mod.rs
`import_h2_v4_catchup_block`) → `complete_deferred_finalization` and `BlockImportedWithParent`. Only
then does block N+1 start. In reth, `on_new_payload` runs the whole validation synchronously on the
engine thread. The root job's `finish` runs inside it, after execution, so N is inserted into the tree
(and N+1 can start) only once N's QMDB commit and its WAL `fdatasync` are done. MDBX persistence
(threshold 0) runs on reth's persistence thread and is already off the critical path, unless the
backlog reaches the backpressure threshold of 16 blocks.

| Share (ms/block) | Component | Measured by | Gap |
|---|---|---|---|
| ~5.3 (22.4 − 17.1) | CL prep (decode, admit, payload rebuild), FCU, finalize/engine events, the await hops | FCU: `n42_evm_path_duration_ms{path=historical_sequential,phase=forkchoice_updated}`; per-block wall: interval between consecutive `catch-up parent reached new_payload(Valid)` lines | **no timer on CL prep** (decode + `build_gov5_gossip_execution_data`) |
| part of 17.1 | Channel hop + engine queueing | `n42_evm_path_duration_ms{phase=new_payload}` minus reth `sync.block_validation.total_duration` | none needed |
| part of 17.1 | Payload well-formedness, tx decode, tx root (`ensure_well_formed_payload`) | reth `payload_validation` + `n42_import_stage_ms{pre_provider_factory}` | only as a remainder |
| part of 17.1 | Sender recovery (all misses during catch-up: senders were never admitted here) | `n42_alt_sig_payload_sender_total{cache=miss}`; stalls in reth `sync.execution.transaction_wait_histogram`; `n42_import_stage_ms{await_convert}` | no per-block recovery CPU time |
| ~10 (est.) | EVM execution, receipts root, post-execution checks | reth `sync.execution.execution_histogram`; `n42_import_stage_ms{pre_execute,post_execution,post_execution_total,execute_to_root_start}` | — |
| 3.4 | QMDB root job (`finish_deferred` on Profile 1) | log `State root job finished strategy=gov5-qmdb`; `n42_import_stage_ms{state_root}`; split by `n42_qmdb_root_job_ms{key,derive,sort,commit}`, `n42_qmdb_candidate_compute_ms`, `n42_qmdb_wal_append_ms`, `n42_qmdb_wal_fsync_ms`, `n42_qmdb_read_view_build_ms`, `n42_qmdb_lock_*` | — |
| part of 17.1 | Tree insert (state cache, deferred trie) | `n42_insert_executed_stage_ms{state_cache,deferred_trie}`, `n42_import_stage_ms{validate_total}` | — |
| off path | MDBX persistence | reth `persistence_duration`, debug `Saved range of blocks` | backpressure hits are not counted |

The first action is a 300-block S1 rerun with these histograms scraped, to fill in the "est." rows.
n42-rs reaches ~0.12 ms per QMDB root. Our 3.4 ms is therefore most likely derive/sort, WAL `fdatasync`
and read-view publication, not tree hashing. The split above settles it.

## 2. Options

**(a) Batch import: one FCU per N blocks, persistence threshold > 0.** Submit N..N+31 with `new_payload`
and send one FCU to the last block. Reth already executes on non-canonical in-tree parents, and the
client already continues from its own position for blocks imported without an FCU (repeated-view path).
Gain: the FCU and part of the per-block orchestration, est. 0.5–1.5 ms/block. A threshold > 0 changes
almost nothing on the critical path, because persistence is already asynchronous. The node also refuses
to start with a non-zero threshold (`validate_immediate_persistence`, bin/n42-node/src/main.rs; devlog-177:
kept "until a partial-persistence design proves QMDB and restart safety"). Leave it at 0.
*Must hold:* `head_block_number` / `execution_validated_head_view` move only on the FCU. Vote release
(`BlockImportedWithParent`) and deferred finalization still run for every block, in order. Committed
blocks below a strict anchor are noted (`note_committed_block`) only after the FCU makes them canonical.
Complexity low, risk low.

**(b) Overlap N+1's EVM with N's root and persistence.**
*(b1) Pipelined submission (no reth change).* A dedicated import task holds 2–4 `new_payload` calls in
flight; CL prep for N+1..N+k runs on the decode lane (keep the decoded block and the built
`ExecutionData` in `RangeBlock` instead of the RLP alone). The engine serializes the payloads, so
N+1 starts the moment N is inserted. This removes the CL prep and the hops from the critical path:
est. −3 to −5 ms/block. A failure of N turns the queued children into invalid-ancestor/Syncing replies.
Treat those as "retry from N" and do not count them as faults.
*(b2) Asynchronous own root on deferred chains (Profile 1, k=1 only).* `finish_deferred` needs only
the parent's filed fields (`executed_fields(N-1)`) to judge N. N's own root is needed first by N+1's
check. Split the job: check carried fields synchronously, then hand `compute_and_commit_own` and
`remember_executed_fields` to one ordered QMDB committer thread. N+1's `finish` waits on the filing
(the condvar wait helper next to `executed_fields` already exists). Gain: up to the 3.4 ms, minus the
part of it already outside the commit.
*Must hold:* QMDB commits in block order on one branch (the commit lock already serializes them). WAL
frame of N durable before reth persists N, or a startup reconcile that replays QMDB from reth's DB when
the WAL lags; verify which exists before shipping. With `N42_QMDB_READS=on|only`, N+1 reads state
from N's QMDB read view, which is built at commit; b2 then needs `off`/`verify` during catch-up or a read
view published before hashing. Not applicable to non-deferred headers, which commit their own root and
must keep it on the Valid path. Complexity medium, risk medium.

**(c) Batch sender recovery across a window.** In the decode lane, after decoding a reply, verify the
senders of a lookahead window (~16–32 blocks ≈ 20–40k tx) with rayon. Ed25519 (0x50) senders can go
through `verify_batch`, ECDSA through `SenderRecoveryCache::recover`. Fill the shared caches. Do not
prefill the whole 256-block reply: 307k entries exceed the 131,072-slot `SenderRecoveryCache`, and
`AltSigSenderCache` drops entries under load. Gain: whatever `transaction_wait` plus `await_convert`
show (est. 0–3 ms). Ed25519 batch verification also halves the CPU cost.
*Must hold:* only senders recovered from the exact keyed hash enter a cache (both caches trust a hit
without re-verification). A miss must still fall back to verification. Complexity low, risk low.

**(d) Per-run QMDB root (one root per batch, checked against the last header).** QMDB is an append log:
the root after blocks N..N+k depends on each block's operations in order, so blocks cannot be merged.
The only saving is deferring the upper-level hashing and the WAL `fdatasync` to the end of the run.
With k=1 every header N+1 carries root(N), and `finish_deferred` files N's fields for that check.
Skipping the intermediate roots means the intermediate headers' carried state roots go unchecked:
E12 evidence (`record_carried_mismatch`) and divergence detection move to the end of the run, and a
mismatch needs bisection. Acceptable only below a strict (commit-QC) anchor. Gain: bounded by the
3.4 ms, and mostly redundant with b2. A group `fdatasync` (one per batch) is the cheap, separable part,
under the same WAL-vs-reth restart invariant as b2. Complexity high, risk high. Defer.

**(e) Parallel EVM.** `crates/n42-parallel-evm` (Block-STM prototype) and `bin/n42-evm-bench` are
revm-only and excluded from the workspace since the evm2 migration (devlog-186).
`ExecutionPath::{HISTORICAL,LIVE}_PEVM` are reserved and rejected by the Engine API adapter
(`uses_current_engine_api` is false). Reth's own parallel path (`execute_block_bal`) needs an EIP-7928
block access list, which gov5 blocks do not carry. Porting to evm2, plus a differential qualification
(state, receipts, QMDB operations), is a project in its own right. It is the only lever on the
~10 ms EVM floor, and it is not part of this plan.

## 3. Recommendation and staged plan

| Stage | Change | Target ms/block | Confirm with |
|---|---|---|---|
| 0 | Add a CL-prep timer (`n42_range_catchup_prep_ms`) and a per-block import wall histogram. Rerun S1 (300 blocks) and fill §1 | 22.4 (baseline) | §1 rows add up to the wall within 5% |
| 1 | (b1) Build the payload on the decode lane, 2–4 payloads in flight + (a) one FCU per ready batch | ~17–18 (≈56 blocks/s) | wall/block ≈ `new_payload` engine time; `phase=forkchoice_updated` count ≈ blocks/32; S1 from genesis bit-identical head hash and QMDB root |
| 2 | (c) Lookahead sender verification (Ed25519 batch) | −0–3 | `transaction_wait` and `await_convert` near 0; `cache=miss` stops growing during import |
| 3 | (b2) Async own root on Profile 1, after the WAL/restart invariant is proven (kill -9 mid-catch-up, restart, compare QMDB root at head) | ~13–14 (≈75 blocks/s) | `n42_import_stage_ms{state_root}` < 0.5 ms; no `parent_unknown` refusals; crash test green |
| — | (d) only if stage 0 shows `fdatasync` dominant and b2 is blocked; (e) separate project | | |

Stages 1 and 2 touch only the CL and the payload conversion, and keep every reth and QMDB invariant
as it is today. Stage 3 is the first change to the root-job contract and needs its own review.
Expected end state: ~1.6× (≈70–75 blocks/s, ~85k tx/s at S1 block size), after which EVM execution
(~10 ms/block) is the floor.
