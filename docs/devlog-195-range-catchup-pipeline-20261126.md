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

---

## Stage 2：发送者提前验证（选项 c）

日期：2026-11-26　分支：`perf/profile1-catchup-sender-ahead`（基于 PR #53，6cb0953）

### S2.1 现状核对

- 引擎导入时确实读共享缓存。reth `payload_processor::spawn_tx_iterator` 对每笔交易调用 `N42EvmConfig::tx_iterator_for_payload` 给出的转换闭包，即 `recover_payload_transaction`：
  - secp256k1 走 PR #50 的共享 `SenderRecoveryCache`（131072 项，直接映射，`recover` 命中即返回，失败不缓存）；
  - 0x50 走 `AltSigSenderCache::global()`，未命中时再查已持有的 frame，最后自行验证并写入。
- 引擎的恢复本身就是并行的：交易数 ≥ 30 时，先串行转换前 4 笔，再在 reth 的 `cpu_pool` 上按 64、128、256… 的指数窗口并行转换，执行线程按序消费。`transaction_wait` 记录的就是执行等转换的时间。所以追块时发送者恢复全部 miss，但多数时间与执行重叠，主要代价是首个窗口的等待，以及与 prewarm、state root 争用同一个 CPU 池。
- 追块中的块从未经过本节点准入，两个缓存在 import 时都是 miss（设计说明 §1 已指出）。

### S2.2 设计

- **放在 n42-execution**：新模块 `crates/n42-execution/src/sender_warmup.rs`，即 `SenderWarmup`。consensus-service 不直接依赖 reth-evm，经 n42-execution 再导出 `SenderRecoveryCache`、`SenderWarmup`、`WarmStats`。
- **专用小线程池**：自建 rayon 池，线程名 `n42-sender-ahead-*`。`N42_RANGE_SENDER_THREADS` 默认取核数的 1/4，夹在 1–4，上限 16。工作不进 reth 的 `cpu_pool` 或全局 rayon 队列，不会让引擎的转换任务排在它后面。代价是操作系统层面多出几条线程争用 CPU。
- **secp256k1**：`cache.recover_with(tx, SignedTransaction::try_recover)`，与 `SenderRecoveryCache::recover` 是同一条路径：low-s 等规则不变，只缓存按交易自身哈希恢复出的发送者，失败不写入。闭包只在 miss 时运行，借此区分 cached 和 recovered。
- **Ed25519（0x50）**：先过滤掉已在 `AltSigSenderCache` 中的交易，余下的按 `ed25519_batch_size()`（默认 64）分块，在池上并行执行 `n42_tx_types::verify_batch`。该函数的逐笔结论与单笔 `verify` 相同，批失败时退回逐笔验证。只写入 `Ok`。如果链未开启 0x50（`alt_sig_enabled()` 为假），跳过这类交易，因为引擎无论发送者如何都会拒绝该块。
- **接口**：`ExecutionLayer::sender_warmup() -> Option<Arc<SenderWarmup>>`，默认返回 `None`。`RethExecutionLayer::with_sender_warmup` 由 `bin/n42-node` 在参与者 EL 上用同一个 `evm_config.inner().sender_recovery_cache` 构建。observer 和 delegated EL 不接入。
- **窗口（相对引擎）**：仅在流水线路径中启用。每批新增一个 warm-up 协调线程（`spawn_blocking`），与 prep worker 共享 `Arc<Vec<RLP>>`，自己再解码一次，因此可以领先引擎超过 prep 的 depth。
  - 推进条件：块 `index` 只有在 `index < answered + window` 时才处理，`answered` 为引擎已回复的块数。submitter 在每次 `new_payload` 返回后更新 `answered` 并 notify（`EngineProgress`：Mutex + Condvar）。
  - 引擎已越过的块跳过。
  - 批结束时，或 future 被丢弃时（`StopWarmup` 的 Drop），停止协调线程。
  - 因此，在引擎处理块 n 时，缓存中最多有 n .. n+W-1 这些块的预热条目。`N42_RANGE_SENDER_AHEAD` 默认 8，上限 32（`MAX_RANGE_SENDER_AHEAD`），`0` 关闭。批本身最多 32 块，即使回复有 256 块，也不会一次灌入缓存。
- **跨批**：窗口不跨批。每批第一块的预热与 prep 同时开始，而 prep 约 2 ms 后引擎就拿到这块，所以第一块通常只有部分命中。考虑过从 range 缓冲区预读下一批的 RLP，但需要复制或改为 `Arc` 存储，本轮不做。
- **未采用**：
  - 把恢复放进 decode lane（设计说明原文的位置）：那里看不到引擎进度，窗口无法相对引擎设限。
  - 在 prep worker 内同步预热：prep 是单线程，预热会直接拖慢 prep（1200 笔 ECDSA 单线程约 30 ms）。

### S2.3 计时与计数

- 新增 overlapped 分段 `warm`：`n42_range_catchup_stage_ms{stage="warm"}`，按块计，包含解码，不计入墙钟之和。
- `n42_range_catchup_warmed_senders_total{outcome=recovered|cached|failed}`。
- 引擎侧命中率：
  - `recover_payload_transaction` 的 secp256k1 分支新增计数，导出为 `n42_secp256k1_payload_sender_total{cache=hit|miss}`（与已有的 `n42_alt_sig_payload_sender_total` 并列）；
  - `n42_execution::PayloadSenderCounts::now()` 汇总两类 hit/miss；
  - `drive_range_catchup` 在每批前后取差值，计入 `n42_range_catchup_engine_senders_total{cache}`。
- 汇总日志 `range catch-up import timings` 新增 `engine_sender_hits`、`engine_sender_misses`、`warmed`、`warm_failed`。注意这些是进程级计数器，同时进行的其他导入（如 follower import）也会计入。

### S2.4 测量（本机 254 核；另有 live round 在跑，负载约 5–10）

`range_catchup_ms_per_block_bench`（release），300 块 × 1,200 笔转账，mock engine 每块 sleep 17 ms。新增 `N42_RANGE_BENCH_RECOVER`：
- `serial`：引擎先用真实的 `N42EvmConfig::recover_payload_transactions`（同一转换器、同一缓存，在全局 rayon 池上）恢复全部发送者，再 sleep；这是收益的上界。
- `overlap`：恢复与 sleep 并行，相当于恢复完全被执行掩盖；这是收益的下界。
- `off`：不恢复。

`N42_RANGE_BENCH_SENDER_AHEAD` 设定窗口。用 taskset 模拟 16 核和 8 核主机，此时预热池为 4 或 2 线程。

| 主机 | 引擎恢复模型 | 流水线，无预热（之前） | 流水线 + 预热 W=4 | W=16 | 引擎命中率（W=4 / W=16） |
|---|---|---|---|---|---|
| 254 核 | off | 18.24 | — | 18.23 | —（不恢复） |
| 254 核 | serial | 20.41 | 20.65 | 20.20 | 93.5% / 85.0% |
| 254 核 | overlap | 18.27 | — | 18.25 | — / 86.4% |
| 16 核 | serial | 21.03 | **18.78** | 19.27 | 93.5% / 85.0% |
| 16 核 | overlap | 18.23 | 18.25 | 18.24 | 93.5% / 85.3% |
| 8 核 | serial | 23.35 | **19.00** | 18.98 | 96.2% / 96.2% |
| 8 核 | overlap | 18.18 | 18.20 | 18.20 | 91.7% / 92.1% |

单位为 ms/块（墙钟）。串行路径（depth 0）作参照：254 核 serial 为 22.51，16 核为 22.87，8 核为 24.91。

254 核、serial 模型下的窗口扫描：W=2 时 20.90 ms（命中 96.8%），W=4 时 20.65（93.5%），W=8 时 20.54（89.0%），W=32 时 20.16（84.9%）。差异在噪声范围内。

结论（如实）：
- **本机（254 核）没有可测收益**。1,200 笔 ECDSA 在 254 核上并行恢复本来就很便宜，mock 中 serial 模型约 2.2 ms/块的开销主要来自 spawn_blocking、解码和 rayon 扇出，而不是 ecrecover；命中后这部分开销仍在。
- **核数少时收益明显，但只在恢复不能被执行掩盖时**：16 核约 −2.3 ms/块，8 核约 −4.3 ms/块（serial 模型）。overlap 模型下在任何核数都是 0。真实 reth 介于两者之间：执行与恢复重叠，只等第一个窗口，但两者争用 `cpu_pool`。所以 S1 节点上的实际收益取决于其核数，以及 `transaction_wait` 和 `await_convert` 的实测值。设计说明估计的 0–3 ms 仍然成立。
- **开启预热不拖慢关键路径**：off 模型（引擎不读缓存）下 18.24 对 18.23 ms。预热池在后台每块耗时约 10 ms（4 线程）或约 18 ms（2 线程），始终在关键路径之外。
- **窗口越大命中率越低**。这是直接映射缓存的冲突逐出，不是因为容量不够：W=16 时窗口内约 19k 条目，占 131072 个槽，某条目在被引擎读到之前被同槽后来者顶掉的概率约为 1−e^(−19200/131072) ≈ 14%。W=4 时约 4%。预热池的速度本来就是引擎的 2 倍左右，所以领先 4–8 块已经足够。**默认窗口据此从 16 降到 8**（评审决定；可配置）。

### S2.5 测试

n42-execution `sender_warmup::tests`：
- `warms_valid_senders_and_never_caches_a_failure`：
  - 40 笔有效交易全部恢复，写入缓存的发送者等于 `try_recover` 的结果；
  - 零签名和 high-s（可塑）签名的交易计为 failed，不写入缓存；
  - 第二次运行时，有效交易计为 cached。
- `warms_ed25519_senders_in_batches_without_caching_a_bad_one`：10 笔 0x50 交易批量验证后写入缓存，用他人密钥伪签的 1 笔不写入。

n42-consensus-service：
- `sender_warmup_keeps_the_import_identical_to_the_serial_one`：引擎真实恢复，120 块 × 8 笔，三种配置（串行 / 流水线 / 流水线 + 预热 W=16）：
  - 每个高度恰好一次且按序；
  - head、hash、execution-validated view 三者一致；
  - 缓存中的发送者都等于该交易自己恢复出的发送者；
  - 预热路径的 warmed ≥ 一半。
- `a_bad_signature_mid_range_is_refused_at_the_same_block_with_the_warmup`：第 10 块最后一笔为零签名，三种配置下：
  - 引擎都在第 10 块返回 INVALID，之后的块不提交，head 停在 9；
  - 坏交易从未进入缓存，预热侧记为 failed；
  - 重试时同样在第 10 块被拒绝，直至客户端停止，没有任何更高的块到达引擎。
- `the_sender_warmup_stays_inside_its_window`：96 块，engine 每块 3 ms，引擎本身不恢复，缓存中的条目全部来自预热。每当块 n 到达引擎时，探针统计缓存里 n+4 及以上各块的条目数：
  - W=4 时为 0；
  - W=32 时大于 0（对照组，证明探针有效）。
- `range_catchup_ms_per_block_bench`：增加预热配置，并新增 `N42_RANGE_BENCH_RECOVER` 和 `N42_RANGE_BENCH_SENDER_AHEAD` 两个参数。

结果：
- `cargo clippy -p n42-consensus-service -p n42-node -p n42-network -p n42-execution -p n42-node-bin --all-targets -- -D warnings`：通过。
- `cargo test -p n42-consensus-service -p n42-node -p n42-network -p n42-execution`：全部通过（consensus-service 370，n42-execution 159，n42-network 264，n42-node 315，以及各集成测试）。

### S2.6 遇到的问题

- **共享 target 目录串味**：`/data/n42-build/n42-26-follower` 由多个 worktree 共用。cargo 对 workspace 成员的 metadata 不含绝对路径，dep-info 用的又是相对路径，所以另一个 worktree 构建的 `n42-execution` 会被当成新鲜产物复用，表现为本次新增的符号"找不到"。处理办法：构建前先 `touch` 本次改动的文件。反过来，其他 worktree 也可能误用本 worktree 的产物，共用这个目录时需留意。
- **直接映射缓存的冲突**：64 槽缓存放 40 条目时就会互相顶掉。测试改为只断言缓存中现存条目的正确性，并对命中数设下限。

### S2.7 后续

1. 在 S1 300 块重跑时比较 `transaction_wait`、`n42_import_stage_ms{await_convert}`、`engine_sender_hits/misses` 与 wall/块，确认真实收益；同时据此决定默认窗口（建议 4–8）。
2. 若要消除批首块的部分 miss，可让下一批的预热提前开始，需要 range 缓冲区以 `Arc` 共享 RLP。
3. 如果 reth 的转换在大量命中后仍偏重（解码加 rayon 扇出），可以考虑把 prep 已解码的交易直接交给引擎，属于 reth 补丁范畴。
