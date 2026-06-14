# devlog-58 — JMT vs QMDB/NOMT 差距分析 + P0 原型(全内存 + 后台快照)

## 背景

针对 `n42-jmt` 与当前 SOTA authenticated storage(QMDB、NOMT、Firewood)做差距分析,
并落地 P0 两项验证:① 量化现状 disk 路径成本 ② 实现"internal 节点全内存 + 后台快照"
原型(Firewood 路线),验证"消除节点 SSD IO"的提升幅度。

参考:`docs/starknet-poseidon-vs-aptos-jmt-comparison.md`(早期状态树对比)。
外部:QMDB(arXiv 2501.05262)、NOMT(rob.tech)、EIP-7864(以太坊统一 binary tree,默认 Blake3)。

## 1. 差距分析(设计层面)

| 维度 | n42-jmt 现状 | QMDB / NOMT(SOTA) | 差距 |
|------|--------------|--------------------|------|
| 树/库 | 包 `jmt 0.12`,16 叉 nibble,Blake3 | QMDB 自研 twig;NOMT 自研 binary | 受第三方库架构锁定 |
| Merkle 的 IO | 节点存 MDBX,读写沿路径多次节点访问 → Merkleization 自带 SSD read+write | Merkleization 零 SSD IO(内存 twig root) | 🔴 最大 |
| 读 leaf | LRU miss 时沿树 pointer-chasing | indexer→offset→1 次 SSD IO | 🔴 严重 |
| 节点剪枝 | 无节点 GC(`store.rs` 只剪 value 版本),节点永久堆积 | twig 生命周期 + 递归剪 upper nodes | 🔴 严重(磁盘无限涨) |
| 内存/entry | Node Borsh ~100-200B + LRU | 2.3 字节/entry | 🟠 中 |
| 并行 | 16 shard rayon,prepare→compute→write 阶段 barrier | Prefetch-Update-Commit 三级流水线 | 🟠 中 |
| proof | ~1KB,含 16 shard root 冗余 512B,仅 inclusion | inclusion+exclusion,无冗余 | 🟠 中 |

**核心判断**:差距不在哈希(Blake3 已对齐 EIP-7864 方向),也不在树结构,而在
**存储/Merkle 模型** —— jmt 库把每个 internal 节点持久化到 KV 库、读写 pointer-chase,
正是 sov-db 在 NOMT benchmark 里 `commit_and_prove` 垫底(16s)的同款病根。

**关键 nuance**:QMDB 为 2800 亿 entry / 单机 228 万 ups 设计;n42 规模(100-500 IDC、
8s slot)大概率小得多,更现实的对标是 **Firewood 路线**(全内存 Merkleization + 异步快照,
内存态 8M ups),工程量比 QMDB 自研 append-only log + indexer 小一个量级。

## 2. 现状勘察(实现事实)

- `ShardedJmt<S: TreeStore = MemTreeStore>` 泛型,`new()` = 内存,`open_disk()` = MDBX。
- `DiskTreeStore`(`disk_store.rs`):每 shard 3 个 MDBX named db(nodes/values/meta),
  `get_node_option` cache miss → 每次新开 RO txn 读节点;`write_batch_in_txn` 把节点写 MDBX。
- **关键发现**:`benches/jmt_bench.rs` 原本全部用 `ShardedJmt::new()`(纯内存),
  **从未 bench 过 disk 路径** —— 既往 JMT 数字其实是内存上界,生产 disk 成本无基线。
- snapshot.rs 已有 value-level 全量快照(`snapshot()` / `from_snapshot()` /
  `save_snapshot` / `load_snapshot`),P0-2 直接复用。

## 3. P0-1:Disk vs Mem 对拍基线

`benches/jmt_bench.rs` 新增 `apply_diff_disk`(MDBX `apply_diff_atomic`)与
`apply_diff_persistent`(全内存)两组,与内存 `apply_diff` 同规模对照。

实测(criterion 均值,本机 Windows,Opus dev build;mem 每次新建空树,disk/persistent
复用实例做增量更新——后者更贴近真实增量场景):

| accounts | 内存 | Persistent | Disk(MDBX) | disk/mem |
|---------:|-----:|-----------:|-----------:|---------:|
| 100 | 203 µs | 273 µs | 6,663 µs | **32.8×** |
| 1,000 | 361 µs | 572 µs | 15,378 µs | **42.6×** |
| 10,000 | 368 µs | 570 µs | 17,818 µs | **48.5×** |

**结论**:
1. MDBX 节点持久化路径比全内存慢 **33–48×**,且随规模放大(印证 QMDB 的 `O((log N)²)` 节点 IO)。
2. 全内存路径(Persistent)仅比纯内存慢 1.3–1.6×,与内存同数量级 → **"消除节点 SSD IO"是真正的数量级杠杆**。

## 4. P0-2:PersistentJmt 原型(Firewood 路线)

新增 `crates/n42-jmt/src/persistent.rs`:

- 内部持有 `ShardedJmt<MemTreeStore>`:所有 internal/leaf 节点常驻 RAM,
  **apply 路径零节点 SSD IO**。
- 持久化:每 `snapshot_interval` 版本触发 value-level 快照;`flush_background()` 把
  序列化+zstd 压缩+落盘放到 spawned 线程,apply 热路径不阻塞 IO(同步只做内存 `snapshot()` 这一廉价 value dump)。
- 生命周期:`open()` 启动从快照恢复;`flush()`/`flush_background()`/`join_pending()`;
  `Drop` 时 best-effort join 在途快照。

### 设计决策

- **为何 value-level 快照而非节点持久化**:jmt 库 Node/NodeKey 不支持 serde;且全量
  value dump + 重建,天然规避了"节点随机写/pointer-chase"。重建成本由 `from_snapshot`
  的 16 shard 并行 apply 承担,启动一次性付出。
- **为何后台线程**:把 zstd 压缩 + 文件写移出 apply,使 slot 内 state root 计算保持内存级。
- **为何不做增量 WAL(P0 范围外)**:原型目标是验证性能上界,不是生产持久化。

### 局限(已标注,列入后续)

- **崩溃恢复粒度 = 快照间隔**:最后一次快照之后的版本在崩溃时丢失。
  生产需补 **StateDiff WAL**(快照间重放)闭合该 gap。
- 快照是全量 value dump,大状态下落盘成本随 entry 数线性增长(后续可做增量/分片快照)。

## 5. 验证

- `cargo test -p n42-jmt`:**68 passed**(含 persistent 模块 4 个新测试:同步/后台 flush
  往返一致、间隔自动快照、空树打开)。
- `cargo clippy -p n42-jmt --all-targets -- -D warnings`:干净。
- 顺带确认:reth main 合并后 **n42-jmt 本机 Windows 可编译可测**(此前 memory 记录的
  "Windows 编不过"在本 crate 已解除)。

## 6. 阶段状态与后续(P1 backlog)

P0 完成:✅ 差距分析 ✅ disk/mem 基线 ✅ PersistentJmt 原型 + 测试。

后续优先级(沿用差距分析):
- **P1**:节点剪枝 GC —— internal 节点加 stale index 按 version 回收(防 disk 无限膨胀)。
- **P1**:proof 去冗余(16 shard root → 对 shard-root 再建小 Merkle,proof 带 log₂16=4 个兄弟)+ 补 exclusion proof。
- **P1**:PersistentJmt 补 StateDiff WAL,闭合崩溃恢复 gap,转生产可用。
- **P2**:commit 流水线化(Prefetch-Update-Commit 跨 block 重叠)。
- **P2**:存储引擎评估(MDBX vs 自研 append-only log)。
- **P3**(战略级):评估弃用 jmt 0.12、自研 binary 对齐 EIP-7864。
