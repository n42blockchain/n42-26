# Phase 1: IDC-手机通信快速优化

> 日期：2026-02-19
> 基于开发日志-05 的三阶段优化设计，执行 Phase 1（不改架构的快速优化）

## 设计决策

### 为什么选择这 4 个优化？

这 4 个优化解决了当前实现中最大的性能瓶颈，且风险可控：

1. **Vec\<u8\> → bytes::Bytes**：broadcast channel 使用 `tokio::sync::broadcast`，其 `Clone` trait 要求每个 receiver 都获得消息副本。`Vec<u8>` 的 clone 是 O(n) 的深拷贝，10K 手机 × 100KB 包 = ~1GB/块。`bytes::Bytes` 内部使用引用计数，clone 是 O(1)。

2. **发送超时**：之前 `open_uni + write_all + finish` 无超时保护，一个慢手机可以阻塞整个发送流程。3 秒超时在 8 秒 slot 内留足余量。

3. **zstd 压缩**：在 IDC 端压缩一次，所有手机共享压缩后的 Bytes（结合 Step 1 的零拷贝）。witness 数据包含大量重复的地址和 storage key，压缩率通常 >50%。

4. **CacheSync 定向发送**：之前每当一个新手机连接，所有已连接的手机都会收到完整的缓存同步消息。通过 per-session channel 实现定向发送，只发给新连接的手机。

### 向后兼容设计

zstd 压缩使用新的消息前缀（0x03/0x04），手机端同时支持旧前缀（0x01/0x02 未压缩）和新前缀。这确保了滚动升级的安全性。

### 超时策略

- `Ok(Err(e))` (QUIC 流错误) → 断开连接（连接已损坏）
- `Err(_)` (超时) → 跳过当前消息，保持连接（超时的 QUIC stream 在 async block 退出时自动 reset）

## 实施细节

### Step 1: Vec\<u8\> → bytes::Bytes

**依赖变更**：
- workspace Cargo.toml: `bytes = "1"`
- n42-network/Cargo.toml: `bytes.workspace = true`
- n42-node/Cargo.toml: `bytes.workspace = true`

**代码变更**：
- `HubCommand` 枚举和 `BroadcastMsg` 枚举中的 `Vec<u8>` → `Bytes`
- `StarHubHandle` 方法签名 `data: Vec<u8>` → `data: Bytes`
- broadcast handler 中 `d.as_slice()` → `d.as_ref()`
- `mobile_packet.rs` 中 encode 后转换：`bytes::Bytes::from(encoded)`
- 注意与 `alloy_primitives::Bytes` 的名称冲突，使用全路径 `bytes::Bytes::from(...)`

### Step 2: 广播发送超时

**新增常量**：`BROADCAST_SEND_TIMEOUT = 3s`

**核心逻辑**：将 `open_uni + write_all + finish` 包裹在 `tokio::time::timeout` 中。

### Step 3: zstd 压缩

**依赖变更**：
- workspace Cargo.toml: `zstd = "0.13"`
- n42-node/Cargo.toml: `zstd.workspace = true`
- n42-mobile-ffi/Cargo.toml: `zstd.workspace = true`

**新增常量**：
- `MSG_TYPE_PACKET_ZSTD = 0x03`
- `MSG_TYPE_CACHE_SYNC_ZSTD = 0x04`

**IDC 端**：
- `mobile_packet.rs` 中 encode 后 `zstd::bulk::compress(&encoded, 3)`
- CacheSync 序列化后也压缩，失败时回退到原始数据
- `MobilePacketError` 新增 `Compression` 变体

**手机端（FFI）**：
- `recv_loop` 新增 0x03/0x04 match 分支
- 0x03: `zstd::bulk::decompress` → 推入 pending_packets 队列
- 0x04: `zstd::bulk::decompress` → `bincode::deserialize::<CacheSyncMessage>` → apply_cache_sync
- 解压大小限制 16MB

### Step 4: CacheSync 定向发送

**StarHub 变更**：
- `HubCommand` 新增 `SendToSession { session_id: u64, data: Bytes }`
- `StarHub` 新增 `session_senders: Arc<RwLock<HashMap<u64, mpsc::Sender<Bytes>>>>`
- `StarHubHandle` 新增 `send_to_session()` 方法
- 连接 acceptor 创建 `mpsc::channel::<Bytes>(8)` per-session channel
- `handle_phone_connection` 签名增加 `session_rx` 和 `session_senders` 参数
- select! 循环新增第三个分支处理 session_rx

**MobileVerificationBridge 变更**：
- `phone_connected_tx: Option<mpsc::Sender<()>>` → `Option<mpsc::Sender<u64>>`
- `try_send(())` → `try_send(session_id)`

**mobile_packet_loop 变更**：
- `phone_connected_rx: mpsc::Receiver<()>` → `mpsc::Receiver<u64>`
- `hub_handle.broadcast_cache_sync(...)` → `hub_handle.send_to_session(session_id, ...)`

## 修改文件总览

| 文件 | 变更类型 |
|------|----------|
| Cargo.toml (workspace) | +bytes, +zstd 依赖 |
| crates/n42-network/Cargo.toml | +bytes 依赖 |
| crates/n42-node/Cargo.toml | +bytes, +zstd 依赖 |
| crates/n42-mobile-ffi/Cargo.toml | +zstd 依赖 |
| crates/n42-network/src/mobile/star_hub.rs | Bytes 类型、超时、zstd 常量、SendToSession、per-session channel |
| crates/n42-network/src/mobile/mod.rs | 导出新常量和 SendToSession |
| crates/n42-node/src/mobile_packet.rs | Bytes 转换、zstd 压缩、Compression 错误、定向发送 |
| crates/n42-node/src/mobile_bridge.rs | Sender\<()\> → Sender\<u64\> |
| crates/n42-mobile-ffi/src/lib.rs | zstd 解压 0x03/0x04 分支 |

## 新增测试

| 测试 | 文件 |
|------|------|
| test_bytes_clone_is_zero_copy | star_hub.rs |
| test_broadcast_send_timeout_reasonable | star_hub.rs |
| test_send_to_session_command | star_hub.rs |
| test_msg_type_constants (扩展) | star_hub.rs |
| test_zstd_roundtrip | mobile_packet.rs |
| test_zstd_witness_like_data_compresses_well | mobile_packet.rs |

## 验证结果

```
cargo build: 成功 (无新 warning)
cargo test -p n42-network --lib: 69 passed
cargo test -p n42-node --lib: 84 passed
cargo test -p n42-mobile-ffi --lib: 37 passed
```

## 后续计划

- Phase 2: 架构优化 — 预分帧 + 共享缓冲区（需要更大改动）
- Phase 3: io_uring 零拷贝写（需要 Linux 内核支持）
- 监控压缩率指标 (raw_size vs compressed_size) 以评估实际效果
