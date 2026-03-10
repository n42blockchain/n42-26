# 开发日志 30 — TPS 各环节实测数据与瓶颈定位

## 1. 测试基准：10 万交易 (90% transfer + 10% ERC-20)

### 1.1 各环节独立测试结果

| # | 环节 | 实测时间 | 数据来源 | 说明 |
|---|------|---------|---------|------|
| 1 | **EVM 执行 (in-memory)** | **83.6ms** | bench CacheDB | 纯 EVM，无 state root，无磁盘 I/O |
| 2 | JSON 序列化 | 0.8ms | bench | ExecutionData → JSON |
| 3 | zstd 压缩 (level=3) | 3.6ms | bench | ~20MB → ~2.8MB (真实比率 14%) |
| 4 | bincode 封装 | <0.1ms | bench | BlockDataBroadcast 包装 |
| 5 | **网络传输 ×6 followers** | **~6ms** | bench TCP localhost | 2.8MB 压缩数据 × 6 = 16.8MB |
| 6 | bincode 解封 | <0.1ms | bench | follower 侧 |
| 7 | zstd 解压 | 2.3ms | bench | 2.8MB → 20MB |
| 8 | **Follower EVM 重执行** | **83.6ms** | bench CacheDB | 与 leader 相同（验证用） |
| 9 | **共识投票 (7 节点)** | **18-31ms** | 实测空块 | 纯投票 RTT，不含等待 |
| 10 | Finalize FCU | <5ms | 实测 Case A | reth engine tree → canonical |
| 11 | Tx pool 打包 10 万 tx | 8.1ms | bench | 生成 + 入队 |

### 1.2 各 block size 的 EVM 执行时间

| Block size | Transfer | ERC-20 | EVM 时间 | Gas | 占 4s slot | 占 2s slot | 占 1s slot |
|-----------|----------|--------|---------|-----|----------|----------|----------|
| 1,000 tx | 900 | 100 | 0.7ms | 29M | 0.0% | 0.0% | 0.1% |
| 5,000 tx | 4,500 | 500 | 3.3ms | 144M | 0.1% | 0.2% | 0.3% |
| 10,000 tx | 9,000 | 1,000 | 6.9ms | 289M | 0.2% | 0.3% | 0.7% |
| **23,809 tx** | 21,429 | 2,380 | **16.9ms** | 688M | **0.4%** | 0.8% | 1.7% |
| 50,000 tx | 45,000 | 5,000 | 38.9ms | 1,445M | 1.0% | 1.9% | 3.9% |
| **100,000 tx** | 90,000 | 10,000 | **83.6ms** | 2,890M | **2.1%** | 4.2% | 8.4% |

**结论：纯 EVM 执行在任何 slot 时间下都不是瓶颈。即使 1s slot + 10 万 tx，EVM 也仅占 8.4%。**

### 1.3 序列化 + 压缩 + 网络传输时间

| Block size | Raw size | 压缩后 (14%) | 压缩时间 | 解压时间 | 网络×6 (localhost) |
|-----------|---------|------------|---------|---------|-------------------|
| 1k txs | 0.2MB | 28KB | 0.1ms | 0.1ms | <1ms |
| 23.8k txs | 5.1MB | 714KB | 0.8ms | 0.5ms | ~2ms |
| 100k txs | 21.3MB | 2.8MB | 3.6ms | 2.3ms | ~6ms |

**结论：压缩+传输+解压全流程仅 ~12ms（100k tx），非瓶颈。**

### 1.4 共识投票实测（从空块 testnet）

```
空块 pipeline 实测数据（7 节点 localhost, 4s slot）:
  view 913: role=F build=0ms import=2ms commit=18ms
  view 914: role=F build=0ms import=1ms commit=26ms
  view 915: role=F build=0ms import=1ms commit=18ms
  view 916: role=F build=0ms import=2ms commit=18ms
  view 917: role=L build=0ms import=0ms commit=21ms (leader 自身)
  view 918: role=F build=0ms import=2ms commit=26ms
```

**共识投票本身极快：p50=18ms, p95=31ms（7 节点本地）。**

---

## 2. 关键发现：bench vs 实际 reth 的巨大差距

### 2.1 reth new_payload 的实测数据（来自日志-26 压测）

| 负载 | Block txs | bench EVM | 实际 import (new_payload) | 差距 |
|------|----------|-----------|--------------------------|------|
| 中负载 | ~6,000 | ~4ms | 110-480ms | **27-120x** |
| 高负载 | ~15,000 | ~10ms | 581-945ms | **58-95x** |
| 极端 | 23,809 | 17ms | **1,401ms** | **82x** |

### 2.2 差距来源分析

```
实际 import = EVM 执行 + State Root (MPT) + 磁盘 I/O + 验证开销
  1,401ms  =    17ms   +     ???ms      +   ???ms   +   ???ms

差距 = 1,401 - 17 = 1,384ms → 全部来自 State Root + 磁盘 I/O
```

**从日志-26 直接数据：**
- EVM 执行：median 16ms, max 202ms
- State root：median 15ms, max 200ms（这是 reth 内部 metrics）
- 但 import 总时间 300-1,401ms 远超两者之和

**推测**：reth 的 `new_payload` 还包含：
- 从 libmdbx 读取账户状态（磁盘 I/O，可能是最大开销）
- Receipt 生成和验证
- Block header 验证
- Engine tree 插入

### 2.3 实际环境下的 State Root 占比

从日志-27 的数据：State root median=15ms, max=200ms。

但这是 reth 报告的单独 state root 时间。在满块场景下：
- EVM: 16-200ms
- State root: 15-200ms
- 磁盘 I/O + 其他: 剩余部分

**对于 23k tx 满块：import=1,401ms 中，EVM 仅占 ~17ms (1.2%)，state root + 磁盘 I/O 占 98.8%。**

---

## 3. 100k TX 完整 Pipeline 时序图

### 3.1 纯计算环节（in-memory 理想情况）

```
Timeline (ms):  0                    90                         182
                |                     |                          |
Leader:         [███ EVM 84ms ███][sc]
                                     ↓ broadcast (6ms)
Follower:                            [dec][███ EVM re-exec 84ms ███]  ← 并行
Consensus:                           [░░ voting 30ms ░░]              ← 并行
Finalize:                                                        [F]

总 Pipeline (含并行): ~182ms
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
sc = serialize(0.8ms) + compress(3.6ms)
dec = decompress(2.3ms)
F = finalize FCU(5ms)
```

### 3.2 含 State Root 的实际情况（基于日志-26 数据）

对 100k tx 块，按日志-26 的 state root 和磁盘 I/O 比例外推：

```
reth 实际 import 时间 ≈ EVM(84ms) + StateRoot(~60ms) + DiskIO(~700ms) = ~844ms
（按 23k tx 的 82x 放大比率，100k tx 的放大会更大）
```

实际 Pipeline（估算）：
```
Timeline (ms):  0              150                   1000                 1050
                |               |                      |                   |
Leader:         [███ Build (EVM+SR) ~150ms ███]
                  (reth resolve_kind 含 state root)
                                ↓ broadcast (6ms)
Follower Import:                [███ new_payload ~844ms ███████████████]  ← 并行
Consensus:                      [░░ voting 30ms ░░]                       ← 并行
Finalize:                                                             [F]

总 Pipeline (含并行): ~1000ms
```

### 3.3 分层总结

| 层级 | in-memory 理想 | reth 实际 (本地) | 瓶颈？ |
|------|-------------|----------------|--------|
| EVM 执行 | 84ms | 84ms | ✗ 仅 8% |
| State Root | 0ms (CacheDB 无) | 15-200ms | △ 潜在 |
| 磁盘 I/O | 0ms | **~700ms+** | **★ 主要瓶颈** |
| 序列化+压缩 | 4.4ms | 4.4ms | ✗ |
| 网络传输 (7 节点) | 6ms | 6ms | ✗ |
| 解压+反序列化 | 2.3ms | 2.3ms | ✗ |
| 共识投票 | 18-31ms | 18-31ms | ✗ |
| Finalize FCU | 5ms | 5ms (Case A) | ✗ |

---

## 4. 瓶颈重新排序（基于实测数据）

### 当前环境（7 节点本地，4s slot，500M gas，23k txs max）

| 排名 | 瓶颈 | 实测数据 | 影响 |
|------|------|---------|------|
| **1** | **reth 磁盘 I/O + State Root** | import 300-1401ms vs bench 17ms | Follower import 是关键路径并行段的瓶颈 |
| **2** | **Slot 节奏限制** | 4s slot，但工作只需 ~1s | 大量空闲时间，可以缩短 |
| **3** | **Gas limit** | 500M → max 23,809 tx/block | 需要提到 1-3G 才能到 10 万 tx |
| **4** | EVM 串行执行 | 84ms/100k tx | 当前完全不是瓶颈 |
| **5** | 网络带宽 (7 节点) | 6ms/2.8MB × 6 | 当前完全不是瓶颈 |

### 关键结论

1. **EVM 不需要并行化** — 100k tx 仅 84ms，即使 1s slot 也只占 8.4%
2. **真正的瓶颈是 reth 的 import 路径** — 磁盘 I/O + state root 使得 23k tx 的 import 耗时 1.4s
3. **共识投票极快** — 7 节点 30ms，不是瓶颈
4. **网络传输不是瓶颈** — 压缩后 2.8MB × 6 followers 仅 6ms（本地）
5. **slot 时间有巨大缩短空间** — 当前 4s 中 95% 是空闲等待
6. **并行 EVM (PEVM) 当前 ROI 极低** — 且我们的实现比串行还慢（实测 0.03x speedup）

---

## 5. 优化路线图（基于实测数据修订）

### Phase 2: 缩短 Slot + 提升 Gas Limit（最高 ROI）

**核心思路**：当前 4s slot 中 95% 是空闲。直接缩短 slot 就是最大的 TPS 提升。

**前提条件**：Leader build + 共识 + follower import 必须在 slot 内完成。

当前满块 (23k txs) pipeline:
- Leader build: ~200ms (reth resolve_kind)
- Network: ~6ms
- Consensus: ~30ms
- Follower import (并行): ~1,400ms ← 这是约束

**方案 A：slot=2s + 保持 500M gas**
- Pipeline 总时间 ~1,400ms（follower import 是瓶颈）
- 能否在 2s 内完成？勉强，但有 buffer
- TPS: 5,952 × 2 = 11,904（同样的每块 tx 数，出块频率翻倍）

**方案 B：slot=2s + gas=1G**
- 每块最多 47,619 tx
- Follower import 时间会增加（估计 ~2-3s）→ **2s slot 放不下**
- 需要先解决 import 瓶颈

**结论**：缩短 slot 到 2s 立即可行（保持现有 gas limit），TPS 直接翻倍到 ~12,000。但进一步缩短需要先解决 reth import 的磁盘 I/O 问题。

### Phase 2.5: 异步 State Root (Delayed State Root)

**核心思路**：Follower import 不需要验证 state root，因为 HotStuff-2 的安全性不依赖执行结果。

当前 follower import (new_payload) 做了：
1. EVM 重执行 → 17ms
2. State root 计算 → 15-200ms
3. 磁盘 I/O (读写状态) → ~700ms+
4. 验证 state root 匹配 → ~1ms

**如果跳过 follower 的 state root 验证**：
- Import 时间从 1,400ms 降到 ~800ms (去掉 state root 200ms + 部分磁盘写入)
- 安全性由 leader 执行保证，follower 只需信任 QC 链

**如果 Leader build 也延迟 state root**（EIP-7862 风格）：
- Build 时间从 200ms 降到 ~100ms
- State root 在下一个块的 header 中包含

**效果**：Pipeline 总时间可能降到 ~600ms，允许 slot=1s

### Phase 3: 优化 reth Import 路径（磁盘 I/O）

**核心问题**：reth 的 new_payload 对每个 tx 都要从 libmdbx 读取账户状态，这是 import 时间的最大来源。

**方案**：
1. **热状态内存缓存**：最近 N 个 block 涉及的账户状态保持在内存中
2. **Compact Block**：Follower 不做 EVM 重执行，只验证 leader 提供的 state diff
3. **reth flat state 优化**：利用 reth 的 flat state 减少 trie 遍历

### Phase 4: 并行 EVM（长期，当前不紧急）

**实测证据**：
- 串行 EVM 100k tx = 84ms，1s slot 下仅占 8.4%
- 当前 PEVM 实现反而更慢（0.03x）
- 只有当其他瓶颈全部解决后，EVM 才会成为限制因素
- 预计在 gas limit > 5G (238k+ tx/block) 时才需要

---

## 6. 时间规划（修订）

| 步骤 | 优化项 | 预期 TPS | 依赖 | 难度 |
|------|--------|---------|------|------|
| **1** | 缩短 slot 到 2s | ~12,000 | 无 | 低（改配置） |
| **2** | Delayed State Root | ~20,000+ | 修改 chainspec | 中 |
| **3** | 热状态缓存 / 优化 import | ~30,000+ | reth 侧修改 | 中-高 |
| **4** | Compact Block (跳过 follower 重执行) | +性能 | 协议修改 | 中 |
| 远期 | 并行 EVM | 在 EVM 成为瓶颈时 | 所有上述 | 高 |

**最高 ROI 路径：步骤 1 → 步骤 2 → 步骤 3**

步骤 1（缩短 slot）是纯配置修改，立即可行，TPS 翻倍。

---

## 7. Benchmark 原始数据

### ETH Transfer 单核性能
```
100,000 iterations: 74.27ms
Per tx: 743 ns
Theoretical TPS: 1,346,380
```

### ERC-20 Transfer 单核性能
```
100,000 iterations: 95.72ms
Per tx: 957 ns
Theoretical TPS: 1,044,731
```

### Tx Pool 打包性能
```
100,000 txs: generate=5.0ms  enqueue=3.1ms  total=8.1ms
Rate: 12,354k tx/s
```

### 网络传输性能 (localhost TCP, 6 followers)
```
  500KB × 6:   1.9ms  throughput=12.6Gbps
    1MB × 6:   3.0ms  throughput=16.0Gbps
    2MB × 6:   5.3ms  throughput=18.2Gbps
    5MB × 6:  10.8ms  throughput=22.2Gbps
   10MB × 6:  13.4ms  throughput=35.8Gbps
```

### 空块共识 Pipeline（实测 7 节点 testnet）
```
role=F: import=1-3ms, commit=18-31ms
role=L: import=0ms,   commit=21ms
```

### 满块 Import（实测，from 日志-26）
```
import (new_payload) p50 = ~500ms
import (new_payload) max = 1,401ms (23,809 txs)
```
