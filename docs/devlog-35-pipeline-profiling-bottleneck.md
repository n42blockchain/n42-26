# 开发日志 35 — Pipeline Profiling 与瓶颈重新定位

## 日期: 2026-03-08

## 一、问题背景

日志-34 将 gas limit 提升至 2G 后，sustained TPS 稳定在 ~14,800，gas 利用率仅 33%。
之前假设瓶颈在 reth new_payload 的 disk I/O（日志-26 显示 23K tx import = 1,401ms）。
本次通过端到端 profiling 推翻了这一假设。

## 二、Pipeline Profiling 方法

在节点日志中收集以下计时数据：
- `N42_PAYLOAD_PACK`: leader 打包 tx 的时间（packing_ms, evm_exec_ms, pool_overhead_ms）
- `N42_PAYLOAD_FINISH`: builder.finish() 时间（包含 state root 计算）
- `N42_NEW_PAYLOAD_TOTAL`: new_payload 处理时间（evm_ms, root_ms, total_ms）
- Persistence elapsed: 异步 persist 耗时

## 三、关键发现

### 3.1 Leader 构建时间极快

| 指标 | 值 | 说明 |
|------|-----|------|
| packing_ms | 32-51ms | 25-30K tx 打包时间 |
| evm_exec_ms | 23-35ms | 纯 EVM 执行时间 |
| pool_overhead_ms | 8-16ms | 池迭代 + 验证开销 |
| finish_ms | p50=25ms, p95=38ms, max=210ms | state root 计算 |
| **总构建时间** | **~60-90ms** | **2000ms slot 仅用 3-4.5%** |

Hot state cache 有效：30K tx 的 EVM 执行仅 30ms（vs in-memory benchmark 22ms，overhead 仅 36%），
说明 libmdbx 读已被 cache 消除。

### 3.2 Follower Import 几乎为零

| 指标 | 值 | 说明 |
|------|-----|------|
| evm_ms | **0** | Compact block cache hit, 跳过 EVM |
| root_ms | **3-5ms** | Synchronous state root (cached state) |
| total_ms | **3-5ms** | 全部 new_payload 处理 |

**与日志-26 的 1,401ms 对比：降低了 280-467 倍！**

原因：compact block 将 leader 的 BlockExecutionOutput 注入 payload cache，
follower 的 new_payload 直接使用缓存结果，跳过 EVM 和 state root 计算。

### 3.3 Follower Persist 不阻塞 Import

关键发现：N42 使用 `BeaconEngineMessage::NewPayload`（不是 `RethNewPayload`），
因此 **不等待 persistence 完成** 就能处理下一个块。

| Persist 指标 | 值 |
|---|---|
| p50 | 720ms |
| p95 | 1.1s |
| max | 1.2s |
| 阻塞 new_payload？ | **否** |

Persist 在独立 OS 线程异步运行，不影响出块管线。

### 3.4 Pipeline 总时序

```
Leader build:     60-90ms  │████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░│ 3-4.5%
Broadcast:        ~10ms    │░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░│ 0.5%
Follower import:  3-5ms    │░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░│ 0.15-0.25%
Consensus vote:   ~20ms    │░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░│ 1%
                           ├──────────────────────────────────────────┤
Pipeline 总计:    ~100ms   │████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░│ 5%
空闲:             ~1900ms  │░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░│ 95%
                           0ms                                    2000ms
```

**整个 pipeline 只用了 2s slot 的 5%，有 95% 的空闲余量。**

## 四、真正的瓶颈：压测工具注入速率

### 4.1 证据

压测工具在 20K TPS 目标下的数据：

| 指标 | 值 | 含义 |
|------|-----|------|
| effective_tps | 13,843 | 实际链上 TPS |
| rpc_lat_ms | 718ms | RPC 批量请求延迟 |
| avg_tx_per_block | 32,901 | 每块平均 tx |
| pool_pending | 2K-58K | 池中待处理 tx（波动大） |
| max_tx_in_block | **86,939** | 单块最高 tx！ |
| max_block_tps | **43,469** | 单块峰值 TPS |

**节点能产出 43,469 TPS 的区块**（block 444），但 sustained 只有 ~14K。

### 4.2 瓶颈分析

压测工具 v8 的 sender_loop 流程：
```
签名 batch (spawn_blocking, ~442ms) → 发送 (fire-and-forget) → sleep(batch_interval)
     ↑                                                              ↑
     └──── 阻塞：下一批签名必须等当前完成 ────────────────────────────┘
```

问题：
1. **签名串行化**：每个 sender 等待当前 batch 签名完成才能继续，sign 2000 tx = ~442ms
2. **batch_interval 额外等待**：333ms（30K TPS / 7 RPC / 2000 batch = 3 batch/s）
3. **总循环时间**：~775ms/batch → 2000tx/0.775s = 2,580 tx/s/RPC → 7 × 2580 = 18,060 tx/s max

理论极限 18K TPS 与实测 14-15K TPS 吻合（考虑 RPC 延迟、pool 排序开销）。

### 4.3 节点端无瓶颈的证明

- 单块 86,939 tx = 43,469 TPS（节点处理能力）
- Gas 利用率 91.3%（block 444）
- 当 pool 积累到 58K pending 时，leader 一次消耗大量 → 产出超大块
- Build time 仅 42-51ms → 即使 100K tx 块，估计也只需 ~150ms

## 五、Persist 实验回顾

之前尝试 `--engine.persistence-threshold 16 --engine.memory-block-buffer-target 8`：
- 效果：在 30K TPS 阶段 block 971 处 stall
- 原因：批量 persist 9 个大块耗时 3.17s，导致 receipt root task 被 abort → FCU Syncing → 无 payload_id
- 结论：保持默认 threshold=2 即可，persist 不是瓶颈

## 六、改动

本次无代码改动，纯 profiling 分析。

## 七、下一步

1. **压测工具 v9 优化**（最高优先级）：
   - Pipeline 签名和发送：sign 和 send 并行执行
   - 增大高 TPS 下的 batch_size（4000-6000）
   - 移除 batch_interval，用 channel backpressure 控制速率
   - 目标：消除 18K TPS 注入天花板，验证节点真实处理极限

2. **1s Slot Time 评估**：
   - 当前 pipeline 100ms << 1000ms，理论可行
   - 可直接翻倍 TPS（30K → 60K）

3. **高负载 Persist 监控**：
   - 当 block size > 50K tx 时，persist 时间可能超过 4s
   - 需要监控 persist 队列是否积压

## 八、指标对比总结

| 指标 | 日志-26 (baseline) | 日志-34 (2G gas) | 日志-35 (profiling) |
|------|-------|------|------|
| Sustained TPS | 5,952 | 14,820 | 14,820 (same data) |
| Follower import | 1,401ms | 57ms (avg) | **3-5ms** |
| Leader build | N/A | N/A | **60-90ms** |
| Max block TPS | 11,904 | 22,087 | **43,469** |
| Max tx/block | 23,809 | 45,116 | **86,939** |
| Gas utilization (max) | 100% (500M) | 33% (2G) | **91.3% (2G)** |
| 真正瓶颈 | EVM import | "disk I/O" | **压测注入速率** |
