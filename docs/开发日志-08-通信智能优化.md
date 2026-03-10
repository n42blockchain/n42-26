# Phase 3：IDC-手机通信智能优化

> 日期：2026-02-20
> 前置：Phase 1（零拷贝、超时、压缩）、Phase 2（预分帧、分级、分片）

---

## 设计决策

### Feature 3A: EWMA RTT

**问题**：累积平均 RTT（`total_rtt_ms / rtt_sample_count`）被历史数据拖尾，无法反映近期网络状态变化。一个手机初期 100ms × 1000 次后突变为 5000ms，累积平均仍显示 ~105ms。

**方案**：替换为 EWMA（指数加权移动平均），alpha = 0.3。使用 CAS 循环实现无锁更新，避免引入额外锁。

**替代方案**：滑动窗口（需要 Vec 存储）、衰减计数器（实现复杂）。EWMA 最简洁且只需一个 AtomicU64。

### Feature 3D: Arc\<MobileSession\> 免锁访问

**问题**：广播路径中每次发送成功/超时都需要 `sessions.read().await.get(&session_id)` 查 HashMap。10K 手机并发 = 10K 次读锁获取。

**方案**：`HashMap<u64, Arc<MobileSession>>` + phone handler 在握手后 clone 本地 Arc。后续操作直接用本地 Arc，不再查 HashMap。MobileSession 的字段全是 AtomicU64，天然支持 `&self` 并发访问。

### Feature 3B: 分级广播

**问题**：`broadcast::channel` 无法控制推送顺序，所有手机同时竞争写入。

**方案**：移除 `broadcast::channel`，command handler 直接遍历 `session_senders`（按 tier 排序），用 `try_send` 非阻塞推送。CacheSync 跳过 Slow 手机。

**关键决策**：
- `try_send` 替代 `send().await`：避免一个慢手机阻塞整个广播
- 排序替代 3 个优先级 channel：10K 条目 sort < 1ms，实现简单
- Per-session channel 容量 8 → 32：覆盖 ~4 分钟缓冲

### Feature 3C: Attestation 延迟指标

**问题**：无法衡量"首个 receipt 到达 → 达到 2/3 阈值"的时间。

**方案**：在 `MobileVerificationBridge` 中用 `HashMap<B256, Instant>` 跟踪首次 receipt 时间，达到阈值时计算延迟并上报 histogram。FIFO 淘汰与 `invalid_receipt_counts` 共用上限。

---

## 实施细节

### 修改文件

| 文件 | Feature | 变更 |
|------|---------|------|
| `session.rs` | 3A | `total_rtt_ms` + `rtt_sample_count` → `ewma_rtt_ms: AtomicU64`，CAS 循环 |
| `star_hub.rs` | 3B + 3D | `HashMap<u64, Arc<MobileSession>>`，移除 `broadcast::channel` + `BroadcastMsg`，有序推送，select! 2 分支 |
| `sharded_hub.rs` | 3B | 移除 `broadcast_buffer_size` 配置字段 |
| `mobile_bridge.rs` | 3C | `block_first_receipt_at` + `block_first_receipt_order`，latency histogram |

### EWMA 计算公式

```
new_ewma = (3 * sample + 7 * old_ewma) / 10
```

首个样本直接采用。存储为纯整数（ms），无浮点。

### 广播路径变更

之前：`command_rx → broadcast_tx → broadcast_rx (per handler) → QUIC write`
现在：`command_rx → iterate session_senders (sorted by tier) → try_send → per-session mpsc → QUIC write`

好处：Fast 手机优先入队，Slow 手机的 CacheSync 被跳过，command handler 不被任何单个手机阻塞。

---

## 遇到的问题

1. **`broadcast_buffer_size` 残留**：`ShardedStarHubConfig` 中引用了 `StarHubConfig.broadcast_buffer_size`，移除 broadcast channel 后需同步清理。
2. **`session_senders` 类型变更**：从 `HashMap<u64, Sender<Bytes>>` 变为 `HashMap<u64, (Sender<Bytes>, Arc<MobileSession>)>`，需要在 `SendToSession` 和 `DisconnectSession` 分支中解构 tuple。
3. **测试适配**：EWMA 行为与累积平均不同，`test_record_rtt_average` 需要替换为多个新测试（first_sample、converges、decay）；`test_tier_rapid_transitions` 中的样本数量需调整。

---

## 完成状态

- [x] Feature 3A: EWMA RTT（session.rs）
- [x] Feature 3D: Arc\<MobileSession\>（star_hub.rs）
- [x] Feature 3B: 分级广播（star_hub.rs）
- [x] Feature 3C: Attestation 延迟指标（mobile_bridge.rs）
- [x] 全量编译通过
- [x] n42-network 96 tests passed
- [x] n42-node 87 tests passed（含 3 个新测试）

---

## 后续计划（Phase 4）

- QUIC Datagram + FEC：等 quinn 支持 RFC 9221 稳定 API
- Pinned 线程 / io_uring：需 benchmark 证明瓶颈在 syscall
- 手机端负载均衡：需要手机 app 更新
- 完整出块→attestation 延迟：在 `register_block` 时传入出块时间
