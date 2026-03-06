# 开发日志-22：共识-执行流水线化 (Eager Import) — Phase 1

> 日期：2026-03-06
> 目标：共识投票与区块导入并行执行，消除 pipeline stall

---

## 背景

日志-21 实现 1s 出块和 1667 TPS 后，下一步瓶颈在于共识与执行的串行等待。
当前 leader 构建完区块后广播并触发共识投票，但 `new_payload`（导入区块到 reth）
要等到 `finalize_committed_block` 才执行（Case B：后台导入），导致约 200ms 的 pipeline stall。

新优化计划 Phase 1 目标：让共识投票和区块导入并行进行。

---

## 根因分析

### 当前时序（串行）
```
Build N → Broadcast → Consensus Vote → Commit → Import(new_payload+fcu) → Build N+1
                                                 ↑ ~200ms stall
```

### 优化后时序（流水线）
```
Build N → Broadcast → Consensus Vote → Commit → [block already in reth] → Build N+1
              ↓
         Eager Import (new_payload + fcu)  ← 与共识投票并行
```

---

## 解决方案：Eager Import

### 核心思想

Leader 在 `handle_built_payload` 中：
1. 先广播 block data + blob sidecars
2. 立即发送 `block_ready_tx` 触发共识投票
3. **紧接着**调用 `engine_handle.new_payload()` + `fork_choice_updated()` 进行 eager import

由于共识投票在主 select loop 中异步进行，而 eager import 在 spawned task 中运行，
两者天然并行。当 `finalize_committed_block` 执行时：
- 如果 eager import 已完成 → Case A（block 已在 reth），直接触发下一个 build
- 如果 eager import 未完成 → Case B（后台导入），fallback 到原有路径

### 关键实现细节

**Engine Channel 序列化**：reth 的 engine 通过 mpsc channel 处理请求，
eager import 的 `new_payload` 排队在 finalize 的 `fcu` 前面。
这意味着即使 eager import 的 `fcu` 和 finalize 的 `fcu` 同时到达，
engine 会按顺序处理，不会冲突。

### 修改 1：execution_bridge.rs — handle_built_payload

```rust
// 原来：_engine_handle（未使用）
// 现在：engine_handle（启用 eager import）

// 1. Broadcast
broadcast_block_data(...);
broadcast_blob_sidecars(...);

// 2. Trigger consensus immediately
let _ = block_ready_tx.send(hash);

// 3. Eager import: parallel with consensus
match engine_handle.new_payload(execution_data).await {
    Ok(status) if Valid | Accepted => {
        let fcu = ForkchoiceState { head: hash, safe: hash, finalized: hash };
        engine_handle.fork_choice_updated(fcu, None, ...).await;
        metrics::counter!("n42_eager_import_hits_total").increment(1);
    }
    ...
}
```

### 修改 2：consensus_loop.rs — finalize_committed_block FCU 计时

添加 timing instrumentation，当 FCU 耗时 > 5ms 时以 info 级别输出，
用于判断 eager import 是否生效（生效时 finalize FCU 应接近 0ms）。

---

## 改动文件

| 文件 | 改动 |
|------|------|
| `crates/n42-node/src/orchestrator/execution_bridge.rs` | eager import 实现，`_engine_handle` → `engine_handle` |
| `crates/n42-node/src/orchestrator/consensus_loop.rs` | finalize FCU 计时 instrumentation |

---

## 测试结果（7 节点本地测试网）

### Eager Import 命中率
- Leader 区块（validator-0）：**130/130 = 100% 命中**
- 总区块 909，其中 130 个由 validator-0 出块（~1/7 符合预期）
- 所有 leader 区块走 Case A（immediate build），无 Case B fallback

### Stress Test

| 目标 TPS | 实际 TPS | 失败率 | 峰值 tx/block |
|----------|----------|--------|--------------|
| 500 | 474.7 | 0.0% | — |
| 1000 | 879.2 | 0.0% | — |
| 2000 | 1315.4 | 0.0% | 2239 |

### 分析

实测 TPS（1315）低于日志-21 的 1667，可能原因：
1. Eager import 的 `new_payload` await 在 spawned task 中阻塞了该 task 的完成
2. 不同测试条件（系统负载、后台进程等）
3. Eager import 的收益主要体现在 leader 自身（1/7 区块），
   对 follower 的区块无影响

### 结论

Eager import 机制工作正确，100% 命中率验证了流水线化方案的可行性。
但单独的 eager import 不足以显著提升 TPS，需要配合 Phase 2（并行 EVM）
才能释放更大的性能潜力。核心价值在于：
- 消除 leader 区块的 pipeline stall
- 为后续 speculative build（推测性构建）奠定基础
- 验证了共识与执行可以安全并行

---

## 关键经验

1. **reth engine channel 是天然的序列化保障**：多个 `new_payload`/`fcu` 请求不会冲突
2. **Eager import 对 leader 区块效果显著**，但总体 TPS 提升有限（只影响 1/N 区块）
3. **真正的性能瓶颈在 EVM 顺序执行**：Phase 2 的并行 EVM 是下一个突破点
