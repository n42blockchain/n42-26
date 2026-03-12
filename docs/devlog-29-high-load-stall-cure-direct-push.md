# 开发日志 29 — 高负载链停滞根治与 Direct Push 生效

## 概述

本阶段解决了高负载下（3000+ TPS）链永久停滞的根本原因，并验证 Phase 1 Leader Direct Push 真正生效。最终实现：链在 pool>50k、连续满 gas block (23809 txs, 500M gas) 压力下稳定运行，大块传播延迟从 30+ 秒降到 4 秒。

## 修复的问题

### 1. Block Number Guard 修复 (execution_bridge.rs)

**问题**：Eager import 的 `AtomicU64` block number guard 从未触发。代码尝试通过 JSON 字段名 `blockNumber` / `block_number` 解析 block number，但 `ExecutionData` 的序列化格式不匹配，导致 fallback 到 0，guard 失效。同一 block number 的两个不同 hash 被发送到 reth `new_payload`，触发 Pipeline Sync → 永久 SYNCING。

**修复**：先反序列化为 `ExecutionData` 类型，再用 `.block_number()` 方法获取 block number，彻底避免 JSON 字段名猜测：

```rust
let execution_data: ExecutionData = serde_json::from_slice(&payload_json)?;
let block_number = execution_data.block_number();
let prev = block_guard.fetch_max(block_number, Ordering::SeqCst);
if prev >= block_number { return; } // skip duplicate
```

### 2. Eager Import 去 FCU — 消除 Canonical Chain Reorg (execution_bridge.rs)

**问题**：Eager import 同时执行 `new_payload` + `fork_choice_updated`，将 canonical chain 推进到尚未被 consensus commit 的 block。当 consensus 最终 commit 了不同的 block 时，`finalize_committed_block` 的 FCU 将 canonical head 回退（reorg），导致后续 leader 无法出块（reth 报 "head is already canonical"）。

**根因分析**：
1. View N 的 speculative build eager import Block X+1 → FCU 推进 canonical head 到 X+1
2. Consensus commit View N 的 Block X → FCU 回退 canonical head 到 X（reorg）
3. View N+1 leader 用 Block X 的 hash FCU → reth 拒绝（head 已 canonical 但不是最新）
4. 永久 retry loop → 链停滞

**修复**：Eager import（leader 和 follower）只执行 `new_payload`，不执行 `fork_choice_updated`。Block 进入 reth engine tree 但不改变 canonical chain。只有 `finalize_committed_block`（consensus commit 后）才执行 FCU。

### 3. 移除 Speculative Build (consensus_loop.rs)

**问题**：Speculative build 基于未 commit 的 block 构建下一个 block，加剧了上述 reorg 问题。

**修复**：`handle_eager_import_done` 不再更新 `head_block_hash`，不再触发 speculative build。只记录 block 已在 engine tree 中。

### 4. Direct Push Only — 消除 GossipSub 大块传播瓶颈 (execution_bridge.rs)

**问题**：`broadcast_block_data()` 同时发送 direct push 和 GossipSub fallback。GossipSub 的 mesh flooding 传播 1-4MB 大块需要 30+ 秒。两者互相竞争带宽，GossipSub 经常比 direct push 先到达（因为 fire-and-forget vs request-response），但整体都很慢。

**证据**：日志显示 875KB block:
```
20:34:56.437 - GossipSub 先到 (875KB)
20:34:56.774 - Direct push 后到 (337ms 差距)
```

**修复**：当 `direct_count > 0`（所有 validator 都通过 direct push 发送了）时，跳过 GossipSub fallback。仅在启动初期（identify 未完成、validator_peer_map 为空）时才使用 GossipSub。

## 压测结果

### 测试环境
- 7 节点本地 testnet，500M gas limit，4s slot
- `n42-stress --step --duration 180 --accounts 600 --batch-size 100 --concurrency 512 --prefill 20000`

### 对比

| 指标 | 修复前 | 修复后 |
|------|--------|--------|
| 最大稳定 TPS | ~2000 (>3000 stall) | **5952** (连续满 gas) |
| 大块传播延迟 | 30+ 秒 | **4 秒** |
| commit_latency p50 | ~4s | **4.2s** |
| commit_latency p100 | >30s | **17.6s** |
| 高负载 stall | 必然 stall | **不再 stall** |
| Pool>50k 恢复 | 永久 stall | 自动清空 |

### 典型高负载 block 序列（修复后）
```
Block  82: txs=23809  gas=500.0M  ts=...560  tps=5952  (满gas)
Block  83: txs=19927  gas=...      ts=...564  tps=4982  (4s slot!)
Block  84: txs=20964  gas=...      ts=...568  tps=5241  (4s slot!)
Block  85: txs=20069  gas=...      ts=...572  tps=5017  (4s slot!)
Block  86: txs=13411  gas=...      ts=...576  tps=3353  (4s slot!)
```

## 关键设计决策

### 为什么移除 Speculative Build？

1. **Speculative build 导致 reth canonical chain reorg**：基于未 commit 的 block 构建 + eager import FCU 会推进 canonical head，但 consensus 可能 commit 不同的 block
2. **Build 时间不是瓶颈**：payload build p50=5ms，即使没有 speculative build，4s slot 中 build 时间占比 <0.2%
3. **简化逻辑**：移除后消除了 speculative_build_hash 状态管理的复杂性

### 为什么 Eager Import 只做 new_payload 不做 FCU？

1. `new_payload` 将 block 插入 reth engine tree（验证+缓存），使后续 `finalize_committed_block` 的 FCU 能瞬间完成（Case A）
2. 不做 FCU 意味着不改变 canonical chain，避免与 consensus commit 冲突
3. 效果：finalize FCU 从 ~200ms（Case B bg import）降到 ~1ms（Case A direct FCU）

### 为什么禁用 GossipSub Fallback？

1. Direct push 已覆盖所有 validator（validator_peer_map 在 ~5s 内完成映射）
2. GossipSub 的 mesh flooding 对大块（>1MB）传播非常慢（30+ 秒）
3. 两者同时发送互相抢带宽，反而都慢
4. 保留 fallback 仅在启动初期（validator_peer_map 为空时）使用

## 各 Phase 状态

| Phase | 优化项 | 状态 | 效果 |
|-------|--------|------|------|
| 0 | 块大小限制 | ✅ 完成 | 防止死亡螺旋 |
| 1 | Leader Direct Push | ✅ **完成并验证** | 大块 30s→4s |
| stall fix | Eager import 去 FCU + guard | ✅ 完成 | 消除永久 stall |
| 2 | Delayed State Root | 未开始 | — |
| 3 | 并行 EVM | 未开始 | — |
| 4 | 全内存热状态 | 未开始 | — |

## 文件变更

- `crates/n42-node/src/orchestrator/execution_bridge.rs`
  - Follower/leader eager import: 移除 FCU，只保留 new_payload
  - Block number guard: 先反序列化 ExecutionData 再取 block_number()
  - broadcast_block_data: direct push only，跳过 GossipSub fallback
- `crates/n42-node/src/orchestrator/consensus_loop.rs`
  - handle_eager_import_done: 不再更新 head_block_hash，不再触发 speculative build
  - finalize_committed_block: rescue 逻辑改为 drain channel 匹配 hash（不依赖 head_block_hash）
  - 恢复 finalize 中 head_block_hash 的正常更新
- `crates/n42-network/src/service.rs`
  - handle_identify_event: 移除 is_trusted 限制，所有 peer 的 validator index 都被映射
