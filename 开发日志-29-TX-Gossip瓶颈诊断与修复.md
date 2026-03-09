# 开发日志-29: TX Gossip 瓶颈诊断与修复

## 背景

在日志-28 中，2s slot 压测 (7 nodes, 500M gas, 8000 tx cap) 发现：
- 共识 commit 延迟在高负载时膨胀到 5-10s
- R1_collect (Round 1 vote collection) 是主要瓶颈
- 基线 TPS ceiling: ~1800 (2s slot 本应 >3000)

## 诊断过程

### Phase 1: ViewTiming 诊断系统

给 ConsensusEngine 添加了 `ViewTiming` struct，在 HotStuff-2 的每个阶段记录时间戳：

```
ViewStart → proposal_sent → R1 votes → prepare_qc_formed → R2 votes → commit_qc_formed
```

关键文件：
- `crates/n42-consensus/src/protocol/state_machine.rs` — ViewTiming struct 和 summary()
- `crates/n42-consensus/src/protocol/voting.rs` — PrepareQC/CommitQC 形成时记录
- `crates/n42-consensus/src/protocol/proposal.rs` — proposal/vote 时记录

### Phase 2: R1_collect 统计

修复前 (2s slot, tx gossip ON):
```
R1_collect: count=53 avg=272ms p50=40ms p95=1051ms p99=1841ms max=6849ms
R2_collect: avg=170ms max=8789ms
```

尖峰极端：View 210, R1_collect=6849ms, R2_collect=4355ms, total=13150ms

### Phase 3: 逐 View 时间线追踪

以 View 210 (leader=v0) 为例：

| 事件 | 时间 | 备注 |
|------|------|------|
| View start | 05:44:18.108 | |
| Slot boundary | 05:44:20.006 | delay=1908ms |
| Payload built | 05:44:20.017 | 11ms, 4165 txs |
| Direct push | 05:44:20.022 | 318KB to 6 peers |
| v4 收到 | 05:44:20.062 | **40ms** |
| v5 收到 | 05:44:20.197 | **175ms** |
| v1 收到 | 05:44:20.749 | **727ms** |
| v2 收到 | 05:44:21.589 | **1567ms** |
| v3 收到 | 05:44:25.233 | **5211ms** |
| v6 收到 | 05:44:29.739 | **9717ms** |
| Committed | 05:44:31.228 | total=13150ms |

关键发现：**同一个 318KB block data，v4 收到只用 40ms，v6 却用了 9.7s！**

### Phase 4: 根因定位

检查 v6 在 05:44:20 到 05:44:29 之间在做什么：
```
[2026-03-08T05:44:20.064374Z] WARN failed to publish transaction error=Duplicate
[2026-03-08T05:44:20.064404Z] WARN failed to publish transaction error=Duplicate
... (连续 9.7 秒无其他有意义的日志)
[2026-03-08T05:44:29.739053Z] INFO N42_DECOMPRESS: follower payload decoded
```

统计各节点的 duplicate tx warnings：
```
v0: 457,101
v1: 474,564
v2: 459,973
v3: 475,157
v4: 401,581
v5: 503,789
v6: 400,030
```

**每个节点 40-50 万条！**

## 根因

```
n42-stress 向所有 7 节点广播 tx (RPC)
→ 每个节点 tx pool 通过 GossipSub 转发
→ 7 节点互相转发 → O(n²) tx gossip 消息洪泛
→ libp2p swarm 事件队列被数万 GossipSub tx 事件填满
→ block_direct (request-response) 请求在队列中排队等待
→ follower 收到 block data 延迟数秒
→ follower 延迟投票 → R1_collect 膨胀到秒级
```

核心问题：**libp2p 的 swarm 将所有 behaviour (GossipSub + block_direct + consensus_direct) 的事件放入同一个队列，无法区分优先级**。

## 修复

### 短期修复：N42_DISABLE_TX_GOSSIP

添加环境变量 `N42_DISABLE_TX_GOSSIP`，设置后：
1. 不订阅 GossipSub 的 mempool topic（不接收 tx gossip）
2. BroadcastTransaction 命令直接跳过（不发送 tx gossip）
3. testnet.sh 默认启用此选项

文件改动：
- `crates/n42-network/src/service.rs` — 条件订阅 mempool topic + 条件跳过 BroadcastTransaction
- `crates/n42-node/src/orchestrator/mod.rs` — tx_broadcast_rx 批量处理（每次最多 64 条）
- `scripts/testnet.sh` — 默认设置 `N42_DISABLE_TX_GOSSIP=1`

### 压测结果对比 (2s slot, 8000 tx cap, single RPC)

| 指标 | 修复前 | 修复后 | 改善 |
|------|--------|--------|------|
| Effective TPS | ~1800 | **4069** | **2.3x** |
| avg_tx_per_block | ~4000 | **8000** (满载) | 2x |
| avg_block_time | 4.5-4.9s | **2.0s** | 正常 |
| bp_pauses | 4-6 | 8 | - |
| R1_collect avg | 272ms | **182ms** | 1.5x |
| R1_collect p95 | 1051ms | **222ms** | 4.7x |
| R1_collect max | **6849ms** | **283ms** | **24x** |
| R2_collect max | 5314ms | 117ms | 45x |
| duplicate tx warnings | 457,101 | **0** | 消除 |

### 共识时序（高负载，修复后）

所有 13 个 leader 视图的 R1/R2 collect：
```
R1=194 R2=74 total=2015
R1=156 R2=96 total=1971
R1=178 R2=38 total=1922
R1=198 R2=18 total=1919
R1=283 R2=50 total=2317
R1=184 R2=117 total=1987
R1=182 R2=71 total=1967
R1=174 R2=79 total=2053
R1=222 R2=34 total=2095
R1=92  R2=73 total=1868
R1=144 R2=50 total=1922
R1=162 R2=88 total=2021
R1=207 R2=94 total=2028
```

**所有 view 的 total 都在 2s 左右，没有任何异常尖峰！**

## 架构改进计划

### 问题本质

libp2p swarm 将所有 behaviour 的事件放入单一队列，无法区分优先级。当 tx gossip 活跃时，block_direct 事件被饿死。

### 长期方案对比

| 方案 | 描述 | 复杂度 | 效果 |
|------|------|--------|------|
| A: 双 swarm | block_direct 和 GossipSub 使用独立 libp2p swarm | 高 | 彻底隔离 |
| B: 原生 TCP block_direct | block data 走独立 TCP 连接，绕过 libp2p | 中 | 彻底隔离+更低延迟 |
| **C: TX Forward to Leader** | **节点收到 tx 后转发给 leader，不用 gossip** | **中** | **根治+降低网络开销** |
| D: GossipSub 限流 | 限制 mempool topic 的事件处理速率 | 低 | 缓解但不根治 |

### 推荐：方案 C (TX Forward to Leader)

设计要点：
1. 节点收到 RPC tx 后，直接通过 consensus_direct 协议转发给当前 leader
2. Leader 是唯一需要收集 tx 的节点（它构建区块）
3. O(n) 消息替代 GossipSub 的 O(n²) 洪泛
4. 可与方案 B 组合：block_direct 走原生 TCP，tx forward 走 consensus_direct
5. 最终完全移除 GossipSub mempool topic

### 当前状态

- 短期修复已实现并验证：N42_DISABLE_TX_GOSSIP 环境变量
- TPS 从 1800 提升到 4069（2.3x）
- R1_collect max 从 6.8s 降到 283ms（24x 改善）
- testnet 默认禁用 tx gossip
- 压测需使用单 RPC 模式（`--rpc http://127.0.0.1:18545`）

## 当前性能基线

| 配置 | TPS | 块时间 | gas 利用率 | R1_collect max |
|------|-----|--------|-----------|---------------|
| 4s slot, tx gossip ON | ~2000 | 4.0s | 33% | ~5000ms |
| 2s slot, tx gossip ON | ~1800 | 4.5s | 33% | 6849ms |
| **2s slot, tx gossip OFF** | **4069** | **2.0s** | **33.6%** | **283ms** |

下一步提升：
- 移除 N42_MAX_TXS_PER_BLOCK=8000 限制 → 可达 ~12000 TPS (23809 tx/block)
- 实现 TX Forward to Leader（方案 C）→ 恢复多 RPC 支持
- 缩短 slot 到 1s（需验证 import 延迟是否允许）
