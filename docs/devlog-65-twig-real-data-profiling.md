# devlog-65 — twig 引擎真实主网数据剖析 + value-arena 优化

## 数据来源

`D:\reth2k` 是**完整以太坊主网 reth archive**(25.2M 块）：HashedAccounts **3.91 亿**、
HashedStorages **15.8 亿**（共 ~20 亿叶，~200GB+，远超 DRAM）。用 `reth.exe db list
HashedAccounts --len 1000000 --json` 导出 1M 真实账户（key=hashed address，value=
nonce/balance/bytecode_hash），喂 twig 引擎剖析。

剖析工具：`crates/n42-twig-core/examples/profile_real.rs`（解析真实账户 → `ShardedTwig`，
阶段计时 + RSS + proof 采样；unix 下出 pprof flamegraph，Windows 用手动 breakdown）。

> `states/state_logs_data.bin`（317GB，`STLOGDT2` 自定义格式，pevm.exe 产出、无源码）逆向
> 风险高，未采用；改用 reth.exe 直接导表（可靠）。

## 剖析结果（1M 真实主网账户，9950X，release）

| 指标 | 优化前 | value-arena 后 | vs SBMT(arena) |
|------|--------|----------------|----------------|
| **RSS** | 304 MB | **280 MB（280 B/acct）** | SBMT 375 → **省 25%** |
| build | 1.33s | **1.30s**（772k accts/s） | 持平 |
| proof | 929 B / gen 1.71us / verify 1.80us | 同 | proof gen 比 SBMT 快 |

吞吐与合成基准一致（key 都是均匀 hash）。proof gen 1.7us（twig path 11 + upper + shard，
扁平堆直读，比 SBMT 的 3us 快）。

## 内存拆解（剖析驱动）

@1M 的 ~300MB：entries 的每值 `Vec<u8>`（24B header + 堆分配圆整）~45% + index HashMap
~18% + twig 节点数组（2 hash/leaf × 32B = 64B/leaf）~21%。

## 优化：value arena（root 不变，−8% RSS）

把每 entry 的 `value: Vec<u8>` 换成**扁平 `value_arena: Vec<u8>`** + `Entry{voff:u64, vlen:u32}`
索引切片：
- 消除每值 24B Vec header + 每值一次堆分配/圆整；值连续利于缓存。
- `voff` 用 u64，单 shard arena 可超 4GB。
- snapshot 对死 entry 存空值 → restore 时压缩 arena（回收 churn 死值）。
- **root 完全不变**：3 个 gov5 跨语言对拍 + 14 测试全过，clippy 干净。

实测：RSS 304→280MB（−8%）、build 略快（少 1M 次堆分配）。

## 优化 2：compact index（root 不变，−6% RSS + 更快）

index 由 `HashMap<[u8;32], u64>` 改为 `HashMap<u128, u64>`（16 字节 key 前缀）+ 查找时用
entry 的完整 key 确认。16 字节前缀碰撞 ≈ N²/2¹²⁹ ≈ 3.7e-21（全 1.58B key，与树自身抗碰撞同级），
且 index 不进 root（root 由树的完整 key 决定），安全。
- RSS 280→263MB（−6%），build 1.30→1.21s（u128 key 比 32 字节 hash 快）。

## 优化 3：自适应 fold（root 不变，build 2.5x）

每插入原 eager-fold 11 次 hash；稠密 genesis 填 twig 时 = 2048×11 ≈ 22528 次/twig，浪费。改为
**deferred + 每块按密度自适应**：apply_batch 只写叶 + 记 touched，`root()` 时每 twig 二选一——
稠密（≥187 改动）整体重算一次（2047 次，~11x 少），稀疏才逐叶 eager fold。
- **build 1.21→0.53s（1.89M accts/s，2.5x！单线程 scalar）**，RSS 持平（touched buffer 用后 shrink）。
- root 完全不变（3 个 gov5 对拍 + 14 测试全过）。
- 重算的"逐层"循环正是 **SIMD 批量的天然结构**（以后 drop-in AVX 即可）。

## 累计效果（1M 真实主网账户，单线程 scalar）

| | RSS | 吞吐 |
|---|-----|------|
| 原始 | 304 MB | 750k accts/s |
| + value-arena | 280 | 772k |
| + compact index | 263 | 824k |
| **+ 自适应 fold** | **264 MB（264 B/acct）** | **1.89M accts/s** |
| **总计** | **−13%；vs SBMT −30%** | **2.5x**；已超 C2 Go 单树 1.71M |

## SIMD（AVX-512 16-way）现状

CPU 热点仍是 blake3。自适应 fold 已把 hash **数量**降下来（算法级），且重算循环是 SIMD-ready。
**真·AVX-512 16-way blake3** 是 CPU 侧最后杠杆，但 `blake3` crate 无公开 batch（hash_many）API，
需 vendored SIMD blake3 或手写 std::arch intrinsics（数百行、共识级、须与单 hash 逐字节对拍）——
不宜一次性硬写。计划：在 `Twig::recompute` 的逐层循环引入 `hash_node_8/16`（AVX2/AVX-512 +
标量 fallback + 运行时检测），用 gov5 cross-check 验证 root 字节恒等；参考 C2 的 Go AVX-512 接法。
预计在 hash-bound 部分再叠加 ~2-4x。

## 跨 shard rayon 并行：实测无收益（内存延迟绑定）

给 `ShardedTwig` 加可选 `rayon` feature（默认关，保 mobile zero-dep）：apply_batch/root
用 `par_iter_mut` 跨 16 shard 并行、无锁（TwigTree 无 Cell，是 Send）。实测（1M 真实账户）：

| workload | 单线程 | 16 核并行 |
|----------|--------|-----------|
| genesis build | 0.53s | 0.56s（持平） |
| 400K 更新（25K/块） | 533k/s | 525k/s（持平） |
| 400K 更新（单大块） | 924k/s | 822k/s（**反而慢**） |

**并行无收益甚至倒退。** 根因:twig 更新是**内存延迟绑定** —— 每次更新随机访问 200MB 的
twig/arena/index(cache miss),16 核做随机 miss 不 scale(瓶颈是内存延迟,非 CPU/带宽),
rayon 派发开销反而拖慢。预留容量避免 realloc 竞争后仍无改善。

这与之前 SBMT thread-sharding 的发现一致:**缓存布局(AlDBaran)才是杠杆,不是核数。** C2 的
Go 7.99M 来自连续 twig(L2 驻留)+ AVX-512,不是单纯并行。→ n42-jmt **不启用** rayon(scalar
这里更快);feature 保留为其他硬件/workload 可选。

> 注:我们 Rust **scalar build 1.89M/s 已超 C2 Go 单树 1.71M**(无 GC + 自适应 fold)。更新
> 吞吐 925k 低于 C2 Go 7.99M,差距是 cache 布局 + SIMD + 紧循环(AlDBaran 技术),并行解决不了。

## 结论与后续

- twig 全-DRAM @1M 真实账户 **280 B/acct**，比 SBMT 省 25%。但距 AlDBaran/QMDB ~100 B/acct
  仍有差距 —— 根因是 all-DRAM 把 entry 数据 + 全 twig 节点驻内存（QMDB 驱逐 twig 叶 + entry 下盘）。
  这是 all-DRAM 路线的内禀成本（用户已选 all-DRAM 换吞吐/简单）。
- **CPU 热点 = blake3**（每插入 12 次：1 leaf + 11 fold）。SIMD 批量（AVX-512 16-way）是 CPU 侧
  下一杠杆（需 blake3 内部/vendored，参考 C2）。pprof flamegraph 在 Linux/WSL 跑 `profile_real`
  可直接看到 blake3 占比。
- 后续可选内存杠杆：compact index（QMDB 式只存 key 高位 + slot，省 ~7%）。

## 复现

```
# 导真实账户（reth.exe 在 D:\reth2k）
reth.exe db --datadir D:\reth2k list HashedAccounts --len 1000000 --json > D:\reth2k-accounts-1m.json
# 剖析
PROFILE_ACCOUNTS=D:/reth2k-accounts-1m.json cargo run --release --example profile_real -p n42-twig-core
# Linux/WSL 额外产出 twig_flamegraph.svg
```
