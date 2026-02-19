# 开发日志-07：IDC-手机通信架构改进 (Phase 2)

> 日期：2026-02-19
> 前置：Phase 1（开发日志-06）完成了零拷贝、发送超时、zstd 压缩、定向 CacheSync

---

## 概述

Phase 2 聚焦架构改进，目标支撑 50K+ 手机连接。实施三个 Feature：

| Feature | 效果 | 变更规模 |
|---------|------|----------|
| A. 预分帧消息 | per-phone syscall 从 2 降到 1 | 小 |
| B. 连接分级 | 可观测性 + 未来优化基础 | 中 |
| C. 多 Endpoint 分片 | 线性扩展到 50K+ | 大 |

---

## Feature A: 预分帧消息 (Pre-framed Messages)

### 设计决策

**问题**：每个手机收到广播消息时做 2 次 `write_all`（type_prefix + data），10K 手机 = 20K syscall/块。

**方案**：在 command handler 阶段将 `type_prefix + data` 预合并为单个 `Bytes`，通过 broadcast channel 传递。每个 phone handler 只需 1 次 `write_all`。

**替代方案考虑**：
- `writev` (scatter/gather IO)：QUIC 层不直接暴露，且 quinn 的 `write_all` 已做内部缓冲
- 修改线格式：增加复杂度且影响手机端，不必要

### 实施细节

1. `preframe_message(type_prefix, data) -> Bytes`：用 `BytesMut` 预分配 `1 + data.len()` 容量，put_u8 + extend_from_slice + freeze
2. Command handler 在 broadcast 前调用 `preframe_message()`
3. `BroadcastMsg::Packet/CacheSync` 现在存储已预分帧数据
4. Broadcast handler 简化为单次 `write_all(framed_data)`
5. Targeted send handler 同样单次 `write_all`（data 已在 mobile_packet.rs 中预分帧）
6. `mobile_packet.rs` 的 CacheSync 定向发送增加预分帧逻辑

### 向后兼容

线格式不变：手机仍收到 `[type_prefix | data]`，仅服务端写入方式从 2 次 syscall 变 1 次。

---

## Feature B: 连接分级 (Connection Tiering)

### 设计决策

**问题**：所有手机连接被平等对待，无法区分连接质量。

**方案**：纯增量式变更，仅增加可观测性，不改变数据路径。

**分级逻辑**：
- **Fast**: `consecutive_timeouts == 0 && avg_rtt < 2000ms`（或无 RTT 数据）
- **Slow**: `consecutive_timeouts >= 3 || avg_rtt > 5000ms`
- **Normal**: 其余

### 实施细节

`session.rs` 新增：
- `PhoneTier` 枚举（Fast/Normal/Slow），带 `as_str()` 用于 metrics 标签
- 5 个 `AtomicU64` 跟踪字段：tier, timeout_count, consecutive_timeouts, total_rtt_ms, rtt_sample_count
- 方法：`tier()`, `record_send_success()`, `record_send_timeout()`, `record_rtt()`, `avg_rtt_ms()`, `recompute_tier()`

`star_hub.rs` 集成：
- 成功发送 → `session.record_send_success()`（替代原 `record_packet_sent()`）
- 超时 → `session.record_send_timeout()` + tier metrics
- 收据处理 → 计算近似 RTT 并调 `session.record_rtt()`

RTT 计算说明：测量的是 `网络延迟(IDC→手机) + 验证时间 + 网络延迟(手机→IDC)`，不是纯网络 RTT，但作为连接质量代理指标足够。

---

## Feature C: 多 Endpoint 分片 (Multi-Endpoint Sharding)

### 设计决策

**问题**：单个 `quinn::Endpoint` 管理 10K+ 连接的瓶颈——单 UDP socket syscall 争用。

**方案**：`ShardedStarHub` 包装 N 个独立 `StarHub`，每个在不同端口运行。

**关键设计决策**：
1. **不用 trait**：避免泛型传播，所有路径统一使用 `ShardedStarHubHandle`
2. **shard_count=1 退化**：等同于原 `StarHubHandle`（只有一个 handle 的 vec 遍历）
3. **共享 SessionIdGenerator**：`Arc<AtomicU64>` 保证全局唯一 session ID
4. **fan-out send_to_session**：发送到所有 shard，不匹配的 shard 直接 no-op
5. **统一 event 流**：forwarder task 合并所有 shard 事件到单个 channel

### 实施细节

新文件 `sharded_hub.rs`：
- `ShardedStarHubConfig`：base_port, shard_count, max_connections_per_shard 等
- `ShardedStarHub`：持有 `Vec<(StarHub, u16)>`，`run()` 用 `JoinSet` 并发运行所有 shard
- `ShardedStarHubHandle`：持有 `Vec<StarHubHandle>`，broadcast 方法 fan-out 到所有 shard

`star_hub.rs` 变更：
- 新增 `SessionIdGenerator` 公共类型
- `StarHub` 新增 `shared_session_id_gen` 字段和 `new_with_shared_id_gen()` 构造器
- 连接接受器使用共享 generator（如有）或本地 counter

`main.rs` 变更：
- 从 `N42_STARHUB_SHARDS` 环境变量读取分片数（默认 1）
- 用 `ShardedStarHub::new()` 替代 `StarHub::new()`
- `data_dir` 定义提前，用于 `cert_dir`

`mobile_packet.rs` 变更：
- `hub_handle` 类型从 `StarHubHandle` 改为 `ShardedStarHubHandle`

### 配置

| 环境变量 | 默认值 | 说明 |
|---------|--------|------|
| `N42_STARHUB_SHARDS` | 1 | 分片数 |
| `N42_STARHUB_PORT` | 9443 | 基础端口（分片递增） |

---

## 修改文件总览

| 文件 | Feature | 变更 |
|------|---------|------|
| `crates/n42-network/src/mobile/star_hub.rs` | A, B, C | preframe_message(), 单次 write_all, tier 集成, SessionIdGenerator |
| `crates/n42-network/src/mobile/session.rs` | B | PhoneTier 枚举, atomic 跟踪字段, recompute_tier |
| `crates/n42-network/src/mobile/sharded_hub.rs` | C | **新文件**: ShardedStarHub/Config/Handle |
| `crates/n42-network/src/mobile/mod.rs` | B, C | 导出 PhoneTier, pub mod sharded_hub |
| `crates/n42-network/src/lib.rs` | A, C | 重导出 MSG_TYPE_CACHE_SYNC_ZSTD, ShardedStarHub 类型 |
| `crates/n42-node/src/mobile_packet.rs` | A, C | CacheSync 预分帧, handle 类型换为 ShardedStarHubHandle |
| `bin/n42-node/src/main.rs` | C | 读取 N42_STARHUB_SHARDS, 创建 ShardedStarHub |

## 测试验证

- `cargo test -p n42-network --lib`: 88 passed (含 23 个新测试)
- `cargo test -p n42-node --lib`: 84 passed
- `cargo build`: 编译成功

新增测试：
- Feature A: preframe_message_format, preframe_zero_copy, preframe_empty_data, session_id_generator, session_id_generator_thread_safe
- Feature B: phone_tier_default, tier_degrades_on_timeouts, tier_recovers_on_success, record_rtt_average, tier_slow_on_high_rtt, tier_normal_on_moderate_rtt, tier_from_u64_roundtrip, phone_tier_as_str
- Feature C: sharded_hub_config_default, sharded_hub_single_shard, sharded_hub_port_assignment, sharded_hub_broadcast_fans_out, sharded_handle_clone_cheap, session_id_uniqueness

---

## 后续计划 (Phase 3)

- 基于 PhoneTier 的优先级发送（Slow 手机延迟/跳过非关键消息）
- RTT 采样优化：加权移动平均替代全量累计
- 签名聚合优先：Fast tier 手机优先接收需要签名的消息
- 负载均衡：手机端连接多个 shard endpoint，shard 间动态迁移
