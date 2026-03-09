# 开发日志 29b — TX Forward to Leader 实现

## 设计决策

### 问题
GossipSub tx gossip 产生 O(n²) 消息放大（每个节点 re-broadcast 其他节点的 tx），导致 libp2p swarm 事件队列堵塞，
阻碍 block_direct 和 consensus_direct 协议，R1_collect 从正常 200ms 飙升到 5-10s。

### 方案选择
- **方案 A (双 libp2p swarm)**: 隔离 tx gossip 和共识/区块通道 — 复杂度高，需大量重构
- **方案 B (优先级队列)**: 在 swarm 事件处理中加优先级 — 治标不治本，仍有 O(n²) 消息
- **方案 C (TX Forward to Leader)**: 非 leader 节点直接把 tx 转发给当前 leader — O(n) 消息量 ✅

选择方案 C：
- 消息量从 O(n²) 降到 O(n)，彻底消除 tx 洪泛
- 利用已有的 request-response 框架（与 block_direct 相同模式）
- 只有 leader 需要所有 tx，其他节点不需要维护完整 mempool

### reth devp2p 兼容性
发现 reth 内置的 eth wire protocol 仍在传播 tx（与 GossipSub 独立）。
TX Forward 作为补充路径工作，未来可禁用 reth devp2p tx 传播后成为唯一路径。

## 实施细节

### 新增文件
- `crates/n42-network/src/tx_forward.rs` — request-response 协议定义
  - Protocol: `/n42/tx-forward/1`
  - Max size: 4MB
  - Request: `TxForwardRequest { txs: Vec<Vec<u8>> }` (RLP-encoded tx batch)
  - Response: `TxForwardResponse { accepted: bool }`

### 修改文件
1. **`crates/n42-network/src/lib.rs`** — 添加 `pub mod tx_forward`
2. **`crates/n42-network/src/transport.rs`** — N42Behaviour 添加 `tx_forward` behaviour
3. **`crates/n42-network/src/service.rs`**:
   - NetworkCommand: `ForwardTxBatch { peer, txs }`
   - NetworkEvent: `TxForwardReceived { source, txs }`
   - NetworkHandle: `forward_tx_batch()` 方法
   - `handle_tx_forward_event()` — 验证发送者是 validator 后 emit event
   - TX gossip 默认禁用（`N42_ENABLE_TX_GOSSIP` 环境变量启用）
4. **`crates/n42-consensus/src/protocol/state_machine.rs`** — `current_leader_index()` 方法
5. **`crates/n42-node/src/orchestrator/mod.rs`**:
   - `tx_forward_buffer: Vec<Vec<u8>>` — 批量缓冲
   - `tx_broadcast_rx` arm 改为 push 到 buffer（不再 GossipSub broadcast）
   - 50ms flush timer — 部分 batch 定时刷新
   - Buffer >= 64 时立即 flush
   - `flush_tx_forward_buffer()`:
     - 自己是 leader → 直接 feed 到 `tx_import_tx`
     - 不是 leader → 查找 leader PeerId → `network.forward_tx_batch()`
     - Leader 不在线 → 保持 buffer，超过 4096 drop oldest
   - `TxForwardReceived` handler → feed txs 到 `tx_import_tx`
6. **`scripts/testnet.sh`** — 移除 `N42_DISABLE_TX_GOSSIP`（现在默认禁用）

## 压测结果 (7 nodes, 2s slot, 500M gas, multi-RPC)

Step test — 所有 7 个 RPC 端口均匀负载:

| Target TPS | Achieved TPS | Peak TPS | Max tx/block | Gas Util |
|---|---|---|---|---|
| 500 | 494 | 500 | 2,407 | 4.1% |
| 1,000 | 930 | 950 | 1,900 | 7.6% |
| 2,000 | 1,740 | 1,750 | 3,500 | 14.3% |
| 3,000 | 2,400 | 2,413 | 4,826 | 19.7% |
| 5,000 | 3,430 | 3,500 | 7,000 | 28.2% |
| 7,500 | 4,336 | 4,430 | 8,859 | 35.6% |
| 10,000 | 4,890 | 5,167 | 10,939 | 40.2% |
| 15,000 | 5,597 | 6,656 | 13,312 | 46.0% |
| 20,000 | 5,555 | 9,044 | 23,809 | 51.3% |

- **持续 TPS: ~5,500-5,600**
- **Peak TPS: 9,044**（23,809 tx/block, 500M gas 上限）
- 链全程稳定，无 stall，R1_collect ~200ms
- 对比上次结果 (4,069 TPS with tx gossip disabled, single RPC): **+37% 持续 TPS**

## Metrics
- `n42_tx_forward_batches` / `n42_tx_forward_txs` — 发送端计数
- `n42_tx_forward_local` — leader 自身 tx 直接入池计数
- `n42_tx_forward_dropped` — buffer 溢出丢弃计数
- `n42_tx_forward_batches_received` / `n42_tx_forward_txs_received` — 接收端计数
- `n42_tx_forward_imported` — 导入本地池计数
- `n42_tx_forward_batches_sent` / `n42_tx_forward_txs_sent` — network service 发送端计数

## 后续优化
- 禁用 reth devp2p tx 传播（让 TX Forward 成为唯一路径，进一步减少网络开销）
- Phase 2.5: Delayed State Root — 从关键路径移除 state root 计算
- Phase 3: Hot state cache — 解决 reth import disk I/O 瓶颈
