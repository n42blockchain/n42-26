# 开发日志-18：TPS 深度优化 680 → 1000+ TPS

> 日期：2026-03-05
> 目标：600 账户压测，目标 10K TPS，全链路瓶颈分析与消除

---

## 背景

前期优化（日志-17）达到稳定 680 TPS / 峰值 754 TPS（7 节点本地测试网，简单转账）。
Gas limit 已提至 120M 但仅使用 53%，说明瓶颈不在 gas 而在执行管线其他环节。

---

## 发现与修复的瓶颈（按发现顺序）

### 瓶颈 1：Genesis gasLimit 锁死 30M

**问题**：`--builder.gaslimit 1000000000` 无效，因为 EIP-1559 每个区块只能调整 ±1/1024。Genesis 设 30M，需要 ~33,000 个区块才能达到 1B。

**修复**：直接在 genesis JSON 中设置高 gasLimit。
```
# scripts/testnet.sh genesis
"gasLimit": "0xBEBC200"  # 200M (从 0x1C9C380 即 30M)
```

**设计决策**：选择 200M 而非 1B/5B，因为更大的 gas limit 会导致单个区块包含过多交易（>9500 tx），产生超大区块（>4MB），超过 P2P 传播限制。

### 瓶颈 2：交易池内存溢出

**问题**：池限制 200K/100K/100K 导致 `Memory capacity exceeded` 错误，RPC 序列化大池状态时崩溃，连带饿死共识事件循环。

**修复**：
```rust
// crates/n42-node/src/pool.rs
pending_limit: SubPoolLimit { max_txs: 50_000, max_size: 100 * 1024 * 1024 },
basefee_limit: SubPoolLimit { max_txs: 25_000, max_size: 50 * 1024 * 1024 },
queued_limit: SubPoolLimit { max_txs: 25_000, max_size: 50 * 1024 * 1024 },
```

### 瓶颈 3：P2P 交易广播风暴

**问题**：7 节点 × 50K 交易 = 数百万条重复 gossip 消息，`"failed to publish transaction error=Duplicate"` 刷屏，淹没共识消息传递。

**修复**：添加 `--disable-tx-gossip` 标志。压测场景下通过 RPC 直接向各节点发送交易。

**权衡**：生产环境需要 tx gossip（或带速率限制的 gossip），当前仅在压测模式禁用。

### 瓶颈 4：区块超过 GossipSub 消息上限

**问题**：Block 208 包含 23,809 笔交易（~3MB+），超过 GossipSub 4MB 消息限制。区块数据无法广播到 follower，共识永久停滞。

**修复**：
```rust
// crates/n42-network/src/transport.rs
.max_transmit_size(8 * 1024 * 1024)  // 从 4MB → 8MB

// crates/n42-network/src/gossipsub/handlers.rs
8 * 1024 * 1024  // block topic 从 4MB → 8MB
```

同时将 gasLimit 从 1B 降至 200M（最多 ~9,523 tx/block ≈ 1,190 TPS@8s slot），确保区块大小在安全范围内。

### 瓶颈 5：Builder 参数未优化

**问题**：
- `builder.interval` 默认 1s → 8s slot 仅 8 次构建尝试
- `BUILDER_WARMUP_DELAY` 100ms 固定浪费
- 签名验证并发 = 1

**修复**：
```bash
# scripts/testnet.sh
--builder.interval 50ms         # 从 1s → 50ms
--txpool.additional-validation-tasks 16  # 签名验证并发 16x
```
```rust
// execution_bridge.rs
const BUILDER_WARMUP_DELAY: Duration = Duration::from_millis(10);  // 从 100ms
```

---

## 压测结果

| 目标 TPS | 实际 P50 TPS | 峰值 TPS | 平均区块时间 | Gas 利用率 | 状态 |
|---------|------------|---------|------------|----------|------|
| 500 | 576 | 1,190 | 4.0s | 24% | 稳定 |
| 1,000 | 926 | 1,786 | 4.0s | 37% | 稳定 |
| 2,000 | 1,075 | 2,381 | 5.2s | 54% | 基本稳定 |
| 3,000 | 744 | 2,381 | 6.0s | 54% | 遇到天花板 |

**关键发现**：
- 稳定 1,000+ TPS（较 680 TPS 提升 47%）
- 峰值 2,381 TPS（单区块）
- Gas 利用率天花板 ~54%（200M 中用 ~108M）
- 区块时间在高负载下从 4s 增加到 5-6s

---

## 全链路开销分析

| 管线阶段 | 耗时 | 备注 |
|---------|------|------|
| 签名（secp256k1） | ~2μs/tx | 非瓶颈，CPU 可饱和 |
| RPC 往返 | 130-320ms | 高负载时退化 |
| 交易验证 | ~10μs/tx | 16 并发后非瓶颈 |
| EVM 执行 | 531ns/ETH tx | 单核理论 1.88M TPS |
| Payload 构建 | 50-200ms | 取决于 tx 数量 |
| 共识轮次 | 4-6s | 含 2 轮投票 + 区块传播 |
| P2P 区块传播 | 100-500ms | 大区块时显著 |
| 状态写入 | 50-100ms | MDBX 批量写 |

**真正的天花板**：共识轮次时间（4-6s）× 单区块 tx 容量（受 gas/区块大小限制）。

---

## 架构级优化方向（未实施）

1. **带速率限制的 tx gossip**：恢复交易传播但限制速率，使所有节点都有交易可打包
2. **Leader 感知路由**：压测工具直接向当前 leader 发送交易
3. **并行 EVM 执行**：利用多核并行执行无冲突交易
4. **流水线共识**：在当前区块达成共识的同时预构建下一个区块

---

## 文件变更清单

| 文件 | 改动 |
|------|------|
| `scripts/testnet.sh` | genesis gasLimit 200M, builder 参数, disable-tx-gossip |
| `crates/n42-node/src/pool.rs` | 池限制 50K/25K/25K |
| `crates/n42-network/src/transport.rs` | GossipSub 8MB |
| `crates/n42-network/src/gossipsub/handlers.rs` | block topic 8MB |
| `crates/n42-node/src/orchestrator/execution_bridge.rs` | warmup 10ms |
| `bin/n42-stress/src/main.rs` | v6 压测工具全量重写 |
| `bin/n42-evm-bench/` | 新增 EVM 单核基准测试 |
