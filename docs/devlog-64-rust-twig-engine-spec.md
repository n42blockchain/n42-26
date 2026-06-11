# devlog-64 — Rust 全-DRAM twig engine 实现 spec

承接 devlog-63（走 AlDBaran 全 DRAM 路线）。本文是把 gov5 Go QMDB（`lib/qmdb`）移植为
Rust 全-DRAM twig 引擎的实现规格：保留 twig 连续布局 + SIMD + 无锁分片，**砍掉 QMDB 的
SSD 下盘/冷存/三级驱逐**，全驻 DRAM + 周期快照。

## 0. 从 gov5 蓝本：保留 / 丢弃

| gov5 Go QMDB | Rust 全-DRAM 版 |
|---|---|
| twig = 2048 叶 / 11 层 / 4096 节点堆，`nodes[1]`=root，children(j)=(2j,2j+1) | **保留** |
| leaf = blake3(0x01‖keyHash‖value)，internal = blake3(left‖right) | **保留**（value 直接进 leaf hash = 统一 KV+Merkle，砍掉独立 values HashMap） |
| upper 树（twig roots 上的二叉 merkle，增量折叠） | **保留** |
| append-only slot + key→slot index | **保留**（见 §2 决策） |
| SIMD：hashNodes8/16（AVX2/AVX-512）、hashLeaves16、batch folding | **保留**（Rust intrinsics + 运行时检测 + 标量 fallback） |
| ShardedTree（按 keyHash[0]>>shift 分 S 片，无锁 worker pool） | **保留**，对齐我们 16 分片 + 深度-4 shard 树 |
| **3 级驱逐**（entry 滑窗 / twig 叶驱逐 / index 下放 MDBX） | **丢弃**（全 DRAM，无冷读） |
| **ColdReader / SSD entry-log** | **丢弃**（entries 驻 DRAM 的 Vec arena） |
| 持久化：4 张 MDBX 表 + sparse leaf blob | **简化**：周期快照（entries + twig roots + bitmaps）走现有 PersistentSbmt 风格；不在读路径落盘 |
| undo records / 历史 proof（Hyades） | **暂不做**（非共识必需；后续可选） |
| compaction | **保留**（控 DRAM 增长 + 回收 obsolete slot；见 §2 共识注意） |

## 1. 精确常量 / 哈希（直接照搬 gov5）

- `TWIG_SIZE = 2048`，`TWIG_HEIGHT = 11`，twig 节点堆 `[Hash; 4096]`，root=`nodes[1]`，
  叶 `[2048, 4096)`，内部 `[1, 2048)`。
- `HASH = [u8; 32]`，blake3-256。`NULL_HASH = [0u8;32]`。
- `hash_leaf(keyHash, value) = blake3(0x01 ‖ keyHash ‖ value)`。
- `hash_node(l, r) = blake3(l ‖ r)`（无前缀，64 字节单块）。
- `null_level[h]`：全空子树根，`null_level[0]=NULL_HASH`，`null_level[h]=hash_node(null_level[h-1],null_level[h-1])`，`[Hash;12]`。
- `null_twig_root = null_level[11]`。
- upper：`up_cap = next_pow2(num_twigs)`，`upper[1]`=世界根，twig roots 在 `[up_cap, up_cap+num_twigs)`，余 `NULL_HASH` 填充。
- 空树根 = `NULL_HASH`。

## 2. 关键设计决策：append-slot（共识确定性）

gov5 用 **append-only slot**：每次 Set 分配 `slot=next_slot++`，旧 key 的旧 slot 置死。
→ **root 依赖 append 历史（slot 位置），非仅 live key 集**；compaction 重排 slot → **改 root**。

**对共识的含义（必须钉死的不变量）**：
1. **块内更新顺序必须 canonical**（所有节点一致）——建议 **按 keyHash 排序**应用一个块的所有
   (key, value)，与节点无关、确定性。
2. **compaction 必须确定性**（同触发阈值、同 keyHash 排序重排）——所有节点在同一块边界同样
   compact，否则 root 分叉。
3. snapshot/恢复必须保存 slot 布局（next_slot + entries），否则恢复后 root 不一致。

→ gov5/C2 已验证此模型在共识下可行（确定性 = 排序 + 同步 compaction）。**采用 append 模型**
（dense、无 sparse 浪费、与 C2 优化路径一致），把上述 3 条作为头号正确性不变量 + 对抗测试目标。

（备选：key-addressed 顺序无关模型，免 compaction，但 sparse 2048-叶 twig 浪费空间且偏离
gov5/C2 蓝本——不选。）

## 3. Crate 结构（保 mobile zero-dep）

- **`n42-twig-core`**（新 crate，zero-dep：仅 blake3 + serde + thiserror）：
  - `Twig`（[Hash;4096] 堆 + activeBits[256] + live + dirty/pruned）
  - `TwigTree`（单分片引擎：entries arena + twigs Vec + upper + key→slot index + compaction）
  - `TwigProof` / `ShardedTwigProof`（twig path 11 + upper path + shard path 4 + value）
  - `verify_for_key`（**带 #11 key+shard 绑定**）+ 无状态验证（mobile/FFI）
  - SIMD kernels（`#[cfg]` AVX-512/AVX2 + 标量 fallback，运行时检测）
- **`n42-jmt`**：`ShardedTwig`（16 分片 × `TwigTree`，对齐现有 `ShardedSbmt` 外壳 + 深度-4
  shard 树 + `apply_diff(StateDiff)->(version,root)`）；`PersistentTwig`（全 DRAM + 周期快照）。
- **`n42-mobile` / `n42-mobile-ffi`**：复用 `n42-twig-core` 验证侧，新增 `n42_verify_twig_account_proof`
  等（带 #11 绑定）。

## 4. n42 接口适配（StateDiff ↔ engine）

```
ShardedTwig::apply_diff(&mut self, diff: &StateDiff) -> (u64, B256)
  1. 把 StateDiff 展平成 ops: Vec<(keyHash, Option<value>)>
     （account_key / storage_key 复用 n42-bmt-core；code_hash 读回从统一 KV 层）
  2. 按 keyHash 排序（canonical 顺序，§2 不变量 1）
  3. 按 keyHash[0]>>4 分 16 片，每片无锁 par_iter_mut 应用（Set/Delete）
  4. 每片 batch folding（SIMD）算 twig roots + upper → 分片 root
  5. 深度-4 shard 树折叠 16 分片 root → 世界根
  6. 周期边界触发确定性 compaction（§2 不变量 2）
```

read-back（prepare 的 code_hash 保留）：从 engine 的统一 KV（entry.value）读，**不再有独立
values HashMap**（省 760MB + 双 miss）。

proof：`ShardedTwig::prove(key) -> ShardedTwigProof`（twig path 11 + upper path + shard path 4 +
value）；mobile/FFI `verify_for_key`（绑定 inner.key==account_key(addr) 且 shard 正确，#11）。

## 5. 持久化（全 DRAM + 周期快照）

- 运行时全驻 DRAM：entries arena、twigs（含叶）、upper、index 都在 RAM。
- 周期 snapshot（对齐 `PersistentSbmt`，间隔 N 块）：序列化 entries（slot 顺序）+ next_slot +
  twig roots/bitmaps + version。WAL 记录块级 ops（key 排序后）保证崩溃恢复到精确 root。
- 恢复：load snapshot → replay WAL → 重算 upper → 校验世界根。**不在读路径碰盘**。

## 6. SIMD（运行时检测 + fallback）

- `hash_nodes_16`（AVX-512）/`hash_nodes_8`（AVX2）/标量：批量算同层兄弟节点。
- `hash_leaves_16`（AVX-512）：批量算 leaf hash（33+len ∈ (64,128]，覆盖账户 72B/storage 32B）。
- batch folding：块内 deferred，按层去重父节点，每叶约 1 次 hash（vs eager 11 次）。
- 运行时 `is_x86_feature_detected!("avx512f")` 选 kernel；非 x86 / 无 AVX 用标量。问 C2 要它
  AVX-512 16-way 的具体接法当参考。

## 7. 分阶段计划

1. **P1 单分片 `TwigTree`（标量）**：twig + upper + entry/slot + Set/Delete/Get/Root + proof，
   对拍 gov5 Go 同输入同 root（跨语言一致性测试），+ inclusion/exclusion proof 验证。
2. **P2 compaction + 确定性不变量**：排序应用 + 确定性 compaction + 对抗测试（乱序应用 root
   一致、compaction 前后 root 规则、跨节点确定性）。
3. **P3 16 分片 `ShardedTwig` + #11 key 绑定 proof**：apply_diff(StateDiff) 接口 + mobile/FFI。
4. **P4 SIMD**（AVX-512/AVX2 + fallback）：先标量基线测对，再加 SIMD 对拍 root 不变 + measure 提速。
5. **P5 持久化 `PersistentTwig`**（snapshot+WAL，全 DRAM）+ 崩溃恢复 E2E。
6. **P6 节点接线**：`orchestrator` 的 `Arc<Mutex<PersistentSbmt>>` → `PersistentTwig`，RPC，
   fresh genesis，多节点 E2E（root/height 一致）。

每阶段：单测 + clippy + （P3 起）codex/C2 对抗复核 + root 字节恒等对拍。

## 8. 预期收益（目标）

- 内存：~400 B/entry → ~100 B/entry 量级（统一 KV 砍 values map + twig 紧布局），全驻 DRAM。
- 缓存：twig 连续 64KB 块 L2 驻留，修掉实测的 ~2x miss。
- 吞吐：全 DRAM + SIMD + 无锁分片 → Rust 版目标 ≥ C2 Go 的 7.99M（Rust 无 GC + 紧布局）。
- proof：twig 11×32 + upper log2 + shard 4×32（比当前 SBMT 580B 略大，但仍 sub-1KB）。

## 8b. P1 完成状态（已落地）

新 crate `n42-twig-core`（zero-dep：blake3+serde+thiserror）实现单分片标量 `TwigTree`：
- twig（2048 叶 / 4096 节点堆，`nodes[1]`=root，children=2j/2j+1）+ 全量 recompute；
- upper 树（next_pow2 padding NULL_HASH，二叉折叠）；
- append-slot 模型（`set` 分配 next_slot++、更新置死旧 slot、追加新 entry）；
- `set` / `delete` / `get`（统一 KV：value 直接进 leaf hash，无独立 values map）；
- `root()`（recompute 脏 twig + 重建 upper，缓存供 prove）；
- `prove()`（twig path 11 + upper path）+ `TwigProof::verify`。

**跨语言一致性验证（P1 关键里程碑）**：5000 inserts（key=blake3(i_le)、value=i_be8）
Rust root **字节等于 gov5 Go QMDB root** `b32e9a4b…689912`（临时 Go 程序对拍，已删）。
证明 twig/upper/leaf/internal 哈希 + append 布局对 gov5 蓝本忠实。

测试：7 个全过（空树/单插/读回/多 twig proof/更新删除/确定性/**Go 对拍**），clippy -D warnings 干净。

## 8c. P2 完成状态（已落地）

`TwigTree` 加上 append 模型的共识确定性 + compaction：
- **`apply_batch(ops)`**：块内按 **keyHash 排序**应用（确定性不变量 #1）——append-slot root
  依赖顺序，所有节点对同一块按 canonical 顺序应用 → 同 root，与输入顺序无关。
- **`compact(threshold)`**：确定性 compaction——稀疏 twig（live ratio < threshold，排除 active
  twig）的 live entry 按 keyHash 排序重 append 到新 slot，清空原 twig，回收其 value 堆。
  **改 root**（重 slot），需所有节点同块边界同阈值（不变量 #2）。
- 空 twig 快速路径：`live==0` 的 twig 直接取 `null_twig_root`（免 2047 次 null hash 折叠）。

**跨语言一致性（P2）**：`apply_batch`(0..5000) 的 Rust root **字节等于** gov5 Go QMDB 同
keyHash-排序 Set 的 root `58671c9a…6526fa` —— canonical 排序不变量对蓝本忠实。
（compaction 是 fresh-genesis 下我们自定义的，不需与 gov5 字节一致，只需我们节点间确定性 +
正确性，已由测试覆盖。）

测试：11 个全过（P1 7 + apply_batch 顺序无关 / compaction 正确性+确定性 / compaction 改 root /
**sorted-batch Go 对拍**），clippy -D warnings 干净。

后续接 §7 的 P3（16 分片 + #11 key 绑定 proof）→ P4 SIMD → …

## 9. 风险

- **共识确定性**（§2 三不变量）—— 头号风险，append 模型 root 依赖顺序 + compaction 改 root。
  缓解：canonical keyHash 排序 + 确定性 compaction + 跨节点对拍 + 对抗测试。
- **proof 安全**：复用 #11 key 绑定 + exclusion 正确性（gov5 此版无 exclusion，需我们自己设计
  + 审计，参考 #1）。
- **SIMD 正确性 / 可移植**：AVX-512 kernel 与标量对拍 root 不变；非 AVX CPU fallback。
- **mobile zero-dep**：twig 验证侧不能带 reth/mdbx（放 `n42-twig-core`）。
- 工作量大（多周、多 crate、共识+安全+持久化+mobile 全链路）。现有 SBMT（PR #1）保持为稳定
  回退基线，新引擎在独立 crate 并行开发，验证通过再切。
