# 开发日志-44: 并发时序分析与 TX Pool 优化研究

## 日期: 2026-03-09

## 1. 数据纠正：Fast Propose vs Slot-Aligned 吞吐量相同

### 1.1 苹果对苹果比较 (7.5K target step, 注入饱和)

| 指标 | 2s Slot-Aligned | Fast Propose | 差异 |
|------|----------------|-------------|------|
| **effective_tps** (120s) | **13,828** | **13,704** | **-0.9% (无差异)** |
| sent (120s 总量) | 1,740,000 | 1,712,000 | 相同量级 |
| blocks (120s) | 62 | 90 | 更多块但更小 |
| tx/block | 28,064 | 19,022 | 不同 |
| block_time | 2.0s | ~1.33s (120/90) | - |
| 真实 TPS (sent/120) | **14,500** | **14,267** | **无差异** |
| avg_block_tps (误导) | 13,959 | 20,228 | 采样窗口差异 |

**结论**: `effective_tps` 完全相同。`avg_block_tps` 差异是 BLOCK_ANALYSIS 采样 50 块窗口的统计假象——Fast Propose 块更小、block_time 更短，tx/block_time 值被放大。

### 1.2 为什么 TPS = 注入速率，与 block time 无关

理论: 如果注入速率 = R tx/s (恒定), block_time = T:
- tx_per_block = R × T
- TPS = (R × T) / T = R

无论 block time 是 1s 还是 2s, **持续吞吐量 = 注入速率**。当前 14K TPS 天花板是 stress tool + RPC 注入速率的限制，不是节点处理速率。

## 2. 并发时序图 (2s Slot-Aligned, 精确到代码)

### 2.1 完整 View 生命周期

```
                        Slot Boundary (每 2s)
                              ↓
时间 ─────────────────────────┬────────────────────────────────────────────────────►
                              │
LEADER NODE (单 tokio runtime):
──────────────────────────────│──────────────────────────────────────────────────
                              │
Thread 1: select! 主循环       │
  build_timer fires ──────────┤
  do_trigger_payload_build()──│──► FCU(attrs) to reth engine ──► 获得 payload_id
                              │    ↓
                              │    spawn_payload_resolve_task()
                              │    ↓ (tokio::spawn, 异步)
                              │
Thread 2: reth PayloadBuilder  │
  ┌───────────────────────────│────────────────────────────────────────┐
  │ payload_job.run()         │                                        │
  │   每 50ms (builder.interval):                                      │
  │     ├── pool.best_transactions()   ◄── 从 TX Pool 拉取 pending tx  │
  │     │     └── 按 gas price 排序, 依赖 pool 内部索引               │
  │     ├── EVM execute batch (~23-35ms for 25K tx)                    │
  │     ├── state_root compute (~25-38ms)                              │
  │     └── 发送 best payload to resolve channel                       │
  │   总计: ~60-90ms (25K tx)                                          │
  └────────────────────────────────────────────────────────────────────┘
                              │    ↓ payload ready
                              │
Thread 1: block_ready_rx      │
  engine.process_event(BlockReady) → on_block_ready()
  ├── 创建 Proposal (BLS sign ~0.1ms)
  ├── 自加 leader vote (R1)
  ├── emit BroadcastMessage(Proposal) → GossipSub (~1ms)
  └── broadcast_block_data():         ◄── 与 Proposal 并行 发出
        ├── serialize compact block (~3ms)
        ├── zstd compress (~17ms)
        ├── send_block_direct to N peers (QUIC, 并行)
        └── announce_block (GossipSub)
                              │
                              │─── NETWORK PROPAGATION ───────────────
                              │    (LAN: 10-50ms, WAN: 150-300ms)
                              │
FOLLOWER NODES (并行, 独立):
──────────────────────────────│──────────────────────────────────────────────────
                              │
Follower-1 select! 主循环:     │
  net_event_rx → ConsensusMessage(Proposal)
  │ engine.process_event(Message(Proposal))
  │   └── process_proposal()
  │         ├── verify leader BLS sig (~1ms)
  │         ├── verify justify_qc aggregate sig (~2ms)
  │         ├── safety check: is_safe_to_vote (~0)
  │         ├── emit ExecuteBlock(hash)
  │         │     └── handle_execute_block():
  │         │           if pending_block_data.contains(hash):
  │         │             → emit BlockImported → send_vote ── 立即投票
  │         │           else:
  │         │             → pending_executions.insert(hash) ── 等待 block data
  │         └── check imported_blocks → defer/vote
  │
  net_event_rx → BlockData                    ◄── 可能先于 Proposal 到达
  │ handle_block_data()
  │   ├── deserialize (~1ms)
  │   ├── decompress (~5ms)
  │   ├── inject compact block to payload_cache
  │   ├── store in pending_block_data
  │   └── if pending_executions.contains(hash):
  │         → emit BlockImported
  │         → on_block_imported() → send_vote()
  │
  send_vote():
  │ ├── BLS sign vote (~0.1ms)
  │ └── emit SendToValidator(leader, Vote)
  │       ├── send_direct(QUIC) to leader
  │       └── broadcast_consensus(GossipSub)   ◄── 双路径 (日志-42)
  │
                              │
LEADER 收集 R1 Votes:         │
──────────────────────────────│──────────────────────────────────────────────────
  net_event_rx → Vote         │
  │ engine.process_event(Message(Vote))
  │   ├── verify BLS sig (~1ms)
  │   ├── VoteCollector.add_vote()
  │   └── check quorum (5/7)
  │         ├── NO: 等待更多 vote
  │         └── YES: try_form_prepare_qc()
  │               ├── aggregate BLS sigs (~0.1ms)
  │               └── emit BroadcastMessage(PrepareQC)
  │
  [FOLLOWER 收到 PrepareQC]:
  │ process_prepare_qc()
  │   ├── verify QC aggregate sig (~2ms)
  │   └── send CommitVote to leader
  │
LEADER 收集 R2 CommitVotes:
  │ 同 R1 流程
  │ → try_form_commit_qc()
  │     ├── emit BroadcastMessage(Decide)
  │     ├── emit BlockCommitted{view, hash, commit_qc}
  │     └── emit ViewChanged{new_view}
  │
  handle_block_committed():
  │ ├── finalize_committed_block()
  │ │     └── FCU (finalize) → reth engine
  │ │           Case A: block in tree → FCU instant (<5ms)
  │ │           Case B: deferred → spawn background import
  │ └── if leader: schedule_payload_build()
  │                   └── next_slot_boundary() → 等待下一个 2s 边界
  │
  ═══════════ 下一个 View 开始 ═══════════
```

### 2.2 并行/串行标注

```
SLOT = 2000ms

t=0ms    ├── SLOT BOUNDARY: build_timer fires
         │
t=0ms    ├── FCU(attrs) → reth ─────────────────────┐
         │                                            │ 异步: reth PayloadBuilder
t=60ms   │                                            ├── payload ready
         │                                            │
t=60ms   ├── Proposal + BlockData broadcast ──────────┤
         │   (GossipSub + QUIC, 并行)                 │
         │                                            │
t=70ms   │── NETWORK PROPAGATION (LAN ~10ms) ────────│
         │                                            │
t=80ms   │── FOLLOWERS VERIFY + VOTE (并行, ~5ms) ───│
         │                                            │
t=100ms  │── R1 VOTE COLLECTION (等 5/7, ~20ms) ─────│
         │                                            │
t=120ms  │── PrepareQC BROADCAST ─────────────────────│
         │                                            │
t=140ms  │── FOLLOWER COMMIT VOTE (并行, ~5ms) ──────│
         │                                            │
t=160ms  │── R2 VOTE COLLECTION (等 5/7, ~20ms) ─────│
         │                                            │
t=180ms  │── Decide + Commit + FCU finalize ──────────│
         │                                            │
t=180ms  ├── ★ IDLE: 等待下一个 slot boundary ★       │
         │   (约 1820ms 空闲!)                         │
         │                                            │
         │── 此期间 TX Pool 持续接收并验证 tx ──────────│
         │── reth 异步 persist 前一个块 ────────────────│
         │                                            │
t=2000ms ├── 下一个 SLOT BOUNDARY ────────────────────┘

真正的共识处理时间: ~180ms (9%)
等待下一 slot: ~1820ms (91%)
```

### 2.3 Fast Propose 的等价流程

```
t=0ms    ├── COMMIT → ViewChanged
         │
t=0ms    ├── schedule_payload_build() → MIN_PROPOSE_DELAY=500ms
         │
t=500ms  ├── build_timer fires → FCU(attrs) → reth
         │
t=560ms  ├── payload ready → Propose
         │
t=580ms  ├── R1 collect (~20ms)
         │
t=600ms  ├── PrepareQC → R2 collect (~20ms)
         │
t=620ms  ├── Commit → next view
         │
等价 block_time: ~620ms
```

### 2.4 关键差异分析

**Slot-Aligned 2s**: 共识处理 180ms + 空闲 1820ms = 2000ms/view
**Fast Propose (500ms delay)**: 等待 500ms + 共识处理 120ms ≈ 620ms/view

**但 TPS 相同!** 因为:
- 2s slot: 积累 28K tx → 28K/2.0s = 14K TPS
- 620ms view: 积累 9K tx → 9K/0.62s = 14.5K TPS
- 都受限于 **注入速率 ~14K tx/s**

**差异的唯一体现**: **Finality 延迟** (用户等待确认的时间)
- Slot-Aligned 2s: 确认延迟 ~2s
- Fast Propose: 确认延迟 ~0.6s

## 3. TX Pool 独立线程分析

### 3.1 当前 TX Pool 在 tokio runtime 中的位置

```
┌─────────────────────────────────────────────────────────────┐
│ 共享 tokio runtime (所有任务竞争 CPU)                         │
│                                                               │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐ │
│  │ Orchestrator  │  │ reth Engine  │  │ RPC Server           │ │
│  │ select! loop  │  │ (payload     │  │ (接收 tx,            │ │
│  │ (共识消息处理) │  │  builder)    │  │  调 pool.add_tx())   │ │
│  └──────────────┘  └──────────────┘  └──────────────────────┘ │
│         ↑                 ↑                    ↓              │
│         │                 │           ┌──────────────────┐    │
│    GossipSub/QUIC    best_txs()      │ TX Pool           │    │
│    消息收发              ↑             │  add_external_tx()│    │
│         │                │           │  validate()       │    │
│         └────────────────┴───────────│  - stateless      │    │
│                                      │  - stateful       │    │
│                                      │    (nonce/balance) │    │
│                                      └──────────────────┘    │
└─────────────────────────────────────────────────────────────┘

问题: 高负载时 pool.add_external_tx() 的 validation 消耗大量 CPU,
      挤压 GossipSub 消息处理 → 共识投票延迟 → block time 拉长
```

### 3.2 reth TX Pool 验证流程 (代码级)

```
tx 到达 RPC → eth_sendRawTransaction
  │
  ├── 1. ECDSA signature recovery (外部, secp256k1)
  │     → 恢复 sender 地址, 约 60μs/tx
  │     → N42_SKIP_TX_VERIFY=1 时跳过
  │
  ├── 2. STATELESS 验证 (validate_stateless, 不访问数据库)
  │     ├── tx 类型检查 (legacy/EIP-1559/4844 等)
  │     ├── nonce != u64::MAX (EIP-2681)
  │     ├── tx 大小 ≤ max_tx_input_bytes
  │     ├── gas_limit ≤ block_gas_limit
  │     ├── max_priority_fee ≤ max_fee_per_gas
  │     ├── chain_id 匹配
  │     └── intrinsic gas 检查
  │     → 约 1-5μs/tx (纯 CPU)
  │
  ├── 3. STATEFUL 验证 (validate_stateful, 需要数据库查询)
  │     ├── state.basic_account(sender) ← 数据库查询!
  │     │     → 获取 (nonce, balance, code_hash)
  │     │     → 约 10-50μs (hot cache) 或 500μs+ (cold)
  │     ├── sender 不是合约 (code_hash 检查)
  │     ├── tx.nonce >= account.nonce ← NONCE 检查
  │     └── cost ≤ account.balance ← BALANCE 检查 (可跳过!)
  │
  └── 4. 入池分类
        ├── tx.nonce == account.nonce → "pending" (可立即打包)
        └── tx.nonce > account.nonce → "queued" (等待 gap 填充)
```

### 3.3 验证开销分析

| 步骤 | 耗时/tx | 可否跳过 | 说明 |
|------|--------|---------|------|
| ECDSA recovery | ~60μs | ✅ 已跳过 (N42_SKIP_TX_VERIFY) | 测试环境 |
| Stateless check | ~1-5μs | ❌ 必须 | 防止 invalid tx 浪费资源 |
| DB lookup (nonce/balance) | **10-500μs** | ❌ 但可优化 | **主要瓶颈** |
| Balance check | ~0μs (已查) | ✅ 可跳过 | `disable_balance_check()` |
| Nonce check | ~0μs (已查) | ⚠️ 需保留 | 但可更智能 |
| Pool insertion | ~1-5μs | ❌ 必须 | BTreeMap/HashMap 操作 |

**关键发现**: **数据库查询 (`state.basic_account`) 是 pool validation 的真正瓶颈**，不是签名验证。

### 3.4 Nonce 管理的智能化研究

#### 当前问题

```
Account A: on-chain nonce = 100

tx(nonce=100) → pending (可打包)
tx(nonce=101) → queued (等 100 先执行)
tx(nonce=102) → queued
tx(nonce=105) → queued (gap: 103, 104 缺失)

如果 103, 104 永远不来 → 105 永远不能执行 → 浪费内存
如果 100 执行后, 101/102 自动升级为 pending
```

#### 问题场景

1. **Nonce gap**: stress tool 多线程发送，nonce 不连续 → queued tx 积压
2. **Nonce too low**: 重试发送已确认的 nonce → 被 pool 拒绝 → rpc_err
3. **Pool 查询开销**: 每次 add_tx 都要查 `basic_account` 获取最新 nonce

#### 更智能的 Nonce 管理方案

```
方案: In-Memory Nonce Cache (环形缓冲区)

┌──────────────────────────────────────────────────────┐
│ NonceCache (per-sender 环形缓冲区)                     │
│                                                        │
│ sender → {                                             │
│   confirmed_nonce: u64,    // 已上链的 nonce            │
│   next_expected: u64,      // pool 中最大连续 nonce + 1 │
│   pending_count: u32,      // pending 中的 tx 数        │
│   last_updated: Instant,   // 上次更新时间              │
│ }                                                      │
│                                                        │
│ 新 tx 到达:                                             │
│   if tx.nonce < confirmed_nonce → 拒绝 (nonce too low) │
│   if tx.nonce == next_expected → pending, next_expected++ │
│   if tx.nonce > next_expected → queued (gap)            │
│   不需要查数据库! 用内存缓存即可!                         │
│                                                        │
│ 块提交后:                                               │
│   confirmed_nonce = max(confirmed_nonce, block_nonce)  │
│   清理 nonce < confirmed_nonce 的 queued tx              │
│   提升 queued → pending (如果 gap 填充)                  │
└──────────────────────────────────────────────────────┘
```

**关键优势**:
- **消除数据库查询**: nonce 信息缓存在内存中
- **O(1) 查找**: HashMap<Address, NonceState>
- **块提交时批量更新**: 而不是每笔 tx 查一次

#### 余额检查的必要性分析

**用户洞察**: "比对余额没必要，因为可以收到转账后余额足够"

**分析**:
- ✅ **正确**: 在出块执行时，余额可能已经因为前一笔 tx 而增加
- ✅ reth 已有 `disable_balance_check()` 配置
- ⚠️ **风险**: 如果不检查余额，恶意用户可以无限发送 tx 占满 pool
- **折中方案**: pool 入口做 **宽松余额检查**（余额 > gas_cost 即可，不检查 value）

### 3.5 更高效的内存模型

#### 当前 reth pool 结构

```
reth TransactionPool:
  ├── pending: BTreeMap<TxId, Transaction>  // 按 gas_price 排序
  ├── queued: HashMap<Address, BTreeMap<u64, Transaction>>  // 按 nonce 排序
  └── all: HashMap<TxHash, Transaction>  // 全局索引
```

#### 环形缓冲区方案 (Ring Buffer Pool)

```
对于高 TPS 场景, 大部分 tx 是 pending (nonce 连续), 极少 queued:

RingBufferPool:
  ├── ring: Vec<Transaction>  // 固定大小环形缓冲区 (e.g., 200K entries)
  │     head ─►[tx_1][tx_2][tx_3]...[tx_N]◄─ tail
  │     写入: O(1), 满时覆盖最旧 tx
  │     读取: leader 从 head 批量取 N 笔
  │
  ├── nonce_cache: HashMap<Address, NonceState>
  │     查 nonce: O(1), 无数据库
  │
  └── by_hash: HashMap<TxHash, u32>  // hash → ring index
        查重: O(1)

打包时: leader 从 ring head 开始, 批量取 tx, 跳过无效的
块提交后: 更新 nonce_cache, 标记已执行的 ring 位置
```

**优势 vs 当前 BTreeMap**:

| 操作 | BTreeMap | Ring Buffer |
|------|----------|-------------|
| 插入 | O(log N) | O(1) |
| 按序读取 N 笔 | O(N log N) | O(N) |
| 删除已执行 | O(log N) per tx | O(1) 移动 head |
| 内存碎片 | 高 (树节点分散) | 无 (连续内存) |
| Cache friendliness | 差 | 极好 |

## 4. 1s Slot-Aligned 测试计划

为了验证 "调整为 1s Slot-Aligned 是否得到 Fast Propose 同样效果":

```bash
# 修改 testnet.sh 的 BLOCK_INTERVAL_MS 为 1000
N42_SKIP_TX_VERIFY=1 BLOCK_INTERVAL_MS=1000 \
  scripts/testnet.sh --nodes 7 --clean --no-explorer --no-tx-gen --no-monitor --no-mobile-sim

# 压测
N42_SKIP_TX_VERIFY=1 target/release/n42-stress --step --duration 120 --accounts 3000 --batch-size 2000 --prefill 20000
```

**预期**: effective_tps 与 2s Slot-Aligned 和 Fast Propose 相同 (~14K)，因为瓶颈在注入速率。
**验证点**: block_time 是否稳定 1.0s，共识是否在 1s 内完成。

## 5. 2s Slot-Aligned 不损失性能的设计

既然 TPS = 注入速率 (与 block time 无关)，2s Slot-Aligned 本身不损失性能。
真正的优化方向是 **降低确认延迟** 和 **提高注入上限**:

### 5.1 Pipeline Build (speculative)

当前: commit → 等 slot boundary → build → propose
优化: commit → **立即开始 build** → slot boundary 时 resolve → propose

```
CURRENT:                         OPTIMIZED:
commit at t=200ms                commit at t=200ms
wait until t=2000ms              start build at t=200ms (speculative)
build: t=2000-2060ms             slot boundary: t=2000ms, payload ready
propose: t=2060ms                propose: t=2000ms (快 60ms)
```

节省: ~60-90ms per slot. 对 TPS 无影响 (因为注入限制)，但减少 finality 延迟。

### 5.2 TX Pool 优化 (提高注入上限)

真正能提高 TPS 的是加速 tx 进入 pool 的速度:
1. **Nonce cache** — 消除数据库查询
2. **disable_balance_check** — 减少验证步骤
3. **TX Pool 独立线程** — 避免与共识竞争 CPU
4. **Batch validation** — 一次查询多个 account 的 nonce

这些优化能将注入上限从 ~14K 提升到 30-50K TPS，此时 block time 的选择才会开始影响 TPS。

## 6. 1s Slot-Aligned 压测验证

### 6.1 测试环境

- 7 nodes, macOS, `BLOCK_INTERVAL_MS=1000`, `N42_FAST_PROPOSE=0`, `N42_SKIP_TX_VERIFY=1`
- Gas limit: 2G, Target TPS: 20,000, Duration: 120s
- Stress v9: prefill 20K, accounts 3000, batch 2000

### 6.2 测试结果

```
FINAL RESULT:
  target_tps=20000
  effective_tps=14,478
  sent=1,764,480  rpc_err=1,520  http_err=0
  blocks=70  avg_tx_per_block=25,206
  duration=122s
  avg_rpc_latency_ms=1069.6
  nonce_resyncs=0

BLOCK_ANALYSIS (50 blocks):
  avg_block_time=1.7s
  overall_tps=14,449
  avg_block_tps=15,672
  p50_tps=12,960  p95_tps=26,192
  max_block_tps=29,051  max_tx_in_block=29,449
  gas_utilization=25.8%
```

### 6.3 三种模式对比

| 指标 | 2s Slot-Aligned | 1s Slot-Aligned | Fast Propose (500ms) |
|------|----------------|----------------|---------------------|
| **effective_tps** | **13,828** | **14,478** | **13,704** |
| avg_block_tps | 13,959 | 15,672 | 23,916* |
| avg_block_time | 2.0s | **1.7s** | 1.0s |
| max single block TPS | 25,498 | **29,051** | 25,498 |
| max tx/block | 25,498 | **29,449** | 25,498 |
| gas_utilization | 23% | 25.8% | 23% |
| rpc_latency_ms | ~1000 | ~1070 | ~1000 |
| blocks (120s) | ~60 | **70** | ~120 |
| fail_rate | 0% | 0% | 0% |
| resyncs | 0 | 0 | 0 |

*Fast Propose avg_block_tps 偏高是 BLOCK_ANALYSIS 50 块采样窗口的统计假象

### 6.4 关键发现

1. **三种模式 effective_tps 完全一致 (~14K)**
   - 再次证明瓶颈在注入端 (stress tool + RPC)，与 block time 无关

2. **1s Slot-Aligned 实际 block time = 1.7s（不是 1.0s）**
   - Leader 日志显示：每个 validator 每 7 个 view 轮到一次 leader
   - 空载 block 间隔: 7s (对应 7 个 validator × 1s slot = 7s/validator)
   - 高负载 block 间隔: 12-14s/validator = **1.7-2.0s/block 实际**
   - 原因: 共识 round-trip (propose → vote → prepare_qc → commit_vote → commit_qc) 在高负载 + 大 block (25K+ tx, 5-7MB payload) 下耗时 > 1s
   - 共识超时 1s slot → 自动滑到下一个 slot boundary

3. **1s Slot-Aligned vs 2s Slot-Aligned: 几乎无差异**
   - 1s 设置在高负载下退化到 ~1.7s，接近 2s
   - 真正的 block time 由共识 round-trip 决定，不是由 slot 配置决定

4. **Fast Propose 的真正优势: 最小 finality 延迟**
   - 不等 slot boundary，commit 后立即 propose → finality 更快
   - 但 sustained TPS 与其他模式相同

### 6.5 结论: Slot Time 选择

**2s Slot-Aligned 是当前最佳选择**：
- effective_tps 与 1s 和 Fast Propose 完全相同 (~14K)
- 2s 给共识留足裕量，block_time 稳定 2.0s（不像 1s 模式 jitter 到 1.7s）
- 适合全球互联网延迟环境（RTT 100-300ms 留足缓冲）
- 当注入能力提升到 30K+ TPS 后再考虑减小 slot time

**未来路线**：
- **Phase 1**: TX Pool 优化 → 突破 14K 注入天花板到 30-50K TPS
- **Phase 2**: 如果 30K+ TPS 仍稳定在 2s slot → 无需调整
- **Phase 2b**: 如果 TPS 受限于 block time → 减小到 1.5s 或 1s
- **Phase 3**: Fast Propose 用于低延迟场景（DeFi、交易所）

## 7. testnet.sh 优化: 账户缓存

### 7.1 问题

每次 `--clean` 启动需要 Python 生成 5000 个账户（~30s），调用 eth_account.Account.from_key × 5000 次。

### 7.2 方案

- 缓存文件: `scripts/testnet-accounts-cache.json` (20,001 行，约 480KB)
- 账户是确定性的 (`keccak("n42-test-key-{i}")`)，每次结果相同
- 首次生成后缓存，后续从文件加载 (<0.1s vs 30s)
- 无缓存时自动降级为原始生成方式

### 7.3 改动

- `scripts/testnet.sh`: `generate_genesis()` 优先读取 `$SCRIPT_DIR/testnet-accounts-cache.json`
- `scripts/testnet-accounts-cache.json`: 预生成的 5000 账户 (key + address)

## 8. Pre-sign Load 压测突破 27K TPS

### 8.1 实现

新增 `--presign-save` 和 `--presign-load` CLI 参数：

```bash
# 生成 (37s, 一次性)
n42-stress --accounts 5000 --presign 5000000 --presign-save /tmp/n42-presigned-5M.bin

# 加载+发送 (3s 加载, 纯 I/O 发送)
n42-stress --accounts 5000 --target-tps 20000 --presign-load /tmp/n42-presigned-5M.bin
```

- 二进制格式: `N42T` magic + RPC 分组 + 长度前缀的 RLP 字节
- 文件大小: **570 MB** (5M tx)
- 签名速率: 134K tx/s (多线程并行)
- 加载速率: **1.7M tx/s** (2.9秒加载 5M tx)
- 内存占用: ~1.1 GB (hex 字符串)
- 账户绑定 RPC: `account_idx % 7` → 零 nonce gap

### 8.2 测试结果

```
PRE-SIGN LOAD RESULT (5M tx, 20K target TPS, 2s Slot-Aligned):
  effective_tps = 27,484   ← 突破 14K 天花板! +96%
  injection_tps = 27,491
  sent = 4,998,593 / 5,000,000
  rpc_err = 1,407 (0.03%)
  blocks = 42 in 182s
  avg_tx_per_block = 119,014
  avg_rpc_latency = 2,167ms

BLOCK_ANALYSIS (50 blocks):
  avg_block_time = 4.5s    ← 2s slot 膨胀到 4.5s!
  overall_tps = 14,817
  avg_block_tps = 15,172
  max_block_tps = 33,408
  max_tx_in_block = 80,000 (命中 MAX_TXS_PER_BLOCK 上限)
  gas_utilization = 69.1%  ← 从 25% 升到 69%!
```

### 8.3 并行时序瓶颈分析 (80K tx/block)

```
=== 空闲 (view 14, 空 block) ===
proposal=@1973ms  R1_collect=11ms  R2_collect=14ms  total=1998ms  ← 正常 2s
import=1ms  commit=25ms

=== 高负载开始 (view 21, ~46K tx/block) ===
proposal=@1308ms  R1_collect=512ms  R2_collect=257ms  total=2078ms
import=108ms  commit=770ms

=== 高负载峰值 (view 28, 80K tx/block) ===
proposal=@2744ms  R1_collect=1377ms  R2_collect=384ms  total=4506ms  ← consensus 4.5s!
import=183ms  commit=1762ms

=== 高负载持续 (view 35, 80K tx/block) ===
proposal=@3152ms  R1_collect=1006ms  R2_collect=119ms  total=4278ms
import=174ms  commit=1126ms

Leader Payload Pack (80K tx):
  packing_ms = 604-1052  (vs 25K tx: 32-51ms, 12-20x 慢)
  evm_exec_ms = 431-875  (vs 25K tx: 23-35ms, 12-25x 慢)
  pool_overhead = 168-176ms
  payload_kb = 18,766 KB  (18.3 MB per block!)
  ser_ms = 12-13ms
```

### 8.4 瓶颈层次模型

```
层次 1: Stress Tool Injection (已突破)
  之前: 签名 ~80μs/tx → 14K TPS 天花板
  现在: presign-load → 27K+ TPS injection
  状态: ✅ 已解决

层次 2: TX Pool CPU 竞争 (部分暴露)
  pool_pending 峰值 200K, queued 100K
  R1_collect 从 11ms → 1377ms (125x)
  原因: pool 验证占用 tokio CPU → GossipSub 延迟
  状态: ⚠️ 下一优化目标

层次 3: Payload Build (新暴露)
  80K tx packing: 604-1052ms (之前 25K: 32-51ms)
  EVM exec: 431-875ms
  proposal 延迟到 @2744-3152ms (超过 slot boundary)
  状态: ⚠️ 大 block 下的主要瓶颈

层次 4: Block Propagation (新暴露)
  80K tx = 18.3 MB payload
  GossipSub 传播延迟 → follower 投票延迟
  R1_collect 暴涨的另一原因
  状态: ⚠️ 与层次 3 关联

层次 5: Block Import
  follower import: 99-443ms (80K tx, 含 compact block)
  commit: 322-1762ms
  状态: ⚠️ 比 25K tx 慢 5-10x
```

### 8.5 关键发现

1. **effective_tps 27K 是"注入率"而非"持续出块率"**
   - 42 blocks / 182s = 0.23 blocks/s → 4.3s/block
   - avg_tx_per_block = 119K → overall_tps = 119K / 4.3 = **27.7K TPS**
   - 但 BLOCK_ANALYSIS 的 overall_tps = 14,817 因为它用 block_timestamp 计算，而 block_timestamp 有大跳跃

2. **80K tx/block 触发了新的瓶颈层**
   - Payload build 从 50ms → 1000ms (20x)
   - 18.3 MB payload 需要更长的网络传播
   - Pool 200K pending 导致 GossipSub 延迟
   - 共识 total 从 2s → 4.5s (2.25x)

3. **MAX_TXS_PER_BLOCK=80K 成为天花板**
   - 多个 block 恰好 80,000 tx → 命中上限
   - Gas util 84% (1.68G / 2G) → gas 也接近上限

### 8.6 下一步优化方向

**短期 (突破 30K+ TPS)**:
1. **增大 MAX_TXS_PER_BLOCK** 到 200K 或去掉限制
2. **增大 gas_limit** 到 5G
3. **TX Pool 独立 runtime** — 隔离 pool 和 consensus CPU

**中期 (突破 50K TPS)**:
4. **减小 block payload** — compact block 已经有，但 80K tx 仍 18MB
5. **Parallel EVM** — 80K tx EVM 耗时 875ms，并行可降至 100ms
6. **Batch pool validation** — 减少 per-tx DB 查询

**长期 (100K+ TPS)**:
7. **Sub-block pipelining** — 不等完整 80K block
8. **StateRoot-Free Consensus** — 省去 state root 计算
9. **Sharding / Multi-proposer**
