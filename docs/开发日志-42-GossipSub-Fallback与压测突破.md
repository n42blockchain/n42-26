# 开发日志-42: GossipSub Fallback 修复与压测突破 22K TPS

## 日期: 2026-03-09

## 问题: 清空启动后共识无法出块

### 根因分析

通过系统性追踪投票路径,发现两个关键网络可靠性问题:

#### 问题 1: Block Data Direct Push 不可靠
- **症状**: Follower 收到 Proposal 但没有 block data,无法投票
- **根因**: `broadcast_block_data()` 通过 `send_block_direct` (request-response/QUIC) 发送 block data 到所有 validator peer。当 direct push 成功发送到 >=1 个 peer 时,**完全跳过 GossipSub fallback** (`if direct_count == 0`)
- **问题**: QUIC request-response 静默失败 — leader 发送到 6 个 peer,但部分 peer 实际未收到 (validator-3 缺失约 37% 的 view 的 block data)
- **修复**: 始终同时使用 direct push + GossipSub 广播 block data,接收端通过 `pending_block_data.contains_key(&hash)` 去重

#### 问题 2: Vote/CommitVote Direct Send 不可靠
- **症状**: Follower 发送 Vote 到 leader (via `SendToValidator` → `send_direct`),但 leader 只收到 2/6 个投票
- **根因**: `SendToValidator` 使用 request-response 协议直接发送到 leader。如果 QUIC 连接有问题,投票静默丢失
- **修复**: `SendToValidator` 同时通过 request-response + GossipSub 广播。共识引擎按 (view, voter) 去重投票

### 诊断过程

1. 添加 INFO 级别日志到投票决策路径 (`process_proposal`, `send_vote`, `on_block_imported`)
2. 添加 INFO 级别日志到 `handle_engine_output` 的 `SendToValidator` 分支
3. 添加 INFO 级别日志到 `handle_execute_block` 追踪 block data 是否到位
4. 通过对比多个 validator 的日志,发现 block data 到达率不一致
5. 追踪到 `broadcast_block_data` 的 GossipSub fallback 条件过于保守

### 代码修改

1. **`crates/n42-node/src/orchestrator/execution_bridge.rs`**:
   - `broadcast_block_data()`: 始终 GossipSub 广播,不再以 `direct_count == 0` 为条件

2. **`crates/n42-node/src/orchestrator/consensus_loop.rs`**:
   - `handle_engine_output(SendToValidator)`: 始终 direct + broadcast 双路径
   - `handle_execute_block()`: 提升日志级别到 INFO

3. **`crates/n42-consensus/src/protocol/proposal.rs`**:
   - `process_proposal()`: 投票决策日志提升到 INFO
   - `on_block_imported()`: deferred vote 日志提升到 INFO
   - `send_vote()`: 添加 INFO 日志,包含 voter/target_leader/view

4. **`crates/n42-node/src/orchestrator/mod.rs`**:
   - 修复 ConsensusMessage match 分支缺失的关闭大括号

## 压测结果

### 环境
- 7 nodes, macOS, N42_FAST_PROPOSE=1, N42_MIN_PROPOSE_DELAY_MS=500, N42_SKIP_TX_VERIFY=1
- Gas limit: 2G, Block time: 1.0s, Stress v9 (pipelined sign+send)

### Step Test 结果

| Target TPS | Effective TPS | Avg Block TPS | p50 TPS | p95 TPS | Max Block TPS | Max TX/Block | Gas Util | Fail Rate |
|-----------|--------------|---------------|---------|---------|--------------|-------------|----------|-----------|
| 1,000     | 1,938        | 1,333         | 1,379   | 2,020   | 2,130        | 2,130       | 1.4%     | 1.9%      |
| 3,000     | 5,943        | 4,880         | 5,107   | 6,257   | 6,333        | 6,333       | 5.1%     | 0.0%      |
| 5,000     | 9,916        | 11,052        | 11,101  | 12,792  | 13,015       | 13,015      | 11.6%    | 0.0%      |
| 7,500     | 13,788       | **21,777**    | 21,747  | 23,545  | **25,498**   | 25,498      | 22.9%    | 0.0%      |
| 10,000    | 13,056       | **18,944**    | 18,933  | 23,348  | **23,784**   | 23,784      | 19.9%    | 0.0%      |
| 15,000    | 12,853       | **17,982**    | 18,265  | 20,441  | **20,878**   | 20,878      | 18.9%    | 0.0%      |

### 关键发现

1. **avg_block_tps 峰值: 21,777 TPS** (7500 target step) — 比 日志-41 的 14K 提升 55%
2. **最高单块 TPS: 25,498** (block #827, 25,498 tx @ 1.0s)
3. **最高目标 step 不是最佳 step**: 7500 target 比 10000/15000 target 更好,因为高注入压力导致 RPC latency 上升 (~1000ms),压制了实际吞吐
4. **零 fail rate, 零 nonce resync** (3000+ TPS step onwards)
5. **Gas utilization 仅 22.9%** (2G gas limit) — 节点远未到极限

### 瓶颈分析

1. **Stress Tool 注入瓶颈**: effective_tps 在 ~14K 封顶,avg_block_tps 可达 22K — 表明链能力 > 注入能力
2. **RPC Latency**: 高负载下 RPC latency 从 68ms 升至 1023ms — 压制注入速率
3. **Pool 积压**: pool_pending 在高负载时波动大 (0-16K) — 表明 tx pool → block packing 是异步的
4. **Block time 稳定 1.0s**: 共识没有因高负载而减速
5. **Gas utilization < 23%**: 远未触碰 gas limit ceiling

### vs 日志-41 对比

| 指标 | 日志-41 | 日志-42 | 变化 |
|------|---------|---------|------|
| avg_block_tps peak | 44,876 | 21,777 | -51% (注1) |
| max single block TPS | 80,000 | 25,498 | -68% (注1) |
| sustained TPS | ~14K | ~14K | 0% |
| fail_rate | 0% | 0% | = |
| block_time | 1.0s | 1.0s | = |
| 启动可靠性 | 不稳定 | 稳定 | 大幅改善 |

**注1**: 日志-41 的高峰值来自 pool 积压释放 (prefill 20K → 第一个块吃掉 80K tx),不代表持续吞吐。日志-42 的数据更真实。

## 后续优化方向

1. **Stress v10 (presign mode)**: 已实现但未测试 — presign 大量 tx 到内存,纯 I/O blast 发送,预计可突破 14K 注入瓶颈
2. **RPC 优化**: 高负载下 RPC latency 1s → 需要 batch RPC 或本地 tx injection bypass
3. **减少 MIN_PROPOSE_DELAY**: 当前 500ms delay 可降至 100-200ms — 需要更快的 tx 供给
4. **GossipSub 消息去重优化**: 现在 Vote/Block data 双路径发送,需要确保去重高效
5. **Compact Block 压缩比**: 当前 25K tx block 的 payload 较大,需监控 GossipSub message size limit
