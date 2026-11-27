# devlog-194：稳态负载下的内存增长归因与限界（B5）

日期：2026-11-26　分支：`fix/profile1-memory-b5`（基于 origin/main 80ecf52）

## 1. 背景

S1（验证者 3，约 1200 笔/块，约 150 tx/s）的 RSS 基本等于 RssAnon：

| 块高 | RSS |
|---|---|
| 约 126 | 739 MB |
| 约 1490 | 2.25 GB |
| 约 2980 | 2.80 GB |

斜率从约 1.1 MB/块降到 0.4 MB/块。清库后从创世重同步，峰值 2.86 GB，说明内存随链长增长，与运行时长无关。对照：n42-rs 运行 28 分钟约 520 MiB。

Q1（`N42_MEM_LOG_SECS=60`）的前 10 分钟：

| 时间 (Z) | rss | jemalloc allocated | jemalloc resident |
|---|---|---|---|
| 01:54 | 628 MiB | 2525 MiB | 2872 MiB |
| 02:04 | 820 MiB | 2562 MiB | 2978 MiB |

RSS 涨了 192 MiB，jemalloc allocated 只涨了 37 MiB。增长主要来自两处：
- 早已分配的零页被逐步写入。典型是 reth 的 cross-block 执行缓存：它按 4 GiB 预分配，页面惰性驻留。
- jemalloc 之外的内存，即 RocksDB 的 memtable。

## 2. 测量方法

- `crates/n42-node/tests/memory_footprint.rs`（新增，ignored bench）：在进程内用 jemalloc 分配器，按 `/proc/self/statm` 计算 RSS 增量。
  - `cross_block_execution_cache`：分别创建 4096 MiB 和 256 MiB 的 `ExecutionCache`，逐个插入账户（每 50 个账户插入一个存储槽）。
  - `qmdb_leaf_tree_per_entry`：用 `QmdbLeafTree::apply_sorted_ops` 插入 keccak 键和 10 字节值。
- 解析 S1 的 QMDB WAL：共 3144 个块记录、2,336,115 个操作（743 个/块）、377,089 个不同键（约 120 个新键/块），值平均 9.3 字节。WAL 共 118 MB，约 37.6 KB/块；期间没有发生 rebase。
- S1 的 RocksDB `OPTIONS` 与 `LOG`：28 次 flush，原因全部是 "Write Buffer Full"。其中 AccountsHistory 25 次，TransactionHashNumbers 3 次，每次 flush 的 memtable 约 128 MiB。另有约 30 个 WAL 文件，每个约 138 MB。
- 让一个 Explore 代理只读普查 n42 各 crate 里按块增长的集合（详见第 3 节）。

测量结果：

| cross-block 缓存 | 创建时 | 1 万账户 | 10 万 | 40 万 | 100 万 | 400 万 |
|---|---|---|---|---|---|---|
| 4096 MiB | +0 | +34 | +130 | +159 | +205 | +417 MiB |
| 256 MiB | +1 | +9 | +16 | +36 | +66 | +125 MiB |

| QMDB 叶树条目数 | 10 万 | 40 万 | 100 万 | 400 万 |
|---|---|---|---|---|
| RSS 增量 | +21 MiB | +65 MiB | +182 MiB | +802 MiB |
| 每条字节数 | 219 B | 170 B | 191 B | 210 B |

## 3. 归因表（S1，约 3000 块时）

| 结构 | 增长方式 | 单位成本 | 约 3000 块时 | 上限（修改前） | 本次处理 |
|---|---|---|---|---|---|
| 编排器 `committed_blocks` 环（同步服务用） | 每个提交块保留完整 BlockData 信封（`Arc`） | 约 98 KB/块（S1 中位数） | 约 290 MB | 10,000 块 ≈ **1 GB**，约 22 小时后才不再增长 | **加 256 MiB 字节预算**（`N42_SYNC_BUFFER_BYTES`），并始终保留最新 64 块 |
| RocksDB memtable（reth 的 OptimisticTransactionDB） | 写入即进 memtable，满 128 MiB 后 flush；旧 memtable 为冲突检查保留 | AccountsHistory 和 TxHashNumbers 各约 128 MiB 活跃 + 256 MiB maintain | 约 0.5–0.8 GB，在 jemalloc 之外 | 理论约 2.5 GB | 加 gauge；参数在 reth 中，只提出建议（见第 5 节） |
| reth cross-block 执行缓存 | 4 GiB 零页按访问到的账户和槽逐页驻留 | 见第 2 节测量表 | 约 160–200 MB | **4 GiB** | **默认值改为 1 GiB**（`--engine.cross-block-cache-size` 仍可覆盖） |
| 网络层 `gov5_served_blocks` | 每个广播或收到的块保留未压缩 RLP | 每块一份 RLP | 1024 块后满 | 1024 块且 1 GiB | **字节上限降为 256 MiB**；命不中时由 canonical 存储回答 |
| QMDB 驻留 ops + undo + 预备候选 | 有预算，几百块后就满 | 每块约 50–60 KB | 约 450 MB | 128 + 256 + 64 MiB | 加 gauge |
| QMDB 读视图缓存 | 有预算 | 节点共享 | ≤ 512 MiB（逻辑字节） | 已有上限 | 加 gauge |
| QMDB 活叶树 | 随状态规模增长 | 约 200 B/条目 | 377k 条目 ≈ 75 MB | 随状态增长 | 只做设计（见第 6 节） |
| QMDB `blocks` 索引 | 从 base 起每块一项，rebase 时不缩 | 约 200 B/块 | 约 0.6 MB | **无上限**（很小） | 加 gauge；列为后续 |
| 共识 `imported_parents`（非 gov5 profile） | 视图切换时 imported_blocks 被清空，parent 边却留下 | 约 80 B/块 | — | **无上限**（bug） | **修复**：与 imported_blocks 一起清空 |
| 手机桥 `attestation_builders` | 没达到阈值的块永远不移除 | 约 150 B + 96 B/签名 | — | **无上限**（启用手机 hub 时） | **修复**：按登记顺序 FIFO，上限为 `max_tracked`（1000） |
| executed_fields、原生头登记表、sender 缓存、FrameStore 等 | 都有上限 | — | 合计数十 MB | 已有上限 | — |

结论：稳态增长有三个主因：
- `committed_blocks` 环：到 1 万块前一直线性增长，是唯一会持续涨到约 1 GB 的 n42 结构。
- RocksDB memtable：几百块内就涨满，之后在约 0.5–0.8 GB 之间来回波动。
- cross-block 缓存：随不同账户和槽的数量增长。

QMDB 的各项预算和 served 缓存会在几百到一千块内填满。这解释了早期约 1.1 MB/块的斜率，以及之后降到约 0.4 MB/块（后段主要剩 committed 环继续涨）。重同步峰值接近原值，因为这些结构随写入的块数填充，与运行时长无关。

## 4. 实施细节

- **`orchestrator/state_mgmt.rs`**
  - 新增 `trim_committed_blocks(ring, max_blocks, max_bytes)`。块数超过上限，或者字节超过预算且块数大于 64（`MIN_COMMITTED_BLOCKS`）时，从最旧的一端弹出。
  - 两个插入点（提交路径、同步路径）都改为调用它。
  - 字节数 = payload + 信封 + 原始执行血统，由 `CommittedBlock::retained_bytes`（`mod.rs`）计算。
  - 默认预算 256 MiB：S1 大小的块约能留 2700 块，小块仍能留满 1 万块。
  - 影响范围：legacy `SyncBlock` 服务只读这个环，在 S1 块大小下可服务的历史从 1 万块缩到约 2700 块。Profile 1 的 range 追块（`bodies_by_range/2`、`commit_qc/1`）读存储，不受影响。
- **`n42-network/src/service.rs`**：`MAX_SERVED_GOV5_BLOCK_BYTES` 从 1 GiB 降到 256 MiB。这个缓存只用来衔接“提案先到、块后到”的情况；命不中时已经回退到 `gov5_canonical_blocks` 存储读取。
- **`bin/n42-node/src/main.rs`**
  - `DefaultEngineValues` 加 `.with_cross_block_cache_size(1024)`，常量为 `N42_CROSS_BLOCK_CACHE_MIB`。
  - 注册 RocksDB memtable gauge：对 `RocksDBProviderFactory::rocksdb_provider().table_stats()` 的 `memtable_size_bytes` 求和。
- **`n42-consensus/src/protocol/{state_machine,timeout}.rs`**：非 gov5 profile 下清空 imported 证据时，同时清 `imported_parents`。
- **`n42-node/src/mobile_bridge.rs`**：新增 `attestation_builder_order: VecDeque`，长度超过 `max_tracked` 时弹出最旧项并删除对应 builder。已经 finalize 的旧项也在这里被弹出，所以队列本身同样有界。
- **N42_MEM 行**（`memory_watch.rs`）新增 `structures=` 字段，同时输出 `n42_mem_structure{name}` gauge：
  - 共识：`committed_blocks`、`committed_block_mib`；
  - 网络：`served_blocks`、`served_block_mib`；
  - QMDB：`qmdb_live_entries`、`qmdb_blocks`、`qmdb_ops_mib`、`qmdb_undo`、`qmdb_undo_mib`、`qmdb_read_views`、`qmdb_read_view_mib`，由新增的 `Gov5QmdbStateRootStore::memory_stats()` 提供；
  - RocksDB：`rocksdb_memtable_mib`。

  其他实现细节：
  - 共识和网络两个 crate 各新增一个 `mem_stats.rs`，里面是 `AtomicU64`，结构变化时写入，读取时不加锁。
  - `memory_stats()` 用 `try_lock`/`try_read`，commit 期间返回 `None`。它不会丢弃 prepared 候选，这一点与 `tree_stats` 不同。
  - 外部结构可以通过 `memory_watch::add_structure_source` 注册。

## 5. RocksDB 建议（未实施，需要 reth 补丁）

reth `RocksDBBuilder` 的默认参数：
- write_buffer_size：4 个 CF 为 128 MiB，2 个为 64 MiB；
- max_write_buffer_number = 2；
- max_write_buffer_size_to_maintain：256 MiB 或 128 MiB；
- WriteBufferManager：4 GiB。

N42 层只能设置 block cache，改这些参数需要新增一个 `patches/reth-*.patch`。建议值：
- write_buffer_size 改为 32 MiB；
- max_write_buffer_size_to_maintain 改为 0 或 32 MiB（只有并发事务冲突检测才需要它，N42 的持久化是单写者）；
- 设置 `db_write_buffer_size` = 256 MiB，作为全局上限。

预计 memtable 从约 0.5–0.8 GB 降到 ≤ 256 MiB。flush 会更频繁，但每次更小。

## 6. QMDB 活叶树：按状态规模估算

- 每条约 200 B。组成：分片 HashMap 的键 32 B + slot 8 B + `Box<[u8]>` 值，加上哈希表与分配器开销；再加上 twig 和上层节点。
- 估算：
  - 1,000 万条目 ≈ 2 GB；
  - 1 亿条目 ≈ 20 GB。

  “chain-94 大小”的状态需要先拿到它的条目数，然后按 200 B × 条目数估算。
- 不能用 cap：根的更新需要每个活键的 slot，淘汰条目会让根算不出来。
- 正确的后续方向是 QMDB 原论文的磁盘形态：
  - 值和叶子放进追加文件，内存只保留紧凑索引（键前缀 8 B + slot 5 B，约 16–24 B/条目）和 twig 根与上层节点（每 2048 个叶子约 32 B）；
  - 预计常驻内存降到原来的约 1/10，读值多一次 pread，可以用页缓存和 cross-block 缓存掩盖。
  - 本轮不做。

## 7. 预期效果（S1 负载，1 万块稳态）

| 项 | 修改前 | 修改后 |
|---|---|---|
| committed 环 | 约 1 GB（继续涨到 1 万块） | ≤ 256 MiB |
| served 缓存 | ≤ 1 GiB | ≤ 256 MiB |
| cross-block 缓存 | 最多 4 GiB | 最多 1 GiB；S1 规模约 100–150 MB |

预计 1 万块稳态 RSS 从约 3.5–4 GB 降到约 2 GB。打上第 5 节的 RocksDB 补丁后，有望再降 0.3–0.5 GB。到约 3000 块时，主要差别来自 committed 环（约 290 MB 降到 256 MiB 并不再增长）。

## 8. 测试

- `cargo clippy -p n42-node -p n42-node-bin -p n42-consensus-service -p n42-network -p n42-consensus --all-targets -- -D warnings`：通过。
- `cargo test -p n42-node -p n42-consensus-service -p n42-network -p n42-consensus`：全部通过。新增 3 个测试：
  - `committed_block_ring_is_bounded_by_bytes_after_the_newest_blocks`
  - `native_timeout_clears_parent_metadata_with_import_evidence`
  - `unattested_builders_are_bounded_by_tracked_blocks`
- 两个 ignored bench 的结果见第 2 节。

## 9. 后续

1. 下一轮 Q 或 S 运行时观察 `structures=`：`committed_block_mib` 应停在 256 MiB；`rocksdb_memtable_mib` 应证实第 5 节的量级。
2. 做 reth RocksDB 选项补丁（第 5 节）。
3. QMDB `blocks` 索引在 rebase 后裁掉 base 以前的块（约 200 B/块，量很小）。
4. 设计 QMDB 磁盘形态（第 6 节）。
5. 普查中发现两处在停滞时会无界增长，各补一个上限：`pending_staking_scans`、`pending_sidecar_diffs`/`missing_sidecar_diffs`。
