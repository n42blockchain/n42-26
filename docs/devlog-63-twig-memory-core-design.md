# devlog-63 — twig 连续内存核心设计（内存即吞吐，AlDBaran 核心）

## 核心洞察：内存布局就是吞吐引擎

前几轮 benchmark 把"内存"和"吞吐"当两条路，是错的。数据已证明它们是同一件事：

- **缓存局部性 ~2×**（实测：5M 树 830MB miss L3 = 0.98M upd/s vs 100k 树命中 L3 = 1.91M）
  —— 这是**内存布局**问题。
- **结构性 ~3×**（命中缓存后仍落后）—— twig append + 连续块 vs 我们"改树 + 改 HashMap"双写。
- 微优化（SIMD 仅节点 ≤7%、SoA 11%RSS、DFS 重排略慢、并行 prepare 更慢）全在边缘，因为
  **它们没碰连续内存这个核心**。

**AlDBaran/QMDB 的核心**：把状态摆成**连续、缓存驻留、DRAM-only 的 twig 块** ——
一次同时拿下：① 内存有界（只留 twig root + bitmap，~2.3 B/entry）② 吞吐（twig 在 L2、
紧循环、SIMD 友好）。50M/s 不是"吞吐技巧"，是"内存布局"的副产品。

→ 结论：要 substantially 提升，唯一真路是**采用 twig 连续内存核心**，内存与吞吐一起拿。
这是 consensus-breaking 大改（root 结构变 → fresh genesis），但它是唯一同时解决两个大头的。

## 走 AlDBaran 路线（全 DRAM），不走 QMDB 路线（SSD 下盘）

AlDBaran 和 QMDB 的内存路线本质不同：

| 维度 | QMDB 路线 | **AlDBaran/Pleiades 路线（选这条）** |
|------|----------|-----------------------------------|
| 数据驻留 | Merkleization 在 RAM；entry 下盘 SSD | **全部在 DRAM**，no-disk on hot path |
| 冷访问 | 1 SSD read/次（磁盘延迟绑定） | **无**（全 RAM）→ 快非常多 |
| RAM/entry | ~2.3 B（entry 下盘） | ~100 B（2.3 B Merkleization + entry 全驻 DRAM） |
| 磁盘 | entry log + 冷数据（SSD 必需） | 仅 commit 异步快照到 NVMe |
| 吞吐 | 2.28M（disk-bound） | **48M**（all-DRAM + SIMD + thread-shard） |
| 复杂度 | 高（冷存分层 + cold-fault + index 命中） | **中**（twig 连续 + 无锁 + SIMD，**无冷存分层**） |

**为什么 AlDBaran**：① 全 RAM = 纳秒级，QMDB 冷读走 SSD = 微秒级（这就是 2.28M vs 48M 的根因）；
② 无冷存分层 = 少一大堆复杂度/bug 面；③ 我们当前 SBMT **本来就 all-in-DRAM**，走 AlDBaran 是
顺着已有属性优化，走 QMDB 反而要倒退加下盘机制；④ IDC 节点 RAM 充足，状态塞得进 DRAM。

→ 本文档下面的"twig 核心"按 **AlDBaran 全 DRAM 变体**理解：twig 连续布局 + 全驻 DRAM + SIMD +
无锁分片，**砍掉 QMDB 的 SSD entry-log / 冷存 / cold-fault**。快照仍走我们已有的
`PersistentSbmt` 周期快照（只在 commit 落盘，不在读路径）。

## 现状 vs 目标

| | 当前 SBMT（pointer/arena 二叉树 + values HashMap） | twig 核心目标 |
|---|---|---|
| 内存/entry | ~400 B（全状态驻 RAM，线性） | ~2.3 B（twig root+bitmap 驻 RAM，余下盘） |
| @5M RAM | 1.7 GB | ~数十 MB Merkleization +（index/log 视设计驻 RAM 或下盘） |
| 缓存 | 深度遍历乱跳，miss | twig 连续块 L2 驻留 |
| 吞吐（9950X 16C） | 1.3M upd/s | 目标 ~8M（AlDBaran 单核 0.5M × 16） |
| proof | 580 B / depth 21（压缩后） | twig path 11×32 + upper log2(twigs)（略大） |

## twig 核心设计（适配 n42-SBMT）

沿用 QMDB 结构（gov5 `lib/qmdb` + C2 Rust 实现已验证）：

1. **Leaf 层**：`hash(0x01 || keyHash || value)`，slot-indexed（append-only）。
2. **Twig**：2048 叶/twig = 11 层完全二叉堆，**连续数组**（~64KB，L2 友好）。**全驻 DRAM**
   （叶 blob 不下盘——这是 AlDBaran 与 QMDB 的关键差异），twig root + activeBits 也在 RAM。
3. **Upper 树**：twig roots 上的二叉 merkle，增量折叠，只更新脏 twig 路径。
4. **Entry 存储**：append-only（key+value）**驻 DRAM**（不是 QMDB 的 SSD entry-log）。
   O(1) 追加。只在周期快照时整体落盘（走现有 `PersistentSbmt`）。
5. **Index**：key→slot，**驻 DRAM**（AlDBaran 不下放盘，避免 cold-fault）。16 分片各一套，无锁并行。
6. **Compaction**：稀疏 twig 修剪 + 重排，回收 obsolete slot（仍需要，控制 DRAM 增长）。
7. **Proof**：membership = twig path(11×32) + upper path(log2 twigs×32) + value；exclusion
   用相邻 entry 区间。与当前 ShardedBmtProof 不同 → mobile/FFI 验证要改。

## 共识 / 兼容影响

- **root 结构变** → consensus-breaking → **fresh genesis**（SBMT 已经是 fresh genesis 切换过，
  再切一次可接受，pre-production）。
- **on-disk 格式变**：snapshot/WAL → entry log + twig tables（gov5 用 4 张 MDBX 表）。
- **proof 格式变** → 重做 mobile/FFI 验证 + 像 #11 那样重新做 key 绑定安全审计。

## 关键决策：Rust 自建（gov5 Go 为参考蓝本）

**更正**：C2 / gov5 是 **Golang**（`C:\n42\n42-gov5\lib\qmdb`），不是 Rust。所以：

- **不能复用 C2 的代码**（语言不通，Go 进不了 Rust 的 n42-26）。
- **Go 吞吐天生不如 Rust + 有 GC** → 同样的 QMDB/AlDBaran 设计**用 Rust 实现会比 C2 的
  7.99M 更快**（无 GC、更紧的内存布局、Rust SIMD intrinsics）。C2 的 7.99M（Go）是
  **下限参考**，不是目标上限。
- **gov5 Go QMDB = 临时/参考路线；n42-26 Rust = 真目标。** 不偏离 AlDBaran 路线 =
  **在 Rust 里自建 twig 核心**。

**唯一方案：Rust 自建**，三个输入：
1. **设计蓝本**：gov5 `lib/qmdb`（Go 源，本机可读）—— twig(2048 叶 11 层) / entry log /
   upper 树 / MDBX 表 / proof / compaction 的成熟设计，直接照搬结构。
2. **优化提示**：C2 的实测发现（AVX-512 16-way、thread-sharding 无锁、twig buffering、
   确定性缓存布局）—— 经 bridge 问它具体怎么做的，少踩坑。
3. **我们已有的资产**：`n42-bmt-core`（zero-dep，可放 twig 的 mobile 验证侧）、#11 key 绑定
   安全修复、fresh-genesis E2E 流程、16 分片外壳、reth-libmdbx 持久化。

→ **下一步行动**：① 精读 gov5 `lib/qmdb` Go 源定结构；② 经 bridge 问 C2 它的 AVX-512 /
compaction 具体实现细节当提示；③ 出 Rust twig engine 的 crate 设计 + n42 `StateDiff`/
`apply_diff` 接口 spec。目标：Rust 实现 ≥ C2 的 7.99M（Go），内存趋近 ~2.3 B/entry。

## 分阶段计划（若走 A，移植 + 适配）

1. **接口适配层**：QMDB engine ← n42 `StateDiff`/`apply_diff`/`account_key` 接入；保持
   `ShardedSbmt`-like 外壳，内部换 twig。
2. **持久化**：entry log + twig tables（复用 reth-libmdbx）替换当前 snapshot+WAL；fresh genesis。
3. **proof + mobile/FFI**：twig+upper proof 打包 + 验证；key 绑定（#11 同款）安全审计。
4. **节点接线**：`orchestrator` 的 `Arc<Mutex<PersistentSbmt>>` → 新引擎；RPC `n42_jmtProof`。
5. **验证**（对齐 C2 标准）：全 suite + race + commitment 全绿、root 在新方案内字节恒等、
   崩溃恢复 E2E、内存/吞吐 benchmark（目标 ~数十 MB RAM @5M + 多 M upd/s）。

## 风险 / 工作量

- 大（多周级），consensus + 安全 + 持久化 + mobile 全链路都动。
- 风险点：proof 安全（exclusion + key 绑定，#1/#11 类）、崩溃恢复正确性、mobile zero-dep 约束。
- 缓解：复用 C2 已验证 engine（方案 A）+ 沿用我们已有的 #11 绑定 + fresh-genesis E2E 流程 +
  codex/C2 对抗性复核。

## 建议

1. **精读 gov5 `lib/qmdb` Go 源**（本机可读），把 twig/entry-log/upper/proof/compaction 的
   结构定下来，作为 Rust 移植蓝本。
2. **经 bridge 问 C2 优化细节**（AVX-512 16-way 怎么接的、compaction 触发与重排、twig
   buffering、确定性缓存布局）当提示，少踩坑。
3. **出 Rust twig engine 的 crate 设计 + n42 `StateDiff`/`apply_diff` 接口 spec**，目标
   Rust ≥ C2 的 7.99M（Go）、内存趋近 ~2.3 B/entry。
4. 评估完工作量/风险后再决定启动节奏。在此之前不动现有 SBMT（PR #1 已是稳定的
   root-invariant 基线，可随时回退）。
