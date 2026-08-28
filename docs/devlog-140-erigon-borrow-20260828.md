# Devlog 140：借鉴 Erigon 近三月变更的六项落地（2026-08-28）

日期：2026-08-28
分支：`perf/erigon-borrow-20260828`（自 `main` @ `d35014a` 切出，未合并）

## 背景

一份把 Erigon 2026 年 5--8 月变更映射到本仓库的 review 列出了六项可直接借用的改动。
本轮按 brief 中给出的 file:line 逐项落地，每项一个提交、带测试；能测的给数字，不能测的
写明原因。原 review 文档（`docs/review/erigon-3month-review-2026-08-28.md`）在 gov5
工作区中并不存在，本轮以 brief 正文里的 file:line 为准，实际代码位置均已核对。

提交顺序：

| 提交 | 标题 |
| --- | --- |
| `65421e7` | perf(qmdb): append and fsync the WAL outside the state lock |
| `0fe21c3` | feat(parallel-evm): count Block-STM executions, aborts and fallbacks |
| `e941766` | perf(codec): reuse zstd contexts and Snappy encoders across calls |
| `9a6ed66` | fix(interop): bound a whole gov5 range response and keep protected cache |
| `2325233` | perf(interop): serve gov5 ranges from one consistent view per batch |
| `4b83fe7` | perf(mobile): execute witnesses against the tip state when the parent is head |

每个提交都跑过 `cargo build`、所涉 crate 的 `cargo test`、`cargo clippy --all-targets -- -D warnings`
和 `cargo fmt`。按要求没有启动 devnet fleet，也没有跑任何超过两分钟的饱和基准。

## 1. QMDB 提交锁：WAL 追加与 fsync 移出 `state` 互斥锁

文件：`crates/n42-node/src/qmdb_state_root.rs`

### 改了什么

原 `compute_and_commit` 在全局 `state` 互斥锁内完成树重建、插入 `blocks`、然后
`append_wal_block`：每块一次 `create_dir_all` + `OpenOptions::open` + `metadata`，
再 `write_all` + `sync_all`。fsync 占了锁持有时间的大头，期间 `compute_candidate`
（leader 构块的 exec_cache 路径）和 archive RPC 读全部被挡住。

现在的顺序：

1. 新增 `commit: Mutex<()>`，把整条提交串行化（树重建 → 插入 → WAL）。WAL 记录顺序等于
   插入顺序，子块也不可能在父块落盘前被发布。
2. `state` 锁只覆盖树重建、根校验、WAL 记录编码（借用 `StoredQmdbBlock`，无克隆）和插入；
   插入后把 `pending_durable = Some(block_hash)` 记在 state 里，释放锁。
3. 锁外通过持久句柄 `wal: Mutex<Option<QmdbWalFile>>` 写帧并 `sync_data`（纯追加，
   POSIX 语义下足以覆盖数据及其可读所需的尺寸元数据）。句柄在 `persistent()` 完成撕裂尾
   截断之后打开一次，不再每块重开。
4. 重新加锁：成功则清 `pending_durable`、发布 `cached_tip`；失败则从 `blocks` 移除并按原
   语义返回 `Persistence` 错误——WAL 失败的块不算已提交。写失败时文件按 `QmdbWalFile::len`
   回滚到上一帧边界。

在途窗口内的可见性：archive 读（`contains`、`root_for`、`snapshot_for`、`proof_for`、
`distance_from_base`、`parent_for`、`retained_block_count`）通过 `durable_block()` 把
`pending_durable` 视为不存在；`compute_candidate` 允许在它之上推测计算——这与原来
"在一个随后提交失败的父块上算候选"是同一种情形，后续提交会以 `MissingParent` 失败。

新增指标：

- `n42_qmdb_wal_fsync_ms`：只覆盖 `sync_data`；`n42_qmdb_wal_append_ms` 继续覆盖整个追加。
- `n42_qmdb_tip_cache_total{outcome="hit"|"miss"}`：`reconstruct_tree_locked` 的 `resumed` 分支。

### 测试

- `wal_append_failure_rolls_back_the_insert`：注入写失败，检查内存图、缓存 tip、磁盘长度、
  重开后的 store 都不含该块；不带故障重试成功并可持久恢复。
- `wal_append_does_not_hold_the_state_lock`：把追加卡住 400 ms，主线程在此期间对在途块之上
  算候选并读 archive，断言耗时 < 100 ms；在途块对 `contains`/`root_for` 不可见，落盘后可见，
  候选根与冷重建一致。
- `borrowed_wal_record_encodes_identically_to_the_owned_record`：借用记录与原 owned 记录
  bincode 字节相同，WAL 格式不变。
- `tip_cache_metric_counts_resumed_and_cold_reconstructions`。

### 数字

`qmdb_commit_lock_hold_vs_wal_append`（ignored，200 块 × 32 ops，持久 store，
tempdir 在 `/home` ext4 上），三次取样：

| 指标 | 改前 mean / p50 / p99 / max | 改后 mean / p50 / p99 / max |
| --- | --- | --- |
| `n42_qmdb_lock_hold_ms{commit}` | 1.49 / 1.49 / 1.85 / 2.19 ms | **0.42 / 0.41 / 0.63 / 1.04 ms** |
| `n42_qmdb_wal_append_ms` | 1.18 / 1.17 / 1.35 / 1.73 ms | 1.09 / 1.08 / 2.2 / 3.7 ms（已在锁外） |
| `n42_qmdb_wal_fsync_ms` | 无 | 1.08 / 1.07 / 2.2 / 3.7 ms |
| `n42_qmdb_candidate_compute_ms{commit}` | 0.26 ms | 0.26 ms |
| 200 块总耗时 | 363--371 ms | 367--374 ms |

提交锁持有时间 −72%；剩余 0.42 ms 基本就是树重建 + 一次 tree clone 发布缓存。
单线程总耗时不变是预期的：fsync 本身没有消失，收益是并发方能穿插进来。
WAL 追加 mean 从 1.18 降到 1.09 ms，对应省掉的每块 open/metadata。

### 风险

- 在途窗口内 RPC 读会短暂看不到刚提交的块（原来会阻塞到落盘再返回）。调用方（rpc.rs
  archive 查询）本就以 `None` 处理未知块，无行为回归。
- `commit` 锁把提交串行化，与 reth engine tree 逐块调用 `finish` 的形状一致，不引入新等待。

## 2. Block-STM 计数

文件：`crates/n42-parallel-evm/src/{worker.rs,scheduler.rs,lib.rs}`

新增：`n42_parallel_evm_executions_total`（每个 Execute 任务）、
`n42_parallel_evm_validation_failures_total`（每次 Validate 失败）、
`n42_parallel_evm_reexecutions_total`（每次 `abort_and_reschedule`，即验证驱动的 abort 次数，
对应 Erigon 的 reexecutions 口径；被级联标记为 REDO 的高位 tx 在其自身 Execute 时计入
executions）、直方图 `n42_parallel_evm_rounds`、以及非收敛回退路径的
`n42_parallel_evm_sequential_fallback_total{reason="non_convergence"}`（原来只有 warn）。

测试：

- `abort_and_reschedule_counts_one_reexecution_per_abort`（scheduler 级，thread-local recorder）。
- `forced_conflict_increments_the_reexecution_counter`：32 笔 tx 全部 CALL 同一个 counter
  合约（SLOAD/SSTORE slot 0），8 个区块。rayon worker 看不到 thread-local recorder，测试通过
  进程级 `set_global_recorder` 装一个 `DebuggingRecorder`；注意 metrics-util 的
  `snapshot()` 会清空计数，三个计数必须来自同一次快照（第一版按名字各取一次快照，
  reexecutions 永远读到 0，已修）。

数字（`--test-threads=1` 单独运行）：256 笔 tx → executions 3,230、reexecutions 229、
validation_failures 229。即全部串行冲突时每笔 tx 平均执行 12.6 次；一次 abort 平均级联
重执行 ~13 笔。这正是决定 live 路径是否启用 Block-STM 前需要的数据（见"未做"一节）。

## 3. codec 复用

### zstd（`crates/n42-consensus-service/src/orchestrator/mod.rs`）

`compress_payload`/`decompress_payload` 及 consensus_loop 中两处执行输出解压改走
thread-local 的 `zstd::bulk::Compressor`/`Decompressor`（`zstd_compress_pooled` /
`zstd_decompress_pooled`）。`pooled_zstd_matches_one_shot` 在 0 B--3 MiB、同线程切换
level 后再切回的情形下与 `zstd::bulk::compress` 逐字节相同。

### Snappy（`crates/n42-network/src/snappy_pool.rs`，新模块）

`snap::write::FrameEncoder`/`read::FrameDecoder` 没有 reset 接口，每次 `new` 都分配
raw encoder 哈希表 + 两个 block 级 scratch（约 140 KiB）。新模块按线程保留一个
`snap::raw::Encoder`/`Decoder`，直接在其上实现 frame 格式（stream identifier、
64 KiB 分块、masked CRC-32C、压缩不足 1/8 时存原文）。CRC-32C 用 SSE4.2 指令，
无 SSE4.2 时退回查表，`crc32c_matches_known_vectors` 覆盖 RFC 3720 校验值和两种实现的一致性。

- `frame_encode_matches_snap`：0 B--3 MiB × {runs, noise, zeros} 与 `FrameEncoder` 逐字节相同，
  另有 12 MiB 用例；`frame_decode` 复刻 `FrameDecoder` 的全部检查，并在解码越过声明长度的
  第一个 chunk 处直接拒绝（原来靠 `.take(declared + 1)` 多解一字节再比较）。
- 接入点：gov5 RPC 分帧、Status 握手、H2 wire、H2 v4 gossip、leader 直推区块。

### 数字（release，ignored 基准，同一进程内新旧对比）

| 负载 | 操作 | 旧 | 新 |
| --- | --- | ---: | ---: |
| Snappy 1 KiB | frame encode | 1.02 µs | **0.58 µs** |
| Snappy 1 KiB | frame decode | 1.04 µs | **0.36 µs** |
| Snappy 16 KiB | frame encode | 12.8 µs | 12.1 µs |
| Snappy 16 KiB | frame decode | 10.8 µs | **7.9 µs** |
| Snappy 1 MiB | frame encode / decode | 1.66 / 0.76 ms | 1.66 / 0.74 ms |
| Snappy 12 MiB | frame encode / decode | 19.9 / 9.9 ms | 19.8 / 9.4 ms |
| Snappy raw 1 KiB--12 MiB | encode / decode | 差异 < 2% | |
| zstd L3 16 KiB | compress / decompress | 26.3 / 15.3 µs | 25.3 / 15.6 µs |
| zstd L3 1 MiB | compress / decompress | 2.42 / 0.81 ms | 2.42 / 0.80 ms |
| zstd L3 12 MiB | compress / decompress | 30.9 / 10.2 ms | 30.9 / 10.0 ms |

结论：上下文创建是固定成本，只在小消息上可见（1 KiB frame 解码 −65%，16 KiB −27%）；
1 MiB / 12 MiB 的差异都在噪声内（≤ 4%）。zstd 的 CCtx/DCtx 创建本来就只有微秒级，
大负载上没有可测收益，这项改动对 zstd 主要是消除每块一次的分配而非提速。

## 4. gov5 range 接收预算 + live cache 驱逐修正

`crates/n42-network/src/gov5_rpc.rs::read_response`：每个 chunk 各限 64 MiB、每响应
1024 个 chunk，合起来仍允许单个响应物化 64 GiB。现在 `read_range_response` 累计已解码字节，
预算 `MAX_GOV5_RANGE_RESPONSE_BYTES = finalized_range::MAX_MATERIALIZED_FINALIZED_RANGE_BYTES`
（256 MiB，与 finalized-range 物化上限同源，测试断言二者相等）。剩余预算作为下一
chunk 的解码上限传给 `read_framed_payload`，越界的 chunk 在声明长度处即被拒绝，不先解码。
测试 `bodies_by_range_response_is_bounded_as_a_whole` 以 4 个小块把预算钉在精确边界：
等于总量通过，少一字节拒绝，生产常量放行。

`crates/n42-consensus-service/src/observer.rs::trim_gov5_live_cache`：当所有缓存块都被
finalized catch-up 保护时，原来 `position(..).unwrap_or(0)` 会驱逐 index 0——恰好是马上要
应用的 lineage 最老一块，导致重新 fetch。现在跳出循环、计 `n42_gov5_live_cache_over_cap_total`
并 debug 记录；保护解除后从头恢复驱逐。测试
`test_gov5_live_cache_keeps_everything_when_all_entries_are_protected` 覆盖两个阶段。

## 5. gov5 range 服务端一致性 + 阻塞工作下沉

`bin/n42-node/src/main.rs`：两处相同的闭包收敛为 `gov5_canonical_block_reader`。
它按批打开一个 `BlockchainProvider::consistent_provider()`（一个 DB 只读事务 + 一份内存
canonical 快照），`block_by_number` 与 `block_hash` 都从这一个视图取，再交给
`encode_gov5_block_rlp`（`hash_slow()` 复核保留）。

没有为整个 range 只开一个视图：range 最多 1024 块，写 socket 的节奏由对端决定，
一个跨整个 range 的 MDBX 只读事务可能持续数秒到数十秒，会阻止页回收并触发 reth 的
长事务保护。所以视图的生命周期只覆盖一个批次的同步读取，不跨 socket 写。

`crates/n42-network/src/gov5_rpc.rs::write_response`：`Gov5CanonicalBlockReader` 新增
`new_ranged`（range 闭包，单块读从中派生；`new` 保留给现有调用与测试）。Stream 分支按
32 块一批调 `build_gov5_range_batch`（存储读、RLP 解码、块号/父链校验、Snappy 分帧），
有 Tokio runtime 时 `spawn_blocking`，且提前一批预取：写第 k 批的同时第 k+1 批已在阻塞线程上
读取编码；没有 runtime（codec 单测用 `futures::executor::block_on`）时内联执行。
错误语义与原逐块循环一致（not found / 超限 / 非法 / 块号不符 → 错误帧并停止；父链断裂 →
静默截断）。

测试：`bodies_by_range_pipelines_batches_on_blocking_threads`（multi-thread tokio，
3 批，缺块落在批边界，校验链接前缀 + 末尾 "block not found" 帧，并断言每批 ≤ 32 块）；
既有 7 个 range 测试覆盖内联路径。未做端到端计时：该路径的收益是 swarm task 不再被
DB/RLP/Snappy 阻塞，需要 devnet 才能测，本轮不跑 fleet。

## 6. 手机 witness 状态源

`crates/n42-node/src/mobile_packet.rs`：`parent_state_provider` 先比较 canonical head
（`best_block_number` + `block_hash`）与 `parent_hash`，相等则取 `latest()`，取完再比一次
head——中间若前进了（tip 状态可能已含子块）则退回 `history_by_block_hash`。新增
`n42_mobile_witness_state_source_total{source="tip"|"historical"}`，debug 日志带 `state_source`。

测试通过 `reth_provider::test_utils::MockEthProvider`（dev-dep 开 `test-utils`）驱动两条路径
并校验计数。

风险/说明：packet 通知来自 "block finalized" / "background import completed"，多数情况下
head 已经是该块本身，父块走 historical；只有 newPayload 完成而 FCU 尚未推进时才命中 tip。
命中率要看线上计数，这正是加这个计数的原因。

## 未做与原因

- **两阶段 QMDB 提交**（先发布再异步落盘 / 组提交）：本轮只把 fsync 移出锁，
  仍然是"同步落盘后才返回"。组提交需要改 reth engine tree 的 `finish` 契约（它等待根），
  收益上限是一次 fsync（~1 ms）对 8 s slot 不构成瓶颈，不值得动契约。
- **live 路径启用 Block-STM**：第 2 项的计数正是决策前提。强制冲突形状下每笔 tx 执行 12.6 次，
  说明真实负载的冲突率必须先在线上量到（reexecutions / executions）再决定；本轮不改执行路径。
- **分块增量 range 导入**（客户端边收边导入）：`read_response` 仍先物化整个响应
  （现在有 256 MiB 上限）。改成流式需要 observer 的 catch-up 状态机按块推进并处理中途断流的
  回滚，超出本轮范围；预算上限先把内存风险封住。
- **crc32c 依赖**：没有引入 `crc32c` crate，自带 SSE4.2 + 查表实现（约 40 行），避免新增
  审计面；aarch64 只走查表（snap 本身同样如此）。
- **其他 zstd 调用点**（mobile_packet 打包、jmt snapshot、mobile quic_client）未改：
  基准显示 zstd 上下文复用在 ≥ 16 KiB 负载上没有可测收益。
- **QMDB `n42_qmdb_lock_hold_ms` 的线上对比**：只在单测里量；线上直方图在下次 devnet 取样时
  可直接对比 0.4 ms 量级是否成立。

## 状态

六项全部落地并通过各自 crate 的测试、clippy（`-D warnings`）与 fmt；n42-node 182、
n42-network 196、n42-consensus-service 212（lib）、n42-parallel-evm 17、n42-node-bin 17
个测试全绿。分支已推送，未合并到 `main`。
