# EIP-4844 Blob 交易支持

> 日期：2026-02-21
> 目标：以最小改动启用 EL 层 blob 支持，通过 GossipSub 独立 topic 在 IDC 节点间传播 sidecar

---

## 设计决策

### 为什么支持 EIP-4844

N42 不做 DA（数据可用性），但需要兼容以太坊生态工具（如使用 Type-3 blob 交易的 L2 工具链、ethers.js 等）。目标是 target=3 blobs/block，手机端不接触 blob。

### 为什么选择 DiskFileBlobStore

reth 提供两种 blob 存储：
- `NoopBlobStore`：直接丢弃，之前用于禁用 4844
- `DiskFileBlobStore`：磁盘存储 + 内存缓存，有 maintenance 任务自动清理

选择 DiskFileBlobStore 因为：
1. reth 的 `create_blob_store(ctx)` 一行代码创建，零配置
2. 自动清理过期 sidecar，不会无限增长
3. `EthTransactionValidator` 默认启用 KZG 验证（`set_eip4844(true)`）

### 为什么用独立 GossipSub topic

blob sidecar 不通过 `/n42/blocks/1` 传输，原因：
1. sidecar 体积大（3 blobs ≈ 384KB），与区块数据分离避免膨胀区块广播
2. sidecar 非共识关键路径，丢失不阻塞投票
3. 新 topic `/n42/blobs/1` 对旧节点透明，不影响混合版本网络

### 为什么不影响共识

- `N42Consensus` 委托 `EthBeaconConsensus`，后者已验证 `excess_blob_gas`/`blob_gas_used`
- `default_ethereum_payload()` 根据 `cancun_activated()` 自动处理 blob 打包
- `parent_beacon_block_root` 已设为 `Some(B256::ZERO)`

---

## 实施细节

### Phase 1: EL 层启用 (pool.rs)

**关键改动**：
- `NoopBlobStore` → `DiskFileBlobStore`（类型别名 + build_pool）
- 删除 `.set_eip4844(false)` —— reth 默认启用
- `blob_limit`: `{ max_txs: 256, max_size: 50MB }` —— 足够缓冲多个 slot
- 使用 `reth_node_builder::components::create_blob_store(ctx)?` 创建存储

### Phase 2: 网络层 (n42-network)

**topics.rs**: 新增 `blob_sidecar_topic()` → `/n42/blobs/1`
**handlers.rs**: `validate_message` 新增 `blob_sidecar_topic_hash` 参数，blob topic 限制 1MB
**service.rs**:
- `NetworkCommand::BroadcastBlobSidecar(Vec<u8>)` 命令
- `NetworkEvent::BlobSidecarReceived` 事件
- `NetworkHandle::broadcast_blob_sidecar()` 方法
- `NetworkService` 新增 `blob_sidecar_topic_hash` 字段，订阅 topic，路由消息
**transport.rs**: blob topic peer scoring（weight=0.3, 中等优先级）

### Phase 3: Orchestrator 集成 (orchestrator.rs + main.rs)

**BlobSidecarBroadcast 结构体**：
```rust
struct BlobSidecarBroadcast {
    block_hash: B256,
    view: u64,
    sidecars: Vec<(B256, Vec<u8>)>, // (tx_hash, RLP-encoded sidecar)
}
```

**Leader 路径**（do_trigger_payload_build 的 spawned task）：
1. 从构建的 block 中过滤 `is_eip4844()` 交易
2. 通过 `blob_store.get_all(hashes)` 获取 sidecar
3. alloy_rlp::Encodable 编码每个 sidecar
4. bincode 序列化 broadcast → `network.broadcast_blob_sidecar()`

**Follower 路径**（handle_blob_sidecar）：
1. bincode 反序列化 → BlobSidecarBroadcast
2. alloy_rlp::Decodable 解码每个 sidecar
3. `blob_store.insert(tx_hash, sidecar)` 存入本地

**main.rs 接线**：
```rust
let blob_store = full_node.pool.blob_store().clone();
orchestrator.with_blob_store(blob_store);
```

---

## 遇到的问题及解决方案

### 1. SealedBlock body 字段访问
**问题**: `SealedBlock` 的 `body` 字段是私有的
**解决**: 使用 `.body()` 方法代替直接字段访问

### 2. transactions() 返回迭代器而非切片
**问题**: `.transactions()` 返回 `impl Iterator`，不能调用 `.iter()`
**解决**: 直接链式调用 `.filter()/.map()/.collect()`

### 3. Transaction 类型推断
**问题**: `EthereumTxEnvelope<TxEip4844>` 的 `is_eip4844()`/`tx_hash()` 方法需要正确的 trait
**解决**: `Typed2718` trait（已导入）提供 `is_eip4844()`，`tx_hash()` 是固有方法无需 trait

### 4. BlobStore trait import 不需要
**问题**: `pool.blob_store()` 是固有方法，不需要 `BlobStore` trait in scope
**解决**: 从 main.rs 移除未使用的 `use reth_transaction_pool::BlobStore`

---

## 阶段完成状态

- [x] Phase 1: EL 层启用 blob 池（pool.rs）
- [x] Phase 2: 网络层 blob sidecar topic（4 文件）
- [x] Phase 3: Orchestrator 集成（orchestrator.rs + main.rs）
- [x] Phase 4: 编译验证 + 191 测试全部通过

## 改动文件清单

| 文件 | 改动类型 |
|------|----------|
| `crates/n42-node/src/pool.rs` | 修改: NoopBlobStore → DiskFileBlobStore, 启用 4844 |
| `crates/n42-network/src/gossipsub/topics.rs` | 修改: 新增 blob_sidecar_topic() + 测试 |
| `crates/n42-network/src/gossipsub/mod.rs` | 修改: 新增 re-export |
| `crates/n42-network/src/gossipsub/handlers.rs` | 修改: validate_message 新增参数 + 测试更新 |
| `crates/n42-network/src/service.rs` | 修改: Command/Event/Handle/routing + 测试 |
| `crates/n42-network/src/transport.rs` | 修改: 新增 blob topic peer scoring |
| `crates/n42-node/src/orchestrator.rs` | 修改: blob_store 字段, sidecar 广播/接收 |
| `bin/n42-node/src/main.rs` | 修改: blob_store 接线 |

## 向后兼容性

- `/n42/blobs/1` 是新 topic，旧节点不订阅也不受影响
- 旧节点不会因为块包含 blob 交易而拒绝（EVM 规则一致）
- 混合版本网络可正常运行：仅升级节点传播 sidecar
