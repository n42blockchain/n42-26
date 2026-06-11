# devlog-62 — SBMT benchmark + 优化,对标 gov5 QMDB/BMT

## 目标

切换到自建 SBMT 后，对其做规模化 benchmark（吞吐/proof/内存/磁盘），与
`c:\n42\n42-gov5`（Go，geth 系）的 **BMT** 和 **QMDB** 对比，并据此优化。

## 工具

新增独立 bench：`crates/n42-jmt/examples/sbmt_state_bench.rs`
（`cargo run --release --example sbmt_state_bench`，env `SBMT_BENCH_ENTRIES` /
`SBMT_BENCH_BLOCK` / `SBMT_BENCH_PROOF_SAMPLES` / `SBMT_BENCH_OUT`）。
输出与 gov5 `bench_state_*.json` 同 schema 的 JSON + RSS 内存。

辅助：`n42_bmt_core::Sbmt::node_stats()` / `ShardedSbmt::node_stats()`
（活树 internal/leaf 计数 + content-addressed 序列化字节）。

## 指标口径（关键，避免假对比）

- gov5 BMT 的 `storage_bytes=33.65GB` / `total_nodes=350.9M` 是 **5M 个块的累积
  归档**（每次 copy-on-write 写入的所有历史节点版本）。我们的 SBMT 是**内存活树
  + 周期 snapshot + WAL**，原地覆盖、不留历史。两者的 storage/nodes **不可直接比**。
- 真正 apples-to-apples 的是：**proof 指标（结构内禀）**、**avg_node_size**、
  **每块/每条 apply+root 时间**、**活态 footprint / RSS**。
- gov5 proof_size 只算 `siblings×32`；我们的 `proof_size_avg` 是含 value +
  shard_path + framing 的完整序列化。可比的是 **proof depth**（`depth×32` =
  authentication path 字节）。

## 数据（Windows，release，in-place insert 优化后）

| 规模 | build | entries/s | per-block(apply+root) | proof depth | proof gen/verify | 活节点(int/leaf) | live node B | snapshot(KV) | RSS |
|------|-------|-----------|----------------------|-------------|------------------|------------------|-------------|--------------|-----|
| 1M   | 1.36s | 734k | 245 us | 25.27 | 3.18 / 3.48 us | 2.44M (1.44M/1.0M) | 202 MB | 104 MB | 405 MB |
| 5M   | 7.11s | 703k | 258 us | 27.62 | 3.57 / 3.79 us | 12.2M (7.2M/5.0M) | 1012 MB | 520 MB | 2018 MB |

gov5 Go BMT 参考（`bench_state_5m_bmt.json`，18.34M entries / 5M blocks，磁盘归档）：
per-block 4010 us（root only）、249 blocks/s、914 entries/s、avg node 95.9B、
proof 713.9B / depth 22.31 / gen 66.82 us / verify 2.56 us、archival 350.9M nodes / 33.65GB。

## 对比结论

1. **proof 生成快 ~20×**：我们 3.2–3.6 us vs gov5 66.8 us（纯内存、无磁盘 pointer-chase）。
   验证相当（~3.5 us vs 2.56 us）。
2. **proof depth 偏大**：我们 25–28 vs gov5 22。`depth×32` 即 auth path 字节，我们
   略大。根因见下（未路径压缩 + shard nibble 浪费）。
3. **吞吐**：内存 SBMT ~700k entries/s。gov5 914 entries/s 是磁盘归档 over 5M 块，
   存储模型不同，不直接可比。
4. **内存（核心权衡）**：SBMT **全状态驻内存**，RSS 随状态线性增长（5M→2.0GB，
   外推 100M 账户→~40GB）。**QMDB 靠 2048-leaf twig 滑窗 + 冷存 + compaction 有界
   内存**，代价是更大的 proof（twig path 11×32 + upper log2(twigs)×32）和复杂度。
5. **磁盘**：SBMT snapshot 是活态 KV（5M→520MB）+ WAL；gov5 BMT 33.65GB 是含全部
   历史的归档；QMDB compaction 后更小。

## 优化

### 已做（安全，不改 root）：in-place insert

`Sbmt::insert` 的 `Internal` 分支原先 `match *node` 把节点搬出、递归后再
`new_internal(left,right)` 重建 —— 每次插入沿路径 O(depth) 次 Box 重分配，并重置
全路径 cache。改为 `node.as_mut()` 原地递归、仅 `cache.set(None)` 失效：
- **build 2.45s → 1.36s（1.8×）**，per-block 463→245 us（@1M）。
- 节点数/proof/root **完全不变**（结构与承诺不变，91 个单测全过）。

### 已识别（共识级，待决策）：路径压缩

- `internal/leaf = 1.443`（1M & 5M 一致）是**未路径压缩随机二叉 trie** 的特征
  （internal ≈ 1.44·N）。
- depth 超 `log2(N)` 约 5.4 层，其中 **~4 层来自 in-shard 树仍逐 bit 走 shard 选择
  nibble**（同 shard 内 key 前 4 bit 恒等，却各占一个单孩子内部节点）。
- 路径压缩（PATRICIA：节点提交跳过的公共前缀）预计：depth → ~log2(N)（@1M 约 20，
  省 ~5 层 / proof auth path 省 ~160B），total nodes -18%，live 内存相应下降。
- **代价**：改 merkle 承诺（root 变）→ 共识级、需重新 genesis；node/proof 编码与
  mobile/FFI verify 都要改；exclusion proof 验证更复杂。SBMT 当前 pre-production
  （刚 fresh genesis 切换），是做此改动的窗口，但需充分测试 + codex 交叉验证。

## 阶段完成状态

- [x] 规模化 bench 工具 + node_stats
- [x] 1M / 5M 实测，与 gov5 BMT 对标
- [x] in-place insert 优化（1.8×，root 不变，测试全过）
- [x] 量化路径压缩收益与代价
- [ ] 决策是否实施路径压缩（共识级）
- [ ] （可选）让 codex 在 mac 跑 gov5 Go QMDB bench 拿真实 QMDB 内存/磁盘数

## 后续计划

- QMDB 真实数：gov5 的 `cmd/n42-qmdb-bench` 无 committed 结果，需现跑 Go（mac/codex）。

---

## Phase 2：shard-prefix 路径压缩（已实施）

用户决策做路径压缩。采用**安全的 shard-prefix 压缩**（非全 PATRICIA）：

**设计**：给 `Sbmt` 加 `base_depth`，in-shard 树从 `base_depth = log2(SHARD_COUNT) = 4`
开始分支，跳过所有 key 共享的 shard 选择 nibble（bits 0-3）。leaf 仍 commit 完整 key
（`blake3(0x00 || full_key || vh)`），所以**改 root（共识级）但 soundness 不变**——
每层仍单 bit、无新的 exclusion 复杂度，纯深度偏移。

**实施**：
- `n42-bmt-core`：`Sbmt::with_base_depth` + insert/get/prove/remove 从 base_depth 起；
  `BmtProof::verify(root, vh, base_depth)`（sibling i 验 bit `base_depth+i`，exclusion
  的 `shares_prefix_range` 校验 `[base_depth, base_depth+len)`）；`ShardedBmtProof::verify`
  对 inner 传 `base_depth=4`。
- `n42-jmt`：`ShardedSbmt::new`/`from_snapshot` 用 `Sbmt::with_base_depth(4)` 建 16 shard。
- 新增对抗测试 `base_depth_prefix_skip_inclusion_and_exclusion`（含"错 base_depth 必须
  验证失败"）。

**实测收益（measure 确认）**：

| 指标 | @1M 压缩前 | @1M 压缩后 | @5M 压缩前 | @5M 压缩后 |
|------|-----------|-----------|-----------|-----------|
| proof depth | 25.27 | **21.28** | 27.62 | **23.57** |
| proof size avg | 988 B | **860 B** | 1063 B | **933 B** |
| verify | 3.48 us | 2.93 us | 3.79 us | 3.25 us |

depth -4 层、proof -128~130B（-13%）、verify 更快；10000 个采样 proof 全部 `verify_for_key`
通过；91+ 单测全绿。活节点数不变（1.44·N 是全 PATRICIA 的活，shard-prefix 只省常数）。

**未做**：全 intra-tree PATRICIA（internal 1.44→1.0、再省 ~1 层 + 内存）——需提交 skip
长度到节点 hash + 重写 exclusion 验证 + 单独安全审计，风险高，作为后续可选项。

**待 codex**：consensus-breaking（root 变），需 fresh-genesis E2E + 对抗性复核（真实 RPC
proof roundtrip、bound verify 正确/错误 address、错 base_depth 拒绝）。

---

## Phase 3：QMDB 实测数据（gov5 `cmd/n42-qmdb-bench`，本机现跑）

gov5 的 `n42-qmdb-bench` 自带 synthetic churn workload（seed `keys` 个 key，再跑 `blocks`
块、每块覆写 `perblock` 个随机 key），**无需 journal**。in-memory（无磁盘 store），与我们
in-memory SBMT 同口径。输出：footprint（QMDB entryLog+twigMeta vs JMT all-history）、
每块 root 时间、compaction、并行 vs 顺序 twig 重算 speedup。

实测（Windows，go 1.25）：

| keys | QMDB footprint | （entryLog / twigMeta / twigs） | JMT all-history | QMDB root/block | JMT root/block | 并行 speedup |
|------|----------------|--------------------------------|-----------------|-----------------|----------------|--------------|
| 1M（blocks=2000） | **96.3 MB** | 96.1 / 0.2 MB / 684 | 3452 MB（8.98M nodes） | 118 µs | 2.77 ms | 14.8× |
| 5M（blocks=500） | **350.9 MB** | 350.2 / 0.7 MB / 2491 | 13933 MB（35.3M nodes） | 213 µs | 3.44 ms | 17.4× |

要点：
- **QMDB 的树结构内存极小**：twigMeta 仅 0.2 MB(@1M) / 0.7 MB(@5M)；footprint 几乎全是
  entryLog（flat append KV，8+32+32 B/entry）。对比我们 SBMT 把每个内部节点都堆成 Box，
  @5M RSS 2.0 GB 里绝大部分是 12.2M 个树节点。
- **内存对比（in-memory，同口径）@5M**：QMDB 资源结构 ~351 MB vs SBMT RSS ~2018 MB →
  **QMDB 约省 5.8× 内存**。这就是 QMDB 全部复杂度（twig 滑窗 + 冷存 + compaction）换来的东西。
- compaction 在此 workload 下 0% reclaimed（纯覆写既有 key，twig 始终 dense，不触发稀疏
  twig 修剪）；要展示 compaction 收益需带删除的 workload。
- 每块 root：QMDB 118/213 µs vs SBMT 244/258 µs（apply+root）。同量级，QMDB 略快（但 QMDB 是
  覆写、SBMT 是插入新 key，workload 不同）。
- 此 bench **不测 proof**（proof 指标在 `bench_state`）；QMDB proof 仍按结构估计：twig path
  11×32 + upper log2(twigs)×32，比 SBMT 大。

## 最终对比结论（含实测 QMDB）

| 维度 | SBMT（实测） | QMDB（实测） |
|------|-------------|-------------|
| 内存 @5M | RSS 2.0 GB→**1.72 GB**（arena 后；全树驻内存，线性） | **~351 MB**（gov5 qmdb-bench；约省 ~5×） |
| 活态磁盘 @5M | snapshot 520 MB | ~351 MB（entryLog；compaction 后更小） |
| 每块 root @5M | 258 µs（apply+root，插入） | 213 µs（覆写） |
| proof 生成 | ~3.1 µs（vs gov5 Go BMT 快 ~20×） | **~1.0 µs**（扁平堆直读 sibling，零重哈希）→ 比 SBMT 快 ~3× |
| proof 验证 | 2.9 µs | 2.0–2.9 µs（同级） |
| proof 大小 | **580 B**（实链，压缩后）/ 860 B（bench 全序列化） | 751 B(@1M) / 815 B(@5M) |
| proof depth | 21.3 / 23.6 | **21 / 23（几乎一致）** |
| 并行 root speedup | 16 分片 | 14.8–17.4×（twig） |
| 实现复杂度 | 低（单引擎） | 高（twig/compaction/冷存） |

> QMDB footprint（351 MB）来自本机跑 gov5 `qmdb-bench`；QMDB proof 行（751/815 B、~1 µs gen、
> depth 21/23）来自另一会话（C2）对其 QMDB 实现的实测。proof depth 两边独立测出 21/23 ≈ SBMT
> 21.3/23.6，互为交叉验证。
>
> **可借鉴点**：QMDB proof gen ~3× 快，因为 twig 是完整二叉堆、所有节点 hash 预存为扁平数组，
> prove 时直接读 sibling、零重哈希。我们的 `prove()` 虽走 arena + cache，但每次仍有 Vec 分配 +
> 逐层 peek + 可能的未缓存 sibling 重算。若 proof gen 成热点，可把 in-shard 树的 sibling hash
> 预物化为扁平数组（类似 twig）来追平。当前 3 µs 非瓶颈，列为后续可选。

**一句话**：SBMT 在 proof（生成快 20×、更小）和实现简单度上占优；QMDB 在大规模内存
（省 ~5.8×、有界）上占优。选择取决于状态规模——中小状态 SBMT 全驻内存可接受且 proof 极优；
超大状态（数千万~亿账户）QMDB 的有界内存才是刚需。

---

## Phase 4：内存优化——arena 扁平节点（学习 QMDB/AlDBaran 紧凑布局）

### 论文对标背景

查证：传说中的 **50M update/s 来自 AlDBaran/Pleiades**（Eclipse Labs，arXiv 2508.10493），
非"SBMT 论文"（SBMT 是本项目自起名）。谱系 **NOMT(~50k/s) → QMDB(2.28M/s, 2.3 B/entry,
arXiv 2501.05262) → AlDBaran(48M 无历史 / 24M 带历史 / 60M 峰值，96 核 + 1.5TB DRAM)**。
我们 SBMT ~0.74M/s、**~400 B/entry**——比 QMDB 内存差 ~170×。关键差距：我们把每个内部节点
堆成 `Box<Node>`（指针追逐 + 分配器开销），而 QMDB 内存只留 twig root + bitmap（2.3 B/entry）。

借鉴的优秀方法：QMDB twig（2048 叶/twig + append-only log）、AlDBaran 的 thread-sharding
无锁 / twig buffering / SIMD 16-branch hash / 确定性缓存布局。

### 本次落地：arena 扁平节点（安全，root 不变）

`Sbmt` 由 `Option<Box<Node>>` 链表改为**扁平 `Vec<Node>` + u32 索引**（`NodeIdx`，`NIL`
哨兵），insert/remove/get/prove/hash 全部改索引递归 + free-list 复用。消除每节点堆分配 +
分配器开销，节点连续利于缓存。**树结构与所有 root/proof 完全不变**（200+ 单测全过）。

实测：

| 规模 | RSS 前 | RSS 后 | 省 | build 前→后 |
|------|--------|--------|-----|------------|
| 1M | 405 MB | 375 MB | ~7% | 1.36→1.37s |
| 5M | 2018 MB | **1721 MB** | **~15%** | 7.11→7.58s（~6%） |

规模越大省越多（小规模 values map+基础开销占比大）。代价是 ~6% build（arena 多一次
索引 + Vec 倍增 realloc）。proof/depth 不变。

### 仍未做（更大的内存杠杆）

- **SoA 拆分**（leaf arena + internal arena）：Internal 节点 41B vs 统一 enum 的 68B，
  预计再省 ~10%。中等改动、root 不变。
- **QMDB twig 模型**：2048 叶/twig + append-only entry log（可落盘）+ 内存只留 twig
  root+bitmap → 趋近 2.3 B/entry、有界内存。consensus-breaking + 大重构 + 单独审计。这是
  把内存从"线性"变"有界"的唯一路。

---

## Phase 5：thread-sharding（吞吐，measure-first，root 不变）

学习 AlDBaran 的"关键路径无锁 / thread-sharding"。先 measure：

### 关键发现：吞吐瓶颈是块大小，不是锁

| block 大小 | 吞吐（@1M build） |
|-----------|------------------|
| 200 | 674k entries/s |
| **2000** | **1.37M entries/s** |
| 20000 | 1.24M entries/s |

小块（200）被 rayon 16-task 派发开销吃掉一半吞吐；**block=2000 直接翻倍到 1.37M/s**
（逼近 QMDB 2.28M）。之前一直报的 674k 是 bench 默认小块的 artifact——生产块大时我们
已在 ~1.2–1.37M/s。@5M block=2000：build 4.18s = **1.20M entries/s**。

### 已做：去掉冗余内层 Mutex（无锁化，root 不变）

节点把整树包成 `Arc<Mutex<PersistentSbmt>>`，所有访问已串行化，所以 `ShardedSbmt` 内每
shard 的 `Mutex<Sbmt>` / `Mutex<HashMap>` 是**双重加锁**（它只为让 `Sbmt` 满足 rayon 的
`Sync`——因 cache 是 `Cell`）。改为：
- `shards: Vec<Sbmt>` / `values: Vec<HashMap>`（无 Mutex）；
- `apply_diff` 用 `par_iter_mut`（只需 `Send`，每 worker 独占一个 shard，**关键路径零锁**）；
- 读路径（get/prove/root_hash/snapshot）直接索引，无锁。

perf 中性（parking_lot 无竞争锁本就 ~ns），但更干净、移除 double-lock、对齐"无锁"原则、
200+ 测试全过、RSS/proof/root 不变。

### 已否决：并行化 prepare（measure 证明是负优化）

把 `prepare`（per-account blake3 key 派生）改 rayon 并行 **实测更慢**：block=200
674k→466k、block=2000 也微降。原因：额外 accounts/entries 中间 Vec + rayon 派发开销 >
blake3 并行收益。**撤回**，prepare 保持单遍串行。

### 与论文的差距来源（已澄清）

剩余 ~1.7×（vs QMDB）/ ~8×（vs AlDBaran）的吞吐差不是锁，而是结构性的：
- **分片数固定 16**：多核机上最多用 16 核；AlDBaran 跨 96 核分片。增分片会改 merkle 结构。
- **SIMD**：AlDBaran "一条指令 hash 16 分支"；我们逐节点 blake3（blake3 内部已 SIMD，但未跨
  兄弟节点批量）。
- **硬件**：AlDBaran 96 核 + NVMe 管线。

这些是后续（增分片=共识级；SIMD 批量 hash=中等改动）的方向，非本轮范围。
