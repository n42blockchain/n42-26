# 开发日志-50: Cache Hit Fast Path — 90K TPS 恢复

> 日期：2026-03-11
> 阶段：性能回归修复

---

## 背景

审计优化后的代码在 7 节点测试网上 90K cap 测试仅跑出 ~45K TPS（avg_block_time=1.85s），
远低于审计前同配置的 **91,214 TPS**（avg_block_time=1.0s）。

## 根因分析

### 问题定位

在 `payload_validator.rs` 的 `validate_block_with_state` 函数中，即使 payload cache 命中
（EVM 0ms）且 state root 延迟（0ms），仍然执行三个昂贵的 CPU 操作：

| 操作 | 耗时 | 说明 |
|------|------|------|
| `convert_to_block` | ~300-400ms | 90K tx 的 RLP 解码 |
| `calculate_tx_root` | ~200-300ms | Merkle 根计算 |
| `hashed_post_state` | ~200-400ms | keccak256 哈希 |
| **合计** | **~920ms** | 与 np_elapsed=~1000ms 吻合 |

### 级联效应（FAST_PROPOSE 模式）

```
FCU 等待 new_payload (~1000ms)
→ Leader 必须等 FCU 完成才能开始构建下一个区块
→ 提案延迟 ~1400ms
→ 每个区块时间 ~1.85s（远超 1.0s 目标）
```

### 为什么审计前更快

审计前使用 **固定 slot 模式**（非 FAST_PROPOSE），FCU 与 slot 计时器并行运行，
不在关键路径上。FAST_PROPOSE 模式下 FCU 成为关键路径瓶颈。

## 解决方案：Cache Hit Fast Path

在 `payload_validator.rs` 中添加快速路径：当 `is_cache_hit && n42_skip_root` 时，
跳过所有昂贵的后验证操作。

### 修改文件

- `reth/crates/engine/tree/src/tree/payload_validator.rs`

### 核心逻辑

```rust
// N42 fast path: cache hit + skip/defer state root → skip ALL expensive post-validation.
if is_cache_hit && n42_skip_root {
    let block = convert_to_block(input)?;
    let block = block.with_senders(senders);
    // Skip: calculate_tx_root, hashed_post_state, validate_post_execution
    // Return immediately with empty trie updates
    ...
}
```

跳过的操作：
- `calculate_tx_root`（Merkle 根 ~200-300ms）
- `hashed_post_state`（keccak256 ~200-400ms）
- `validate_post_execution`（等待上述完成）

保留的操作：
- `convert_to_block`（仍需要 SealedBlock 对象）

### 安全性

- 仅在 Leader 已验证的区块上生效（cache hit 说明 Leader payload builder 已执行完整验证）
- `n42_skip_root` 必须显式启用（环境变量 `N42_SKIP_STATE_ROOT` 或 `N42_DEFER_STATE_ROOT`）
- Follower 信任 Leader 的验证结果，符合 HotStuff-2 共识模型

## 测试结果

### 修复后（2026-03-11）

```
BLOCK_ANALYSIS blocks=50 total_tx=4456500 avg_block_time="1.0s"
  overall_tps="90,949" avg_block_tps="89,112"
  p50_tps="90,000" p95_tps="90,000"
  max_tx_in_block=90000
  gas_utilization="93.6%"
```

- Fast path 命中次数：5,101
- Follower np_elapsed：0ms（从 ~1000ms 降到 0ms）

### 对比

| 指标 | 审计后（修复前） | 审计后（修复后） | 审计前基线 |
|------|-----------------|-----------------|-----------|
| overall_tps | ~45,000 | **90,949** | 91,214 |
| avg_block_time | 1.85s | **1.0s** | ~1.0s |
| np_elapsed (follower) | ~1000ms | **0ms** | - |
| fast path hits | 0 | 5,101 | - |

### 测试环境

- 7 节点本地测试网（同一台机器）
- N42_FAST_PROPOSE=1, N42_DEFER_STATE_ROOT=1, N42_SKIP_TX_VERIFY=1
- N42_MAX_TXS_PER_BLOCK=90000, N42_INJECT_HIGH_WATER=90000
- TCP 注入，5M 预签名交易，batch_size=500

## 结论

Cache Hit Fast Path 成功将 TPS 从 ~45K 恢复到 **90,949**，与审计前 91,214 基本一致。
核心思路：Leader 已完整验证的区块，Follower 无需重复昂贵的后验证操作。
