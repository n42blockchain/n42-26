# 开发日志-19：TPS 优化 Phase 1 — Leader Deferred Import

> 日期：2026-03-06
> 目标：消除 leader 双重 EVM 执行，突破 1400 TPS P50 天花板

---

## 背景

日志-18 实现的优化（gas limit 500M, builder interval 50ms, 16 validation tasks）在 600 账户下
达到稳定 P50 = 1300-1400 TPS。Gas 利用率仅 20-26%，说明瓶颈不是 gas 而是 **EVM 串行执行速度**。

分析发现：sequential EVM 在 MacBook 上的执行速度约为 1400 tx/s（简单转账）。
而 leader 路径存在**双重执行问题**：
1. Payload builder 执行所有 tx（构建区块）
2. `new_payload` 再次执行所有 tx（验证导入）

---

## 设计决策

### 为什么选择 Deferred Import 而非 Parallel EVM？

1. **ROI 最高**：消除 leader 双重执行可节省 30-40% 时间，代码改动小
2. **Parallel EVM (pevm) 集成复杂度极高**：需深度修改 reth payload builder，风险大
3. **立即可用**：不依赖第三方库

### 之前的实现为什么失败？

第一版 deferred import 在高 TPS（5000+）时导致链 stall：
- `finalize_committed_block` → Case B → `import_and_notify` **同步阻塞事件循环**
- 大区块（10K+ tx）的 EVM 执行耗时数百毫秒
- 阻塞导致下一个 view 的 pacemaker timeout

### 正确的解决方案

将 `import_and_notify` **spawn 为后台任务**，通过 channel 通知完成：
- 事件循环不阻塞，共识可正常推进
- Import 完成后通过 `import_done_tx/rx` channel 通知
- Leader 等 import 完成后再 schedule next build（因为需要最新 state）

---

## 实施细节

### 改动 1：Leader 跳过 new_payload（execution_bridge.rs）

`handle_built_payload` 不再调用 `engine_handle.new_payload()`，直接序列化并广播：
```rust
async fn handle_built_payload(...) {
    // 序列化 payload
    let payload_json = serde_json::to_vec(&execution_data)?;
    // 直接广播，跳过 new_payload
    broadcast_block_data(&network, &leader_payload_tx, hash, ...);
    let _ = block_ready_tx.send(hash);
}
```

### 改动 2：后台 import channel（mod.rs）

新增 `import_done_tx/rx: mpsc::UnboundedChannel<(B256, u64, bool)>` channel 对，
在事件循环中添加接收分支。

### 改动 3：非阻塞 finalization（consensus_loop.rs）

`finalize_committed_block` Case B 从同步 await 改为 spawn 后台任务：
```rust
// 之前（阻塞！）
self.import_and_notify(broadcast).await;

// 之后（非阻塞）
tokio::spawn(async move {
    let success = Self::background_import(eh, &data, block_hash).await;
    let _ = done_tx.send((block_hash, view, success));
});
```

### 改动 4：Build 调度逻辑

- **Case A**（block 已在 reth）：直接 schedule build — 无延迟
- **Case B**（需要 import）：spawn 后台任务，`handle_import_done` 完成后 schedule build
- **Case C**（data 未到）：defer finalization，等 data 到后触发 Case B

---

## 压测结果对比（600 accounts, 7 nodes, 4s slot, 500M gas）

| Target | 之前 P50 | 之后 P50 | 提升 | 之后 P95 | Gas Util |
|--------|---------|---------|------|---------|----------|
| 2000 | 1316 | 1633 | +24% | 1750 | 26.1% |
| 3000 | 1218 | 1902 | +56% | 2306 | 42.2% |
| 5000 | 1392 | 1658 | +19% | 1980 | 41.8% |
| 7500 | 1400 | 1838 | +31% | 1838 | 39.9% |
| 10000 | N/A | 1979 | - | 3627 | 40.1% |

**关键改进**：
- P50 TPS：1300-1400 → **1650-1980**（+35%）
- P95 达到 **3627 TPS**
- Gas 利用率从 20-26% 提升到 **40-42%**
- 108 次 bg import 全部成功，0 失败，0 stall
- 链完全稳定

---

## 当前瓶颈分析

仍然是 **sequential EVM 执行速度**（~1400 tx/s 原始速率）。
Deferred import 消除了双重执行，使有效吞吐提升 35%，但单次构建仍受限于串行 EVM。

Gas 利用率 40% = ~200M / 500M，对应 ~9500 tx/block。
如果 block time 保持 4s → 2375 TPS 理论上限（与 P95 吻合）。

---

## 文件变更清单

| 文件 | 改动 |
|------|------|
| `crates/n42-node/src/orchestrator/execution_bridge.rs` | Leader 跳过 new_payload，直接广播 |
| `crates/n42-node/src/orchestrator/mod.rs` | 新增 import_done channel + 事件循环分支 |
| `crates/n42-node/src/orchestrator/consensus_loop.rs` | Case B spawn 后台 import；新增 handle_import_done + background_import |

---

## 下一步

- Phase 2：进一步提升 builder 打包效率（增加 warmup 或调整 builder 策略）
- Phase 3：Parallel EVM (pevm) 集成（需评估可行性）
- Phase 4：Pipelined consensus（构建 N+1 与共识 N 并行）
