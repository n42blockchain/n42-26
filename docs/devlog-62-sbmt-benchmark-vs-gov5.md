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
