# 开发日志-46: Optimistic Voting — R1 投票收集优化

> 日期：2026-03-10
> 阶段：Phase A13 — 共识 R1 延迟优化

---

## 背景

Phase A12 压测数据显示 R1_collect 平均 460ms（空载）/ 565ms（有负载），其中：
- follower 等待 new_payload 完成：~114ms
- vote_delay（等待 BlockData import 后才投票）：~363ms
- **本质问题**：投票被 import 阻塞，但 R1 vote 签署的是 `(view, block_hash)`，并不承诺区块有效性

## 设计决策

### Optimistic Voting（乐观投票）

**核心洞察**：HotStuff-2 的 R1 vote 仅表示"我见过一个合法的 Proposal"，不等同于"我验证了区块内容"。区块有效性在 finalize 阶段通过 `new_payload` + `FCU` 验证，无效时触发 view change 回滚。

**方案**：follower 收到 Proposal 后，完成以下验证即立即投票：
1. view 匹配
2. leader 身份正确
3. leader BLS 签名有效
4. justify_qc 聚合签名有效
5. HotStuff-2 安全规则（safe-to-vote）

**不再等待**：BlockData 到达、new_payload 完成、EVM 执行

**安全性论证**：
- R1 vote 签署 `signing_message(view, block_hash)` — 不包含区块内容
- Commit（R2 后 finalize）时执行 `new_payload` 验证区块
- 如果 `new_payload` 失败 → FCU 失败 → timeout → view change 恢复
- 与 Ethereum Beacon Chain 的 optimistic sync 思路一致

### 代码改动

**`proposal.rs::process_proposal()`**：
```rust
// BEFORE: 等待 import 后才投票
self.emit(EngineOutput::ExecuteBlock(proposal.block_hash))?;
if self.imported_blocks.remove(&proposal.block_hash) {
    self.send_vote(view, proposal.block_hash)?;
} else {
    self.pending_proposal = Some(PendingProposal { ... });
}

// AFTER: 立即投票
self.emit(EngineOutput::ExecuteBlock(proposal.block_hash))?;
self.send_vote(view, proposal.block_hash)?;
```

**`proposal.rs::on_block_imported()`**：简化为纯跟踪，不再触发延迟投票

**`state_machine.rs`**：4 个测试更新为验证立即投票行为

## 压测结果

### 测试配置
- 7 nodes, 2s slot, 2G gas, compact block, 48K tx cap
- presign-load 5M txs, target_tps=16000, ~290s

### R1 延迟对比

| 场景 | 优化前 | 优化后 | 改善 |
|------|--------|--------|------|
| 空载 R1_collect | ~460ms avg | **12ms p50, 14ms max** | **97% 降幅** |
| 有负载 R1_collect | ~565ms avg | **97ms p50, 202ms max, 108ms avg** | **81% 降幅** |

### Follower vote_delay（核心指标）

| 指标 | 优化前 | 优化后 |
|------|--------|--------|
| p50 | 363ms | **0ms** |
| p95 | - | **1ms** |
| max | - | **19ms** |

vote_delay 从 363ms 降到 0ms — **完全消除了 import 等待**。

### Leader R1 分布

**空载（n=24）**：min=9ms, p50=12ms, max=14ms, avg=12ms
**有负载（n=17）**：min=22ms, p50=97ms, p95=161ms, max=202ms, avg=108ms

有负载时 R1 的 ~100ms 主要是网络传播 + 大块处理导致的 GossipSub 延迟，不是投票等待。

### 整体性能

| 指标 | 本次 | Phase A12 基线 |
|------|------|---------------|
| overall_tps | 13,858 | 13,636 |
| avg_block_time | 3.2s | 2.8s |
| max_block_tps | 24,000 | 24,000 |
| avg_tx_per_block | 60,239 | - |
| gas_utilization | 46.0% | 38.6% |
| bp_pauses | 0 | 0 |

avg_block_time 3.2s（高于预期）原因：48K cap 导致更大块 + pool_pending 峰值 200K + 大块网络传播耗时增加。R1 本身已经不是瓶颈。

### R2 延迟

| 场景 | R2_collect |
|------|-----------|
| 空载 | p50=13ms, max=15ms |
| 有负载 | p50=371ms, max=644ms |

R2 有负载时较高，因为 commit vote 发送依赖 PrepareQC 传播 + follower 处理。

## 关键发现

1. **Optimistic Voting 效果显著**：vote_delay 从 363ms → 0ms，R1 空载从 460ms → 12ms
2. **有负载 R1 仍有 ~100ms**：不是投票延迟，是 GossipSub 消息传播 + CPU 竞争
3. **R2 成为新瓶颈**：有负载时 R2=371ms p50，比 R1 更慢
4. **Block size 是整体延迟主因**：48K tx/block = 更大的 compact block = 更长的网络传播

## 文件改动清单

### n42-26
- `crates/n42-consensus/src/protocol/proposal.rs` — Optimistic Voting: 立即投票，简化 on_block_imported
- `crates/n42-consensus/src/protocol/state_machine.rs` — 4 个测试更新 + PendingProposal dead_code 标注
- `scripts/testnet.sh` — MAX_TXS_PER_BLOCK 默认值 24000 → 48000
