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

## twig buffering（算法级，root 不变，更新 +29%）

`Twig::fold_batch`：一个块内多次改同一 twig 时，**逐层算父节点、每节点只 hash 一次**（兄弟对去重），
取代"每叶各自 fold 11 层"（K×11，共享祖先重复算）。`root()` 的稀疏分支用它（稠密仍整体重算）。

为何有效：一个 25000-更新的块,每 shard ~1562 更新散到 ~31 twig = **每 twig ~50 改动**（K≈50），
原 50×11=550(重复) → buffering ~union-of-paths（~5x 少 hash）。

实测（1M 真实账户,400K 更新）：

| block | 优化前 | twig buffering |
|-------|--------|----------------|
| 25000（真实块大小） | 533k/s | **687k/s（+29%）** |
| 200000（K>187→recompute） | — | 761k/s（buffering 不适用,本就重算） |

root 不变（3 个 gov5 对拍 + 14 测试全过）;genesis build 持平（稠密走 recompute,不变）。

> 真实块大小（K 中等）正是共识场景,所以 +29% 是实打实的更新吞吐提升。更大块 K>187 走 recompute
> 已最优,buffering 不适用。

## AVX-512 16-way SIMD（root 不变，build +10% / 更新 +23%）

关键洞察让 vendored kernel 可控：`hash_node(l,r)=blake3(l‖r)` 是 64 字节**单块**输入 = 一次
压缩函数调用（CV=IV、len=64、flags=CHUNK_START|CHUNK_END|ROOT）。无需 vendor 整个 blake3，
只写**单块压缩的转置（SoA）kernel**：`src/simd.rs`，AVX-512 16-lane（原生 VPROLD 旋转）+
AVX2 8-lane + 标量 fallback，运行时检测（`TWIG_NO_SIMD=1` 强制标量供 A/B）。对齐 C2 Go
`compressNodes16AVX512` 蓝图。

接入 `Twig::recompute`（整层连续批）与 `fold_batch`（去重层列表）——两者天然保证
子读/父写不相交。**正确性**：kernel 输出逐字节对拍 blake3 crate（随机输入 + 满批/尾部/全零
null-level），3 个 gov5 端到端 root 对拍在 SIMD 路径下全过。

实测（9950X Zen5 AVX-512）：build 1.77→1.96M accts/s（+10%）、400K 更新 663→812k（+23%）。
非 6x 是 Amdahl：buffering 后 fold hash 只占更新一部分，剩余是内存延迟（下一项）+ 标量 leaf hash。

## cache-friendly 布局：FlatIndex + lookahead prefetch（更新再 +44%）

更新路径剩余瓶颈 = 随机 DRAM miss（index 探查为首）。两招（`src/flat.rs`）：
1. **FlatIndex**（开放寻址，16B prefix → slot）替换 `std HashMap<u128,u64>`：免 SipHash
   （bucket = prefix 与**每进程秘密种子**的乘法混合 — 种子保密杀 HashDoS 研磨，索引不进
   root 故共识无关）、线性探测、tombstone、暴露 bucket 地址**可显式预取**。差分 fuzz
   20 万次 vs std HashMap 全一致。
2. **lookahead 预取**：apply 循环处理 op[i] 时 `_mm_prefetch` op[i+12] 的 index bucket —
   把下一个随机 miss 与当前 op 的 hashing 重叠（AlDBaran"下个节点已在 L2"思路）。

实测：400K 更新 812k → **1.17M upd/s（+44%）**；build 1.96 → **2.17M accts/s**（insert 免
SipHash）；RSS 持平略降（引擎 ~245MB）。root 不变（19 测试含 gov5 对拍全过）。

## 优化总账（1M 真实主网账户，单线程，root 全程不变）

| 阶段 | build | 400K 更新 | RSS（引擎） |
|------|-------|-----------|-------------|
| 初版 | 750k accts/s | 533k upd/s | 304 MB |
| value-arena + compact index + 自适应 fold + twig buffering | 1.89M | 687k | 264 MB |
| + AVX-512 SIMD | 1.96M | 812k | 264 MB |
| **+ FlatIndex + prefetch** | **2.17M** | **1.17M** | **~245 MB** |
| **累计** | **2.9×** | **2.2×** | **−19%（vs SBMT −35%）** |

对标：C2 Go 单树 1.71M（我们 build 2.17M 超 27%）；C2 Go 16 分片 AVX-512 7.99M 是 16 核
并行 + 覆写 workload 的数 — 我们已实测跨 shard 并行在此 workload 内存绑定无收益（见上），
单线程 1.17M 更新已是诚实口径。剩余杠杆：leaf hash（hash_leaf 2-block）批量 SIMD、
deactivate 旧 slot 的 twig 叶写预取。

## 采样诊断（samply/ETW，pprof 的 Windows 等价）+ 第三轮优化

WSL 未装（pprof-rs unix-only），改用 **samply 0.13（ETW 采样）**：
`samply record --save-only --unstable-presymbolicate -o profile.json.gz ./profile_real.exe`，
3M 更新 workload，解析 Firefox-profiler JSON 取 self-time 热点。另加 `[profile.profiling]`
（inherits release + debug 符号）供采样。

**诊断发现（决定性）**：`RtlFreeHeap` 7.5% self-time + 对应 alloc ≈ **分配器是第一热点**。
根因：`ops: &[(Hash, Option<Vec<u8>>)]` API **强制每 op 一个 owned Vec<u8>** —— 3M 更新 =
300 万次 malloc/free（调用方构造 + apply 后 drop）。

**修复（第三轮，全部 root 不变）**：
1. **借用 API**：核心改为 `apply_batch_refs(&[(Hash, Option<&[u8]>)])` / `apply_batch_indexed`
   借用版（owned 版保留为兼容壳）。
2. **TwigState::prepare 改 arena**：StateDiff 展平为 (key, (off,len)) 元数据 + 单一 value
   arena，借用切片直通引擎 —— 节点真实路径同样零 per-op 分配。
3. example harness 同步（块级复用 buffer）。

实测：更新 **1.5M → ~1.8M upd/s（再 +25%）**。复采确认 heap self-time 绝对量大降
（残余 ~7% 为每块工作 Vec —— jobs/leaf_hashes/per_shard/借用数组，scratch 复用可再压，边际）。

## 最终总账（1M 真实主网账户，单线程，root 全程不变，gov5 字节对拍全程通过）

| 阶段 | build | 400K 更新 |
|------|-------|-----------|
| 初版 | 750k accts/s | 533k upd/s |
| 算法四连（arena/index/fold/buffering） | 1.89M | 687k |
| + AVX-512/AVX2 node SIMD | 1.96M | 812k |
| + FlatIndex + prefetch | 2.17M | 1.17M |
| + 索引分桶（去 clone）+ leaf SIMD | ~4.4M | ~1.5M |
| **+ 借用 API（去分配）** | **~4.4M** | **~1.8M** |
| **累计** | **5.9×** | **3.4×** |

vs C2 Go：单树 build 超 2.6×（4.4M vs 1.71M）；Go 16 分片 AVX-512 7.99M 为 16 核并行数，
我们单线程 1.8M（并行已实测内存绑定无收益，口径不同）。

## 全局优化清单（整体视角，按 ROI 排序）

**引擎内（边际收益递减，可选）**
- 每块工作 Vec scratch 复用（heap 残余 ~7% → ~2%）
- deactivate 旧 slot 的 twig 叶行预取（随机写 miss）
- proof 生成路径（非热，按需）

**引擎外 / 系统级（真实节点路径上的下一批大杠杆）**
- **key 派生 SIMD**：`account_key = blake3(domain‖addr)` 是 <64B 单块 —— 与 node kernel 同构，
  可直接 16-way 批量！prepare 阶段每 op 一次派生（曾测占 SBMT 更新 17%），落在 n42-jmt 桥接层。
- **PersistentTwig WAL**（P6）：块级 fsync 摊销 / 组提交。
- snapshot 序列化：大状态 bincode 可分 shard 并行 + 增量。
- upper 树增量折叠：十亿级 twig 才需要（当前 31 twig/shard 全量重建微不足道）。
- 流水线并行（架构级）：节点上 EVM 执行与上一块树更新重叠 —— 单 workload 内存绑定，
  但执行/哈希异质负载可并行。

**内存（按需）**
- FlatIndex 负载因子/容量策略调优；twig 叶驱逐（仅当放弃全 DRAM 路线时，QMDB tier-2）。

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
