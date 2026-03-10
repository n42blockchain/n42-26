# 开发日志-13：手机验证模拟与奖励分发

> 日期：2026-02-22
> 阶段：手机验证端启用 + 奖励机制

---

## Phase 1: QUIC 客户端模块提取

### 设计决策
- 从 `tests/e2e/src/scenarios/scenario8_mobile_evm.rs` 提取 QUIC 客户端代码到 `crates/n42-mobile/src/quic_client.rs`
- 包含 `QuicMobileClient` 结构体（connect/receive_packet/send_receipt/close）
- 保留 `SkipServerVerification` TLS 验证器（StarHub 使用 rcgen 自签证书）
- 添加为 `n42-mobile` crate 的公共模块

### 实施细节
- `receive_packet()` 使用总体 deadline 超时（而非每次 accept_uni 独立超时），防止 CacheSync 消息无限循环
- 消息类型区分：0x01 = VerificationPacket（返回），0x02 = CacheSync（跳过继续）
- 10MB 读取上限防止恶意数据

## Phase 2: 手机模拟器二进制

### 新建 `bin/n42-mobile-sim/`
- CLI：`--starhub-ports` (逗号分隔), `--phone-count`, `--duration`
- 确定性 BLS 密钥：`key_gen(keccak256("n42-mobile-key-{i}"))`
- 每个手机的事件循环：receive → decode → verify_block → sign_receipt → send
- Phone-to-port 映射：`ports[phone_idx % ports.len()]`

## Phase 3: 奖励管理器 (MobileRewardManager)

### 关键设计决策

**奖励机制选择：EIP-4895 Withdrawals**
- 不需要签名交易、gas 计算、nonce 管理
- reth 原生支持——block executor 自动处理 withdrawals 为余额增加
- `PayloadAttributes.withdrawals` 已设为 `Some(vec![])`，只需填入实际数据

**BLS pubkey → ETH 地址映射**：
- `keccak256(bls_pubkey_bytes)[12..]`
- 确定性推导，模拟器和节点都能计算
- 提取为 `n42_mobile::bls_pubkey_to_address()` 公共函数避免重复

### 实施细节
- `epoch_attestations: HashMap<String, u64>` 跟踪当前 epoch 的 attestation 计数
- `reward_queue: VecDeque<Withdrawal>` 待发放奖励队列（上限 1M 条目）
- `next_withdrawal_index: u64` 全局单调递增
- 每块最多处理 32 个奖励（支撑百万级验证者规模）
- Epoch = blocks_per_epoch 块（默认 21600 ≈ 24h @ 4s 出块）

## Phase 4: 奖励分发集成

### 数据流
```
HubEvent::ReceiptReceived
    → MobileVerificationBridge::process_receipt()
        → 签名验证通过 + receipt.is_valid()
            → reward_tx.send(pubkey_hex)
                → MobileRewardManager::record_attestation(pubkey)

每次构建 payload 时:
    → check_epoch_boundary(committed_block_count + 1)
    → take_pending_rewards()
    → 注入 PayloadAttributes.withdrawals
```

### 块号追踪修复
- **原始问题**：`timestamp / slot_time` 产生 Unix 时间戳量级的数字（~400M），不是实际块号
- **修复方案**：在 `ConsensusOrchestrator` 添加 `committed_block_count: u64` 字段，每次 `handle_block_committed` 递增。构建 payload 时使用 `committed_block_count + 1`

## Phase 5: 测试网脚本集成

- 在 `scripts/testnet-7node.sh` 添加 `--mobile-sim` / `--no-mobile-sim` 参数
- Step 7.5：启动 `n42-mobile-sim` 连接全部 7 节点的 StarHub
- Cleanup 处理 mobile sim 进程

---

## 审计修复（5 个 bug）

| # | 严重度 | 问题 | 修复 |
|---|--------|------|------|
| 1 | Critical | 块号估算 `timestamp/slot_time` 产生错误数字 | 添加 `committed_block_count` 字段追踪实际块号 |
| 2 | Medium | `record_attestation` 在 `&pubkey_hex[..16]` 短字符串时 panic | 使用 `if len >= 16` 安全截断 |
| 3 | Medium | `bls_pubkey_hex_to_address` 无效 hex 静默返回零地址 | 检查 decode 结果 + 48 字节长度，失败时 warn 日志 |
| 4 | Medium | `reward_queue` 无上限 | 添加 `MAX_REWARD_QUEUE_SIZE = 1M` 常量，超限时跳过分发 |
| 5 | Medium | `receive_packet` 无总体超时 | 使用 deadline 模式，CacheSync 不重置超时 |

## 测试补全（新增 12 个测试）

### mobile_reward.rs（+8 个）
- `test_short_pubkey_hex_no_panic` — 短字符串不 panic
- `test_invalid_hex_address_returns_zero` — 无效 hex 返回零地址
- `test_zero_blocks_per_epoch_no_distribution` — 零 epoch 不分发
- `test_take_empty_queue_returns_empty` — 空队列返回空 vec
- `test_multi_epoch_drain_across_blocks` — 多 epoch 跨块 drain
- `test_reward_address_matches_expected_derivation` — 地址推导一致性
- `test_reward_saturating_mul_no_overflow` — u64 溢出安全
- `test_many_unique_validators` — 100 个验证者批量处理

### mobile_bridge.rs（+4 个）
- `test_reward_tx_sends_pubkey_on_valid_receipt` — 有效 receipt 发送 pubkey
- `test_reward_tx_not_sent_on_invalid_receipt` — 无效 receipt 不发送
- `test_reward_tx_multiple_valid_receipts` — 多个有效 receipt 各自发送
- `test_no_reward_tx_configured_no_panic` — 无 reward_tx 不 panic

## 代码优化

- 提取 `bls_pubkey_to_address()` 为 `n42-mobile` crate 公共函数，消除 `mobile_reward.rs` 和 `n42-mobile-sim/main.rs` 的重复实现
- `bls_pubkey_hex_to_address()` 现在验证 hex 输出恰好 48 字节

---

## 完成状态

- [x] Phase 1: QUIC 客户端提取
- [x] Phase 2: 手机模拟器二进制
- [x] Phase 3: 奖励管理器
- [x] Phase 4: 奖励分发集成
- [x] Phase 5: 测试网脚本集成
- [x] 审计修复（5 个 bug）
- [x] 测试补全（新增 12 个，共 208 个通过）
- [x] 代码优化（函数提取去重）

## 测试结果
- n42-mobile: 94 passed
- n42-node: 107 passed
- comm_stress_bench: 1 passed
- stream_v2_pipeline: 6 passed
- **总计: 208 passed, 0 failed**
