# 开发日志-43: 深度时序分析与 TPS 优化方案

## 日期: 2026-03-09

## 1. 完整 Pipeline 时序图

### 1.1 Leader 视角 (HotStuff-2 Optimistic Path)

```
时间 ──────────────────────────────────────────────────────────────────────────────►

View N committed ──► ViewChanged ──► schedule_payload_build()
                                          │
         ┌────────────────────────────────┘
         ▼
  [MIN_PROPOSE_DELAY: 500ms]  ◄── 可调参数，当前最大瓶颈
         │
         ▼
  do_trigger_payload_build(timestamp)
         │
         ├──► reth PayloadBuilder.new_payload_job()  ◄── 异步启动
         │         │
         │    [PayloadBuilder 在后台持续 packing txs]
         │         │  packing: 32-51ms (日志-35)
         │         │  evm_exec: 23-35ms
         │         │  state_root: 25-38ms (p50/p95)
         │         │  total: ~60-90ms
         │         ▼
         │    payload_ready → block_ready_rx channel
         │
         ▼
  engine.process_event(BlockReady(hash))
         │
         ├──► on_block_ready()
         │    ├── 创建 Proposal{view, hash, justify_qc}
         │    ├── BLS sign: ~0.1ms
         │    ├── 自加 leader vote (Round 1)
         │    └── emit BroadcastMessage(Proposal)
         │
         ▼
  ┌─────────────────────────────────────────────────────────────┐
  │ PARALLEL (异步):                                              │
  │                                                               │
  │  Path A: broadcast_block_data()                               │
  │    ├── serialize CompactBlockExecution (serde_json): ~3ms     │
  │    ├── compress (zstd level 3): ~17ms                         │
  │    ├── send_block_direct to all peers (QUIC): ~1-5ms          │
  │    └── announce_block via GossipSub: ~1ms                     │
  │                                                               │
  │  Path B: broadcast_consensus(Proposal)                        │
  │    └── GossipSub mesh flooding: ~1ms                          │
  │                                                               │
  │  Path C: leader_payload_tx → leader feedback                  │
  │    └── cache block data in pending_block_data                 │
  └─────────────────────────────────────────────────────────────┘
         │
         ▼
  [等待 Round 1 Votes]  ◄── 网络延迟 + follower 处理
         │
         │  收集 Vote (per validator):
         │    ├── GossipSub 接收: ~10-50ms (LAN)
         │    ├── verify BLS signature: ~1ms
         │    └── add_vote to VoteCollector
         │
         │  达到 quorum (2f+1 = 5/7):
         ▼
  try_form_prepare_qc()
         │
         ├── aggregate BLS signatures: ~0.1ms
         ├── emit BroadcastMessage(PrepareQC)  ◄── 可选: chained mode 省略
         ▼
  [等待 Round 2 CommitVotes]
         │
         │  达到 quorum:
         ▼
  try_form_commit_qc()
         │
         ├── emit BroadcastMessage(Decide)
         ├── emit BlockCommitted{view, hash, commit_qc}
         └── emit ViewChanged{new_view}
                    │
                    ▼
              [回到顶部: schedule_payload_build]
```

### 1.2 Follower 视角

```
时间 ──────────────────────────────────────────────────────────────────────────────►

  ┌─────────────────────────────────────────────────────────────┐
  │ PARALLEL 接收 (到达顺序不确定):                                 │
  │                                                               │
  │  Path A: Proposal 到达 (via GossipSub)                       │
  │    ├── verify leader signature (BLS): ~1ms                    │
  │    ├── verify justify_qc aggregate sig: ~2ms                  │
  │    ├── safety check (is_safe_to_vote): ~0                     │
  │    ├── enter_voting()                                         │
  │    ├── emit ExecuteBlock(hash)                                │
  │    └── check imported_blocks → defer or vote immediately      │
  │                                                               │
  │  Path B: BlockData 到达 (via Direct Push / GossipSub)         │
  │    ├── deserialize BlockDataBroadcast: ~1ms                   │
  │    ├── decompress payload: ~5ms                               │
  │    ├── inject compact block to payload_cache: ~1ms            │
  │    ├── store in pending_block_data                            │
  │    └── if pending_execution exists → BlockImported event      │
  └─────────────────────────────────────────────────────────────┘

  Case 1: BlockData 先到, Proposal 后到 (常见)
  ──────────────────────────────────────────────
  BlockData → stored in pending_block_data
  Proposal  → process_proposal() → ExecuteBlock → handle_execute_block()
                                                    → has_data=true → BlockImported
                                                    → imported_blocks.insert(hash)
           → imported_blocks.remove(hash) = true → send_vote() 立即

  Case 2: Proposal 先到, BlockData 后到
  ──────────────────────────────────────
  Proposal  → process_proposal() → ExecuteBlock → handle_execute_block()
                                                    → has_data=false → pending_executions.insert
           → imported_blocks empty → pending_proposal = Some(...)

  BlockData → handle_block_data() → pending_executions.contains → BlockImported
           → on_block_imported() → pending_proposal match → send_vote()

  投票后续:
  ──────────
  send_vote() → SendToValidator(leader, Vote)
             → direct (QUIC) + broadcast (GossipSub)  ◄── 日志-42 修复: 双路径

  PrepareQC 到达 → process_prepare_qc()
                 → verify QC → send CommitVote to leader
                 → SendToValidator(leader, CommitVote)

  Decide 到达 → BlockCommitted → finalize_committed_block()
              → FCU (Case A: block already in reth) or
              → background_import (Case B: deferred)
```

### 1.3 共识时序数值分析 (Fast Propose, 7 nodes, LAN)

```
┌─────────────────────────────────────────────────────────────────────────┐
│                    CONSENSUS TIMING BREAKDOWN                          │
│                    (日志-41 实测: total ~551ms)                          │
├──────────────────────┬──────────────────────────────────────────────────┤
│ Phase                │ Duration           Notes                        │
├──────────────────────┼──────────────────────────────────────────────────┤
│ MIN_PROPOSE_DELAY    │ 500ms              ★ 当前最大等待项              │
│ Payload build        │ 60-90ms            packing+EVM+state_root       │
│ Proposal broadcast   │ ~1ms               GossipSub                    │
│ BlockData broadcast  │ ~20ms              serialize+compress+send      │
│ Network propagation  │ ~10-50ms           LAN GossipSub mesh           │
│ Follower verify+vote │ ~3-5ms             BLS verify + vote sign       │
│ Vote collection R1   │ ~50-100ms          等 5/7 votes via network     │
│ PrepareQC broadcast  │ ~1ms               (chained mode may skip)      │
│ CommitVote collection│ ~50-100ms          等 5/7 commit votes          │
│ Decide broadcast     │ ~1ms               GossipSub                    │
├──────────────────────┼──────────────────────────────────────────────────┤
│ TOTAL (估算)         │ ~700-870ms         不含 MIN_PROPOSE_DELAY        │
│ TOTAL (含delay)      │ ~1200-1370ms       含 500ms delay               │
│ ACTUAL (日志-41)     │ ~551ms             ★ 快于估算 (pipeline overlap) │
└──────────────────────┴──────────────────────────────────────────────────┘

为何实测 551ms < 估算 700-870ms?
→ Pipeline overlap: Payload build 与上一 view 的 finalization 并行
→ Fast Propose: MIN_PROPOSE_DELAY 内已在 build
→ Block data 在 Proposal 前已开始传播 (leader 先发 block data)
→ 一些 followers 的 block data 在 proposal 到达前就已缓存
```

## 2. 异步并行处理标注

### 2.1 Pipeline 并行区域

```
View N                                    View N+1
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Leader:
  [Build_N]──►[Propose_N]──►[Collect R1]──►[PrepareQC]──►[Collect R2]──►[Commit_N]──►[Build_N+1]...
                   │              │                             │
                   │   ┌──────────┘                             │
                   │   │  PARALLEL                              │
                   │   ▼                                        │
Follower 1:        │  [Recv Proposal]──►[Vote]                  │
                   │   ▲                  │                     │
Follower 2:        │  [Recv Proposal]──►[Vote]──►[CommitVote]──►│
                   │   ▲                  │                     │
Follower 3:        │  [Recv Proposal]──►[Vote]──►[CommitVote]──►│
                   │                                            │
                   │                                            │
Block Data:   [Broadcast]────────────────────────────────────►[Background Import]
              ▲ ASYNC, parallel with consensus                  ▲ ASYNC, parallel with N+1
              │                                                 │
              └── 不阻塞共识投票                                  └── 不阻塞下一 view

reth Engine:
  ─────[FCU finalize_N]──────────────────────────[FCU finalize_{N+1}]────────
        ▲ 阻塞 leader 下一次 build? 不! FCU < 5ms (Case A, block in tree)
```

### 2.2 关键并行点

| 操作 | 串行/并行 | 说明 |
|------|----------|------|
| Payload Build | **与上一 view finalize 并行** | reth builder 是异步 job, 不等 FCU |
| Block Data 广播 | **与 Proposal 广播并行** | 两个独立 network 调用 |
| Follower 投票 | **6 个 follower 并行** | 独立处理, 不互相等待 |
| Vote 收集 R1 | **边收边验** | add_vote 逐个处理, quorum 即停 |
| CommitVote R2 | **与 R1 类似** | 边收边验 |
| Block import (reth) | **后台异步** (Case B) | tokio::spawn, 不阻塞共识 |
| FCU finalize | **可能阻塞** (Case A < 5ms) | 快速路径: block 已在 engine tree |
| Persist to disk | **完全异步** | reth 内部异步 persist, 720ms p50 |

### 2.3 串行瓶颈链

```
CRITICAL PATH (决定 block time):
MIN_PROPOSE_DELAY ──► Payload Build ──► Network Propagation ──► Vote Collection (2 rounds)
     500ms               60-90ms            10-50ms               100-200ms
     ════                                                         ════════
   可压缩到 0           不可再快                                   受网络限制

NON-CRITICAL (不影响 block time):
  Block Data broadcast: 并行
  reth import: 并行 (Case B) 或 已完成 (Case A)
  Disk persist: 完全异步
```

## 3. 瓶颈深度分析

### 3.1 瓶颈层次模型

```
                    ┌────────────────────────────────┐
                    │   Layer 0: Transaction Supply   │  ← 当前天花板
                    │   Stress tool: ~14K TPS max     │
                    │   RPC latency: 68ms → 1023ms    │
                    ├────────────────────────────────┤
                    │   Layer 1: Consensus Latency    │  ← 理论天花板
                    │   View time: ~551ms (Fast)      │
                    │   MIN_PROPOSE_DELAY: 500ms      │
                    ├────────────────────────────────┤
                    │   Layer 2: Block Capacity       │  ← 远未饱和
                    │   Gas: 22.9% of 2G limit        │
                    │   TX/block: 25K (cap 80K)       │
                    ├────────────────────────────────┤
                    │   Layer 3: reth Import          │  ← 非瓶颈 (compact block)
                    │   Follower import: 3-5ms        │
                    │   State root: 0ms (cached)      │
                    ├────────────────────────────────┤
                    │   Layer 4: EVM Execution        │  ← 非瓶颈
                    │   23K tx: 17ms single-core      │
                    │   Leader: 23-35ms               │
                    └────────────────────────────────┘
```

### 3.2 Layer 0: Transaction Supply 瓶颈 (当前限制)

**现象**: effective_tps 在 ~14K 封顶, 但 avg_block_tps 可达 22K — 链能力 > 注入能力

**根因分解**:

1. **ECDSA 签名** (已解决: `N42_SKIP_TX_VERIFY=1`):
   - secp256k1 sign: ~60μs/tx → 单核 16.7K TPS
   - v9 多签名线程已部分缓解
   - presign mode (已实现) 完全移除热路径签名

2. **RPC 序列化 + HTTP 传输**:
   - eth_sendRawTransaction batch JSON 编码: ~5μs/tx
   - HTTP round-trip: 68ms (低负载) → 1023ms (高负载)
   - **高负载 RPC 退化原因**: reth 的 tx pool validation 在 RPC handler 内同步执行

3. **TX Pool 入池速度**:
   - reth TransactionPool::add_external_transactions()
   - 每笔 tx: RLP decode + signature recovery + nonce check + balance check
   - 跳过 verify 后仍有 nonce/balance 检查

4. **TX Forward 延迟** (leader 专属):
   - Non-leader 每 50ms flush buffer to leader
   - Max batch 64 tx → 若 7 nodes × 2K TPS/node = 需要频繁 forward

### 3.3 Layer 1: Consensus Latency 瓶颈 (理论限制)

**当前 view time: ~1.0s (含 500ms delay)**

| 组成 | 耗时 | 可优化空间 |
|------|------|-----------|
| MIN_PROPOSE_DELAY | 500ms | → 100ms (需要更快 tx 供给) |
| Payload build | 60-90ms | → 30-50ms (减少 packing overhead) |
| Network R1 (propose→vote→PrepareQC) | ~100ms | → 50ms (LAN optimal) |
| Network R2 (PrepareQC→CommitVote→Decide) | ~100ms | → 50ms |
| **理论最小 view time** | **~250ms** | **需要 presign + delay=100ms** |

**HotStuff-2 vs HotStuff-1 对比**:
- HotStuff-1: 3 rounds (Prepare → PreCommit → Commit) = ~300ms network
- HotStuff-2: 2 rounds (Prepare → Commit) = ~200ms network ← 我们用的
- **Chained mode**: PrepareQC 搭便车在下一 view 的 Proposal → 节省 1 RTT

**BLS 签名开销**:
- sign: ~0.1ms, verify: ~1ms, aggregate: ~0.1ms
- 总 BLS 开销 per view < 10ms — 不是瓶颈

### 3.4 Layer 2: Block Capacity

**当前**: 25K tx/block, gas 22.9% of 2G

**理论上限** (gas = 2G, 100% utilization):
- ETH transfer: 2G / 21K gas = 95,238 tx/block
- 如果 block time = 1.0s → 95K TPS
- 如果 block time = 0.25s → 380K TPS (理论)

**实际限制因素**:
- Payload build time 随 tx 数量线性增长
- Block data 传播大小: 25K tx ≈ 19KB compressed — GossipSub limit 通常 1MB
- 95K tx compressed ≈ ~80KB — 仍在限制内

### 3.5 GossipSub 双路径开销 (日志-42 引入)

日志-42 修复了 QUIC 不可靠问题, 代价是:
- 每条 Vote/CommitVote 同时 direct + broadcast
- 每个 block data 同时 direct push + GossipSub
- **消息量翻倍**, 但共识层按 (view, voter) 去重
- 开销: 7 nodes × 2 msg/vote × 2 rounds ≈ 28 extra messages/view
- GossipSub mesh 放大: 每条消息传播到 D=6 peers → 28 × 6 = 168 额外网络消息
- **影响**: 在 LAN 上可忽略; 在 WAN 上需要关注

## 4. TPS 优化方案

### 方案总览

```
当前状态: 22K peak block TPS, 14K sustained injection, 1.0s block time

Phase 1: 突破注入瓶颈 ──► 目标: 40-50K sustained TPS
Phase 2: 压缩共识延迟 ──► 目标: 0.3s block time, 80K+ TPS
Phase 3: 突破 gas 上限 ──► 目标: 100K+ TPS
Phase 4: 架构级重构   ──► 目标: 200K+ TPS (长期)
```

### Phase 1: 突破注入瓶颈 (短期, 1-2 天)

#### 1a. Presign Mode 压测 (已实现, 需测试)

```
bin/n42-stress --presign 1000000 --duration 30 --accounts 3000 --batch-size 5000
```

**预期效果**:
- 移除签名热路径 → 纯 I/O blast
- 签名预计算: 3M tx × 60μs = 180s (3分钟预签名)
- 发送: 3M tx / 30s = 100K TPS injection capability
- **瓶颈将转移到 RPC latency / TX pool ingestion**

#### 1b. Direct TX Pool Injection (绕过 RPC)

当前路径: `stress → HTTP → RPC handler → tx pool`

优化路径: `stress → 共享内存 / Unix socket → tx pool`

或更激进: 在节点内集成 stress 模块, 直接调用 `pool.add_transactions()`

**实现方式**:
```rust
// 在 orchestrator 中添加内置压测模式
if std::env::var("N42_BUILTIN_STRESS").is_ok() {
    // 在 payload build 回调中直接向 pool 注入预签名 tx
    // 完全绕过 network/RPC 栈
}
```

**预期效果**: 消除 RPC latency, 注入速率仅受 pool ingestion 限制

#### 1c. Multi-RPC Load Balancing 优化

当前: 7 RPC endpoints, round-robin

优化:
- 所有 tx 发送到 leader 的 RPC (避免 TX Forward 延迟)
- Batch size 增大到 10K (减少 HTTP round-trip)
- HTTP/2 复用连接 (减少 TCP handshake)

### Phase 2: 压缩共识延迟 (中期, 3-5 天)

#### 2a. 减少 MIN_PROPOSE_DELAY: 500ms → 100ms

**前提**: Phase 1 的注入瓶颈已解决, 100ms 内能积累足够 tx

**实现**: 简单改环境变量 `N42_MIN_PROPOSE_DELAY_MS=100`

**效果**: block time 从 ~1.0s → ~0.5s → 理论 TPS 翻倍

**风险**: 如果 tx 供给不足, 会产生很多空块 → empty block skip 机制已有

#### 2b. Chained HotStuff-2 (已部分实现)

当前: PrepareQC 作为独立消息广播 (Round 1.5)
优化: PrepareQC 搭便车在下一 view 的 Proposal 中 (`piggybacked_qc`)

**现状**: 代码已支持 `proposal.prepare_qc` 字段, 但 PrepareQC 仍独立广播

**完全 chained 模式**:
- Leader 形成 PrepareQC 后, 不广播, 存入 `previous_prepare_qc`
- 下一 view 的 Proposal 携带 → followers 在处理 Proposal 时同时处理上一 view 的 QC
- **省一个 RTT** — view time 从 2 RTT → 1.5 RTT

#### 2c. Optimistic Pipelining (Leader 不等 Decide)

当前: Leader 收到 CommitQC → Decide → ViewChanged → schedule_payload_build

优化: Leader 在 PrepareQC 形成后就开始 build 下一个 payload (speculative build)

```
View N: Propose ──► PrepareQC formed ──► [开始 Build N+1, speculative]
                                          │
                         CommitQC formed ──► Commit N ──► Propose N+1 (已准备好)
```

**代码已有 speculative_build_hash 字段** — 但未启用

**风险**: 如果 view N 最终 timeout (不 commit), speculative build 的 tx 需要回滚到 pool

#### 2d. 单轮共识 (极端优化, 仅限 f=0)

在测试网 f=0 (所有节点诚实) 场景:
- 跳过 Round 2 (CommitVote), PrepareQC 即为最终承诺
- view time 从 2 RTT → 1 RTT
- **不适用于生产环境** (需要 f > 0 容错)

### Phase 3: 突破 Gas 上限 (中期, 1-2 周)

#### 3a. 增加 Gas Limit

当前: 2G gas, utilization 22.9%

方案: 提升到 5G 或 10G

**限制因素**:
- state root 计算随 state change 数增长
- compact block 已消除 follower 的 state root 瓶颈
- leader 的 state root: 25-38ms → 线性外推 95K tx ≈ ~150ms — 可接受

#### 3b. Parallel EVM (PEVM)

当前状态: PEVM 实现但比顺序执行更慢 (已知问题)

**根因**: ETH transfer 本质上是串行的 (nonce 依赖, 余额竞争)

**适用场景**: 大量独立合约调用 (DEX, NFT mint)

**优先级**: 低 — 先解决注入瓶颈

### Phase 4: 架构级重构 (长期研究)

#### 4a. Sub-blocks / Micro-blocks (Solana-style)

- Leader 在 slot 内连续发出 micro-blocks
- 每个 micro-block ~100ms, 包含 5K tx
- Slot (1s) 结束时统一投票
- **效果**: 平滑 tx flow, 消除 block 粒度的 burst

#### 4b. Multi-proposer (Narwhal/Bullshark)

- 所有 validator 同时提议 tx batch (DAG-based)
- 共识只对 DAG 排序
- **效果**: 彻底消除 leader bottleneck

#### 4c. StateRoot-Free Consensus

- 共识只对 tx ordering 达成一致
- State root 异步计算, 延迟一个 epoch finalize
- **效果**: 消除 state root 的串行依赖 → 80-100K TPS

## 5. 推荐实施路线图

```
Week 1: Phase 1a + 1b + 2a
  - Presign 压测 → 量化真实注入上限
  - Direct injection 实验 → 消除 RPC 瓶颈
  - MIN_PROPOSE_DELAY=100ms → 压缩 block time

Week 2: Phase 2b + 2c
  - 完全 chained mode → 省 1 RTT
  - Speculative build → pipeline 更深

Week 3: Phase 3a
  - Gas limit 5G → 测量 state root scaling
  - 评估是否需要 parallel state root

Week 4+: Phase 4 研究
  - Sub-blocks PoC
  - StateRoot-Free 架构设计
```

## 6. 预期效果

| 阶段 | 预期 TPS | Block Time | 关键变化 |
|------|---------|------------|----------|
| 当前 | 22K peak, 14K sustained | 1.0s | baseline |
| Phase 1 | 40-50K sustained | 1.0s | 注入瓶颈解除 |
| Phase 2 | 80-100K peak | 0.3-0.5s | 共识延迟压缩 |
| Phase 3 | 100-150K peak | 0.3s | gas headroom |
| Phase 4 | 200K+ | continuous | 架构重构 |

## 7. 实测数据

### 7.1 Slot-Aligned 模式 (2s blocks, baseline)

| Target TPS | Effective TPS | Avg Block TPS | p50 TPS | p95 TPS | Max Block TPS | Gas Util | Fail Rate |
|-----------|--------------|---------------|---------|---------|--------------|----------|-----------|
| 1,000     | 1,902        | 1,932         | 1,982   | 2,119   | 2,150        | 4.0%     | 3.7%      |
| 3,000     | 5,954        | 5,994         | 6,046   | 6,599   | 6,844        | 12.6%    | 0.0%      |
| 5,000     | 9,862        | 9,971         | 9,975   | 10,283  | 10,295       | 20.9%    | 0.0%      |
| 7,500     | 13,828       | 13,959        | 13,961  | 14,607  | 14,815       | 29.3%    | 0.0%      |
| 10,000    | 13,611       | 13,742        | 13,727  | 14,391  | 14,586       | 28.8%    | 0.0%      |
| 15,000    | 15,582       | 14,227        | 11,207  | 20,000  | 20,000       | 38.5%    | 0.0%      |
| 20,000    | 12,493       | 13,300        | 12,900  | 18,176  | 20,000       | 29.5%    | 0.0%      |

### 7.2 Fast Propose 模式 (1.0s blocks, N42_FAST_PROPOSE=1, N42_MIN_PROPOSE_DELAY_MS=500)

| Target TPS | Effective TPS | Avg Block TPS | p50 TPS | p95 TPS | Max Block TPS | Gas Util | Fail Rate |
|-----------|--------------|---------------|---------|---------|--------------|----------|-----------|
| 1,000     | 1,938        | 1,348         | 1,643   | 1,927   | 1,977        | 1.4%     | 1.9%      |
| 3,000     | 5,943        | 4,832         | 5,285   | 5,950   | 6,168        | 5.1%     | 0.0%      |
| 5,000     | 9,857        | 10,552        | 10,665  | 11,916  | 12,198       | 11.1%    | 0.0%      |
| 7,500     | 13,704       | **20,228**    | 20,184  | 22,411  | **23,660**   | 21.2%    | 0.0%      |
| 10,000    | 13,738       | **23,916**    | 24,315  | 27,010  | **28,855**   | 25.1%    | 0.0%      |
| 15,000    | 12,968       | **17,729**    | 18,285  | 22,057  | **23,999**   | 19.8%    | 0.0%      |
| 20,000    | 13,023       | **14,001**    | 11,183  | 22,918  | **26,166**   | 21.0%    | 0.0%      |
| 30,000    | 12,571       | **13,826**    | 15,288  | 20,292  | **22,019**   | 18.4%    | 0.0%      |

### 7.3 Presign Blast 测试 (500K tx, Fast Propose)

**注入性能**: 228,092 TPS (500K tx 在 2.2s 内完成 RPC 发送)

**链上性能**: avg_block_tps = 9,371, max = 18,019

**关键发现 — Pool 过载导致共识退化**:

正常时 (idle):
```
consensus_timing total=576ms, R1_collect=16ms, R2_collect=13ms
block_time ~0.55s
```

Blast 期间 (views 990-1007, pool pending 30-50K):
```
consensus_timing total=3181-5266ms, R1_collect=1071-2187ms, R2_collect=135-2324ms
block_time 2-4s
import_headroom=303-1617ms
```

**根因**: TX pool 入池操作 (nonce check, balance check, RLP decode) 在 tokio runtime 上消耗大量 CPU，
挤压了 GossipSub 消息处理 → 投票延迟 → 共识变慢 → block time 拉长 → TPS 下降

### 7.4 共识时序基准 (空载, Fast Propose)

来自 validator-0 日志:

| 指标 | 值 | 说明 |
|------|------|------|
| consensus total | 576-696ms | proposal_sent → commit_qc |
| R1_collect | 13-16ms | proposal → PrepareQC |
| R2_collect | 13-16ms | PrepareQC → CommitQC |
| proposal @time | 546-665ms | view_start → proposal_sent (**= MIN_DELAY + build**) |
| follower import | 1-9ms | compact block cache hit |
| follower commit | 26-51ms | proposal → finalize |
| block_time | ~0.55s | view start → next view start |

**关键洞察**:
- `proposal @546ms` = 500ms MIN_DELAY + ~46ms build — **delay 占 92% 的 proposal 延迟**
- `R1_collect + R2_collect = ~30ms` — 网络 + 签名 极快 (LAN)
- 实际 block time (0.55s) < consensus total (0.58s) — pipeline overlap

## 8. 修正后的优化方案

基于实测数据，原方案需要修正:

### 最高优先级: TX Pool 隔离 (解决 presign blast 退化)

**问题**: TX pool 操作在共识线程同一个 tokio runtime 上竞争 CPU

**方案**:
1. TX pool validation 使用独立 runtime (或独立线程池)
2. 或: 限制 per-tick pool ingestion 数量 (背压)
3. 或: 为共识消息使用高优先级 task

### 第二优先级: 减少 MIN_PROPOSE_DELAY (500ms → 100ms)

实测 proposal @time 546ms 中 500ms 是等待 → 改为 100ms 可将 block time 压到 ~0.2s

**注意**: 需要解决 pool 过载问题后再降 delay，否则高负载下 pool 操作会占满 100ms

### 第三优先级: Presign + 流控

Presign blast 的 228K TPS 注入率远超节点消化能力:
- 需要智能背压: 监控 pool pending，当 > 阈值时降低注入速率
- 目标: 保持 pool pending < 10K，避免共识退化

### 预期效果 (修正)

| 阶段 | 预期 TPS | Block Time | 关键变化 |
|------|---------|------------|----------|
| 当前 | 24K peak, 14K sustained | 1.0s | baseline (Fast Propose) |
| Pool 隔离 | 30-40K sustained | 1.0s | 消除 pool/共识竞争 |
| Delay 100ms | 60-80K peak | 0.2s | block time 压缩 |
| Gas 5G | 100K+ peak | 0.2s | gas headroom |
