# 开发日志-42: Regression Fix 与并行时序审计

## 背景

上一轮优化引入了两个未提交的改动导致性能退步，同时需要对并行时序图中所有阶段的耗时记录进行准确性审计。

## 发现 1: SendToValidator 策略是必要设计，非退步

### 问题现象
将 `SendToValidator` 从 "direct+broadcast" 改回 "direct-only, broadcast on failure" 后，链完全无法出块。

### 根因分析
- `send_direct()` 通过 libp2p request-response (QUIC transport) 发送
- `send_direct()` 返回 `Ok(())` 但消息可能**未实际送达**（QUIC 静默丢失）
- 结果：commit vote 丢失 → CommitQC 无法形成 → 永久 timeout
- 在 view 7 上观察到 leader 只收到 2 个 commit vote（需要 5 个 quorum）

### 决策
保持 **direct + broadcast** 双发策略：
```rust
// Direct send + broadcast for reliability
if let Some(peer_id) = self.network.validator_peer(target) {
    let _ = self.network.send_direct(peer_id, msg.clone());
}
if let Err(e) = self.network.broadcast_consensus(msg) { ... }
```
- 消息量增加 ~7x，但共识引擎按 (view, voter) 去重
- 去重成本可忽略（HashMap lookup）
- 这不是退步，是 QUIC transport 不可靠下的**必要可靠性保障**

## 发现 2: info! 日志降级为 debug!

### 修复内容
- `consensus_loop.rs`: SendToValidator 日志从 `info!` 改为不输出（直接发送无日志）
- `mod.rs`: "processing consensus message" 从 `info!` 改为 `debug!`

每秒产生 ~30-50 条日志的 I/O 开销在高 TPS 下显著。

## 并行时序审计结果

### 已准确记录的阶段

| 阶段 | 日志标记 | 准确性 |
|------|---------|--------|
| EVM 执行 | `N42_NEW_PAYLOAD_TOTAL evm_ms=0` | ✅ 准确（follower compact block cache hit = 0ms） |
| State Root | `N42_NEW_PAYLOAD_TOTAL root_ms=3-4` | ✅ 准确（follower 3-4ms, leader 0ms） |
| Payload 打包 | `N42_PAYLOAD_PACK packing_ms=X evm_exec_ms=Y` | ✅ 准确 |
| State Root (builder finish) | `N42_PAYLOAD_FINISH finish_ms=13` | ✅ 准确 |
| Compact Block 压缩/解压 | `compress_ms, ser_ms, decompress_ms, deser_ms` | ✅ 准确 |
| FCU 耗时 | `finalize fcu elapsed_ms` | ✅ 准确 |
| 共识阶段 | ViewTiming: R1_collect, R2_collect, vote_delay | ✅ delta 准确 |
| Pipeline | PipelineTiming: build_ms, import_ms, commit_ms | ✅ build_ms 使用 build_triggered_at 准确 |

### 未记录但影响小的阶段

| 阶段 | 影响 |
|------|------|
| ECDSA Recovery (pool validation) | 不在共识关键路径 |
| Compact Block 注入 (inject_compact_block) | 内存操作 <1ms |
| 网络传播延迟 (per-hop) | 可通过 ViewTiming proposal_received - proposal_sent 间接推算 |

## 压测结果

### Fast Propose + Debug 日志 (7 nodes, 2G gas)

**Step 模式 (3K TPS injection)**:
- avg_block_time: 1.0s ✅ (fast propose 生效)
- overall_tps: 2,537 (注入限制)
- avg_block_tps: 2,519
- max_block_tps: 4,738

**Blast 模式 (500K pre-signed txs)**:
- avg_block_time: 4.7s ❌ (大块导致)
- overall_tps: **13,103** (sustained)
- avg_block_tps: **12,704**
- p50_tps: 12,203
- p95_tps: 20,000
- **max_block_tps: 20,000** (80K tx @ 4.0s)
- max_tx_in_block: 80,000 (MAX_TXS_PER_BLOCK cap)
- gas_utilization: 61.2% avg, 84% peak

### 大块时序分析

80K tx 块的 leader view:
```
proposal=@3371ms    ← leader 构建+广播 3.4s
R1_collect=1917ms   ← follower 处理大块后投票
R2_collect=270ms
total=5560ms        ← 远超 1s target
```

80K tx 块的 follower view:
```
proposal=@2-3.4s    ← 网络传播延迟
vote_delay=0.8-1.7s ← 大块 import
import=210-519ms    ← compact block cache hit 但数据量大
total=3-5s
```

## 瓶颈定位

**State Root: 不是瓶颈** — follower 3-4ms, leader 0ms
**EVM: 不是瓶颈** — compact block 下 0ms
**真正瓶颈**: 大块构建时间 + 网络传播
- Leader build 80K tx: ~856ms（打包+EVM+state root）
- 网络传播 80K tx compact block: ~1-2s
- Follower import 80K tx: 200-500ms

## 优化方向

1. **降低 MAX_TXS_PER_BLOCK**: 80K → 20K-30K, 保持 block_time ≈ 1s
2. **优化 stress tool**: 修复 nonce 管理，支持持续高注入
3. **Network 层**: 大块分片传输，减少网络延迟
4. **长期**: Sub-blocks / Multi-proposer 架构
