# 开发日志-24：Pipeline 时钟化与全阶段计时

> 日期：2026-03-06
> 目标：完善 Speculative Build，修复编译问题，实现 Pipeline 时钟化架构

---

## 核心设计思想

**共识出块间隔 = 总线主频时钟**

共识有固定的出块间隔（slot_time），这个时间窗口不应该被浪费在等待上，
而是作为"时钟周期"并行安排所有工作：

```
Slot Clock Tick ─────────────────────────────────────────── slot_time (1s)
├── [Build]         tx_pack + evm_exec + state_root   (~7ms)
├── [Broadcast]     block data + blob sidecars         (~5ms)
├── [Consensus]     2-round voting (HotStuff-2)        (~50-200ms)
├── [Import]        new_payload + fcu                   (~100ms, 与 Consensus 并行)
└── [Commit]        finalize                            (~1ms)

并行关系:
  Build N+1  ←→  Consensus N  ←→  Import N  (三者同时进行)
```

---

## 变更总结

### Phase 1: 编译修复与基础诊断

#### 1.1 修复 `with_build_time_budget` 编译错误

`EthereumBuilderConfig::with_build_time_budget()` 在当前 reth 中不存在。
`cargo check` 通过但 `cargo test` 失败。移除该调用，build time budget 由 reth 内部管理。

#### 1.2 交易池深度诊断

在 `N42InnerPayloadBuilder::try_build()` 中：
- 构建前：记录 `pool_pending` / `pool_queued` 到 metrics gauge
- 构建后：新增 `gas_pct`（gas 利用率百分比）

| 指标 | 含义 | 诊断用途 |
|------|------|---------|
| `n42_pool_pending_at_build` | 构建时池中 pending 交易数 | 低=发送端瓶颈 |
| `n42_pool_queued_at_build` | 构建时池中 queued 交易数 | 高=nonce gap |
| `n42_payload_tx_count` | 实际打包交易数 | 与 pending 对比看效率 |
| `gas_pct` | gas 利用率 | 接近 100%=gas limit 瓶颈 |

#### 1.3 Clippy 清理

- 合并嵌套 `if` 为 `if && let Some()` 模式
- 添加 `#[allow(clippy::too_many_arguments)]`

### Phase 2: Pipeline 时钟化 — 全阶段计时

#### 2.1 `PipelineTiming` 结构

```rust
pub(crate) struct PipelineTiming {
    created: Instant,                   // 首次知晓此区块
    is_leader: bool,                    // Leader/Follower 角色
    build_complete: Option<Instant>,    // 构建/接收完成
    import_complete: Option<Instant>,   // eager import 完成
    committed: Option<Instant>,         // 共识 commit 完成
}
```

#### 2.2 计时点

| 事件 | 记录点 | Leader | Follower |
|------|--------|--------|----------|
| 创建 | `block_ready_rx` / `handle_block_data` | Build 触发 | Block data 收到 |
| Build Complete | `block_ready_rx` | Payload 构建完成 | Block data 解析完成 |
| Import Complete | `eager_import_done_rx` / `import_done_rx` | Eager import 成功 | Follower eager import 成功 |
| Committed | `handle_block_committed` | Consensus commit | Consensus commit |

#### 2.3 输出格式

每个 commit 输出一行 pipeline 摘要：
```
INFO view=42 block=0x1234.. pipeline="role=L build=12ms import=95ms commit=182ms import_headroom=87ms"
```

- **import_headroom**: import 在 commit 之前完成的余量。越大说明 import 不在关键路径上。
  - headroom > 0：并行成功，import 先于 commit 完成
  - headroom = 0 或 `-`：import 是瓶颈

#### 2.4 Metrics Histograms

| Metric | 含义 |
|--------|------|
| `n42_pipeline_build_ms` | Build 阶段耗时 |
| `n42_pipeline_import_ms` | Import 阶段耗时（含 EVM 重执行 + state root） |
| `n42_pipeline_total_ms` | 全 pipeline 端到端耗时 |
| `n42_pipeline_parallelism` | 并行度 = (build + import) / total, >1.0 = 有效重叠 |

---

## 并行关系图

```
时间轴 →

Block N (Leader):
  ├── Build ──┤
  │           ├── Broadcast ──┤
  │           │               ├── Consensus Voting ──────────┤
  │           │               │                               │
  │           ├── Eager Import ─────────┤                     │
  │           │                         │                     │
  │           │                         ↓ import_headroom     │
  │           │                         │←────────────────────┤ Commit
  │           │                                               │
  │           └── Speculative Build N+1 starts ──────→        │

Block N (Follower):
  ├── Receive Data ──┤
  │                  ├── Consensus Voting ──────────────┤
  │                  │                                   │
  │                  ├── Follower Eager Import ─────┤    │
  │                  │                              │    │
  │                  │                              ↓    │
  │                  │                  import_headroom   │ Commit
  │                  │                                   │
  │                  └── Speculative Build N+1 ──→       │
```

---

## 瓶颈判断决策树

```
部署后运行 stress test，查看日志：

1. pool_pending < 100 at build time?
   → YES: 发送端瓶颈。增大 batch_size / accounts / concurrency

2. gas_pct > 95%?
   → YES: Gas limit 瓶颈。增大 gas limit

3. pipeline build > 100ms?
   → YES: EVM 顺序执行慢。考虑并行 EVM

4. import_headroom = "-" 或 < 0?
   → YES: Import 是关键路径。需要异步 state root

5. pipeline total > slot_time?
   → YES: Slot 时间不够。考虑增大 slot_time 或深度优化

6. parallelism < 1.0?
   → YES: 并行不充分。检查哪个阶段串行阻塞
```

---

## 改动文件

| 文件 | 改动 |
|------|------|
| `payload.rs` | 移除 with_build_time_budget，添加 pool depth + gas_pct 诊断 |
| `orchestrator/mod.rs` | 新增 `PipelineTiming` 结构 + `record_pipeline_timing()` + pipeline_timings 字段 |
| `orchestrator/consensus_loop.rs` | commit 时输出 pipeline 摘要 + emit metrics；bg import 记录时间 |
| `orchestrator/execution_bridge.rs` | follower handle_block_data 记录 pipeline timing |

---

## 后续优化方向

### 基于数据驱动的下一步

1. **部署 7 节点测试网**，运行 `n42-stress --step --prefill 20000`
2. **收集 pipeline 日志**，分析瓶颈
3. 根据瓶颈类型选择：
   - 发送端：优化 stress test
   - EVM：并行 EVM 集成
   - Import：异步 state root
   - Gas：调整 gas limit

### 长期：Pipeline 深度并行

- **Pre-warm TX iterator**：在上一个区块 commit 后立即开始扫描 pool
- **Async State Root**：state root 计算不在 build 关键路径上
- **Deferred Execution**：follower 不重复执行交易，只验证 state root
