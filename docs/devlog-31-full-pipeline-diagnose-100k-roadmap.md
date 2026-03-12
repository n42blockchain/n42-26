# 开发日志 31 — 全流程瓶颈诊断与 100K TPS 路线图

## 测试环境
- 7 节点 localhost testnet，macOS (Apple Silicon)
- 2s slot, 500M gas limit
- reth devp2p tx gossip 已禁用 (`--disable-tx-gossip`)
- N42 GossipSub tx gossip 已禁用（默认）
- TX Forward to Leader 为唯一 tx 传播路径

## 压测结果汇总

| Target TPS | Sustained TPS | Peak TPS | Max tx/block | Gas Util |
|---|---|---|---|---|
| 500 | 578 | 1,252 | 2,504 | 4.7% |
| 1,000 | 911 | 1,037 | 2,074 | 7.4% |
| 2,000 | 1,745 | 2,012 | 4,024 | 14.2% |
| 3,000 | 2,413 | 2,593 | 5,186 | 19.6% |
| 5,000 | 3,379 | 4,643 | 9,286 | 27.4% |
| 7,500 | 4,312 | 5,665 | 11,330 | 35.0% |
| 10,000 | 5,097 | 7,156 | 14,312 | 41.4% |
| 15,000 | **6,176** | **8,491** | 16,982 | 50.1% |
| 20,000 | **6,497** | **9,382** | 18,763 | 52.8% |

**当前天花板：~6,500 sustained TPS，~9,400 peak TPS**

## 一、Pipeline 各阶段耗时分析

### Leader Build Pipeline
| 阶段 | avg | p50 | p95 | max | 说明 |
|---|---|---|---|---|---|
| Serialize (JSON) | 0.5ms | 0 | 2 | 2 | serde_json::to_vec |
| Compress (zstd) | 3.5ms | 3 | 10 | 13 | zstd level 3 |
| Direct Push | 0.0ms | 0 | 0 | 0 | 异步 channel send |
| **Leader Import** | **22.5ms** | **21** | **49** | **216** | payload cache 命中 = 跳过 EVM |

### Follower Import Pipeline
| 阶段 | avg | p50 | p95 | max | 说明 |
|---|---|---|---|---|---|
| Decompress | 1.0ms | 0 | 4 | 40 | zstd decompress |
| Deserialize | 1.2ms | 0 | 5 | 67 | serde_json::from_slice |
| **new_payload** | **208.7ms** | **167** | **542** | **862** | EVM + state root + disk I/O |

### Consensus Voting (last 50 blocks)
| 阶段 | avg | p50 | p95 | max |
|---|---|---|---|---|
| R1_collect | 146ms | 177 | 235 | 235 |
| R2_collect | 48ms | 24 | 180 | 180 |

### Block Size Distribution
| 指标 | avg | p50 | p95 | max |
|---|---|---|---|---|
| tx_count | 5,485 | 4,540 | 14,888 | 18,763 |
| Raw JSON | 1,290 KB | 1,069 | 3,504 | 4,416 KB |
| Compressed | 410 KB | 339 | 1,114 | 1,406 KB |

## 二、系统资源使用分析

### 总体 CPU
- 平均: 46.8% (user 28.4% + sys 18.5%)
- **峰值: 82.0%**
- CPU 没有饱和（7 个 n42-node 共享，每个平均约 25.9%）

### 单节点 (n42-node) 进程
- 平均 RSS: 2,876 MB → **峰值 RSS: 3,881 MB**（内存增长明显）
- 平均 CPU: 25.9% → **峰值 CPU: 96.7%**
- 峰值 CPU 出现在高 TPS 阶段，说明 CPU 是瓶颈之一

### 网络
- 平均: in=24 KB/s, out=40 KB/s
- **峰值: in=99 KB/s, out=145 KB/s**
- 网络带宽远未饱和（即使在真实网络 1Gbps 下也绰绰有余）
- 压缩后的块数据最大 1.4 MB，2s 内传输仅需 ~11ms @ 1Gbps

### 磁盘 I/O
- iostat 显示: read=28 KB/s, write=33 KB/s（**采样粒度太粗，未捕获突发**）
- 真实瓶颈在 reth 的 libmdbx 随机 I/O（new_payload 中的 state trie 读写）

## 三、瓶颈分析

### 当前 2s slot 的时间预算

```
2000ms slot budget:
├── [Leader Build]        ~200ms (payload build + resolve)
│   ├── tx_pack:          ~10ms (从 pool 取 tx)
│   ├── evm_exec:         ~17ms (实际 EVM 执行，已 bench)
│   ├── state_root:       ~50ms (trie hash computation)
│   └── payload_cache:    ~0ms (缓存 hit 时跳过重复执行)
├── [Broadcast]           ~15ms (serialize + compress + direct push)
│   ├── serialize:        ~1ms
│   ├── compress:         ~4ms
│   └── direct_push:      ~10ms (6 peers × 1.4MB QUIC)
├── [Consensus]           ~250ms (R1 + R2 voting)
│   ├── R1_collect:       ~177ms (5/7 votes)
│   └── R2_collect:       ~48ms
├── [Follower Import]     ~209ms (new_payload = EVM + SR + disk I/O) ← 关键路径
│   ├── decompress+deser: ~2ms
│   ├── evm_exec:         ~17ms
│   ├── state_root:       ~50ms
│   └── disk_io:          ~140ms (libmdbx read/write) ← PRIMARY BOTTLENECK
├── [Finalize]            ~10ms (FCU to reth)
└── [Margin]              ~1300ms (大量闲置 = 可缩短 slot)
```

### 瓶颈排名

1. **🔴 reth new_payload disk I/O (follower): avg=209ms, max=862ms**
   - 占 2s slot 的 10-43%
   - 10K tx 时 p95=542ms → 剩余 1.5s 给其他阶段
   - 100K tx 时预估 ~5s（线性外推）→ **完全不可行**

2. **🟡 Consensus voting: ~250ms**
   - HotStuff-2 本身已是最少轮次的 BFT 协议
   - localhost 环境，真实网络 RTT 50ms 时约 ~500ms

3. **🟢 EVM 执行: ~17ms/万tx（单核）**
   - 100K tx → ~170ms（单核），可接受
   - 并行 EVM 可进一步压缩

4. **🟢 State Root: ~50ms**
   - 需要优化但不是首要瓶颈

5. **🟢 网络传输: <15ms**
   - 压缩效率好（70% 压缩率），QUIC 直连快

## 四、100K TPS 理论可行性

目标：100,000 TPS = 200,000 tx per 2s block

### 各阶段 100K tx 预估

| 阶段 | 当前 (5K tx) | 100K tx 预估 | 可行性 |
|---|---|---|---|
| EVM 执行 | 17ms | 170ms | ✅ 单核可行 |
| State Root | 50ms | ~500ms | 🟡 需优化（延迟/并行） |
| Serialize | 1ms | ~20ms | ✅ |
| Compress | 4ms | ~80ms | ✅ |
| Network TX | 10ms | ~200ms | ✅ (1Gbps: 30MB/2s) |
| **new_payload** | **209ms** | **~4,000ms** | **❌ 不可能** |

**关键洞察：reth new_payload 是 O(n) 线性增长的，100K tx 时会超过 slot time。
必须从根本上改变 import 架构。**

## 五、优化路线图 — 通往 100K TPS

### Phase A: 低风险优化 → 目标 10K-15K TPS (2-4x 当前)

#### A1: 缩短 slot 到 1s ⚡
- **前提**: new_payload 在 500ms 内完成（当前 p50=167ms，可行）
- **限制**: 需控制块大小，每块 ~5000 tx × 0.5 slot = 5000 TPS
- **风险**: 大块时 import > 1s 会 stall
- **方案**: 动态 gas limit，根据上一块 import 时间调整

#### A2: 禁用 Follower EVM Re-execution (Compact Block) ⚡⚡
- **原理**: Leader 已执行 EVM 并签名确认，Follower 无需重新执行
- **实现**: Leader 在块中附带 state root + execution output hash
  - Follower 验证 state root commitment，跳过 EVM
  - 相当于 Solana/Sei 的 "optimistic execution"
- **效果**: new_payload 从 209ms → ~50ms（只做 state root 验证 + disk write）
- **预估 TPS**: 10,000+ sustained

#### A3: Delayed State Root ⚡
- **原理**: State root 计算从关键路径移除，在下一个 slot 计算上一块的 state root
- **效果**: 进一步缩短 import 时间
- **复杂度**: 中等，需修改 reth Engine API 交互

### Phase B: 中等风险 → 目标 20K-50K TPS (10x)

#### B1: 自定义存储引擎替代 libmdbx
- **原理**: reth 的 libmdbx 是通用 K/V store，不针对区块链 state trie 优化
- **方案**:
  - **内存缓存层**: 将热 state 保持在内存中（~16GB for 1M accounts）
  - **批量 flush**: 异步写盘，每 N 块 flush 一次
  - **参考**: Sei uses SeiDB (custom), Monad uses MonadDB (custom)
- **效果**: import I/O 从 ~140ms 降到 ~10ms（内存命中）
- **预估 TPS**: 20,000-30,000

#### B2: Parallel EVM (修复 PEVM)
- **当前**: PEVM 实现 broken（比串行慢）
- **目标**: 100K tx EVM 执行从 170ms → 30ms（8核并行）
- **参考**: Sei Parallel EVM, Monad speculative execution

#### B3: Pipeline Slot Overlap
- **原理**: N+1 块的 build 和 N 块的 import 重叠执行
- **当前**: 已有 eager import，但 build 仍等待上一块 commit
- **优化**: 基于 pending state (N-1 committed + N executing) 开始 N+1 build
- **效果**: 有效 slot time ≈ max(build, import) 而非 sum

### Phase C: 高风险/长期 → 目标 50K-100K TPS (20x)

#### C1: StateRoot-Free 共识（最激进）
- **原理**: 共识层不再验证 state root，只验证 tx ordering 和 leader 签名
- Leader 附带 state root 承诺（commitment）
- Follower 异步验证 state root（不在关键路径上）
- 如果发现不一致 → slash leader
- **参考**: Sei 的做法类似 — block 先 finalize 再 verify
- **效果**: 块确认时间 = consensus 投票时间（~250ms）
- **预估 TPS**: 50,000+ (每 2s 块可以包含 100K tx，因为 import 不在关键路径)

#### C2: Sharding / Multi-Lane
- 将交易按 sender 地址分片到多个并行 lane
- 每个 lane 独立 build + execute + commit
- 最终合并 state root
- **复杂度**: 极高，需要跨 lane 状态解决

#### C3: Block Production Separation
- 将 Block Builder 和 Block Validator 分离到不同机器
- Builder: 高 CPU（EVM）+ 高内存（state cache）
- Validator: 只做签名验证 + state root 验证
- 参考: MEV-Boost / Proposer-Builder Separation (PBS)

## 六、推荐实施顺序

```
当前: 6,500 TPS sustained
  │
  ├── A2: Compact Block (skip follower EVM) ──── 估计 2 周
  │   → 12,000 TPS
  │
  ├── A3: Delayed State Root ──── 估计 1 周
  │   → 15,000 TPS
  │
  ├── A1: 1s slot ──── 估计 2 天（配置调整）
  │   → 15,000 TPS (latency halved)
  │
  ├── B1: In-memory State Cache ──── 估计 3 周
  │   → 30,000 TPS
  │
  ├── B2: Fix Parallel EVM ──── 估计 2 周
  │   → 40,000 TPS
  │
  ├── C1: StateRoot-Free Consensus ──── 估计 4 周
  │   → 80,000-100,000 TPS
  │
  └── [Stretch] C2+C3: Sharding + PBS
      → 200,000+ TPS
```

## 七、对标 Sei Giga

Sei Giga 200K TPS 的关键技术：
1. **SeiDB**: 自定义存储引擎，替代通用 K/V store
2. **Parallel EVM**: 并行执行无状态冲突的交易
3. **Optimistic Execution**: 先出块后验证，验证失败 slash
4. **Twin Turbo Consensus**: 2 轮共识（类似 HotStuff-2）
5. **Async State Root**: state root 计算从共识关键路径移除

N42 已具备：
- ✅ HotStuff-2 共识（与 Sei 同级别）
- ✅ TX Forward to Leader（低延迟 tx 传播）
- ✅ Leader Payload Cache（避免 leader 重复执行）
- ✅ Eager Import（pipeline 并行）
- ❌ 自定义存储 → Phase B1
- ❌ Parallel EVM → Phase B2
- ❌ Async State Root → Phase A3/C1
- ❌ Optimistic Execution → Phase A2

**结论**: 通过 A2 + A3 + B1 三步，理论上可达 30K-50K TPS。
加上 C1 (StateRoot-Free Consensus) 可冲击 100K TPS。
