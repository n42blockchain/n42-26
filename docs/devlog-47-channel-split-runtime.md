# 开发日志-47: Channel Split 与 TX Pool Bridge 独立 Runtime

## 背景

在完成 Optimistic Voting (OV) 后，R1 从 460ms 降至 12ms，但 R2 仍为 369ms，成为新瓶颈。
分析发现根因是 CPU 竞争：ConsensusMessage 和 TxForwardReceived 共享同一个 `net_event_rx` 通道（容量 8192），
在 20K+ TPS 下，数千 TX 事件排在共识消息前面。

## 设计决策

### Phase 1: Channel Split

**方案**：将 NetworkEvent 拆分为两个通道：
- `consensus_event_rx`（容量 2048）：ConsensusMessage + BlockAnnouncement
- `net_event_rx`（容量 8192）：TxForwardReceived, SyncRequest, PeerConnected 等

**关键教训**：第一版将 BlockAnnouncement 放在低优先级通道，导致严重退步（avg_block_time 从 2.0s 到 4.2s）。
BlockAnnouncement 是 follower import pipeline 的关键消息，必须与 ConsensusMessage 同等优先级。

**文件修改**：
- `crates/n42-network/src/service.rs`: 新增 `consensus_event_tx` 字段，`emit_event()` 按事件类型路由
- `bin/n42-node/src/main.rs`: NetworkService::new() 返回 4-tuple
- `crates/n42-node/src/orchestrator/mod.rs`: 新增 `consensus_event_rx` 字段，biased select 中 P3=共识通道，P5=数据通道

### Phase 2: TX Pool Bridge 独立 Runtime

**方案**：将 TxPoolBridge 从主 tokio runtime 迁移到独立的 multi-thread runtime（2 worker threads）。

**原理**：即使 channel 已拆分，TX pool 的 `add_external_transaction()` 操作（签名验证、nonce 检查）仍在主 runtime 上执行，
tokio 任务调度器级别的竞争导致共识消息处理延迟。独立 runtime 彻底隔离这些 CPU 密集操作。

**文件修改**：
- `bin/n42-node/src/main.rs`: TxPoolBridge 改为 `std::thread::spawn` + 独立 `tokio::runtime::Builder::new_multi_thread()`

## 压测结果

### 测试条件
- 7 节点 macOS, 2s slot, 24K tx cap, 12K TPS 限速, 120s duration
- blast mode, 7 RPC endpoints

### Channel Split Only
| 指标 | 基线 | Channel Split |
|------|------|---------------|
| overall_tps | 11,536 | 11,238 (-2.6%) |
| avg_block_time | 2.0s | 2.0s |
| R2 p50 | ~369ms | ~250ms (-32%) |

### Channel Split + 独立 Runtime
| 指标 | 基线 | 最终结果 |
|------|------|----------|
| overall_tps | 11,536 | **12,223 (+6%)** |
| avg_block_time | 2.0s | 2.0s |
| R2 p50 | ~369ms | **~221ms (-40%)** |
| R2 p95 | ~600ms | ~594ms |
| sustained_tps | - | 11,881 |
| drain_ms | - | 18,016ms |

### 关键观察
- R2 从 369ms 降至 221ms（p50），改善 40%
- overall_tps 从 11,536 提升至 12,223，+6%
- 仍有偶发尖刺（Node 6: R1=5622ms），可能是系统级 CPU 突发
- BlockAnnouncement 必须与 ConsensusMessage 同通道，否则 follower import 严重延迟

## 架构变更总结

```
Before:
  NetworkService → net_event_rx (8192) → Orchestrator select!{biased}
  TxPoolBridge → main tokio runtime

After:
  NetworkService → consensus_event_rx (2048) [ConsensusMsg + BlockAnnouncement]
                 → net_event_rx (8192) [TX, Sync, Peers]
  Orchestrator select!{biased}:
    P3: consensus_event_rx (high priority)
    P5: net_event_rx (lower priority)
  TxPoolBridge → dedicated tokio runtime (2 threads)
```
