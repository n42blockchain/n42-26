# 开发日志-49: LAN 极限 TPS — Cap Sweep + Wave Mode

## 目标

在 LAN 环境下寻找最大 TPS，通过调整 MAX_TXS_PER_BLOCK (cap) 和注入策略找到最优点。

## 测试环境

- 7 节点 macOS, Fast Propose (`N42_FAST_PROPOSE=1`, `N42_MIN_PROPOSE_DELAY_MS=0`)
- 500M gas limit, 预签名 presigned txs, blast 模式
- Pool 限制: pending_limit 100K txs (从 200K 下调)
- 7 RPC endpoints 并行注入

## 核心公式

```
TPS = cap / block_time
```
- cap 越大 → block_time 越长（build_ms, EVM, network 都增加）
- cap 越小 → block_time 越短但每块 tx 少
- 存在最优 cap 使 TPS 最大化

## Cap Sweep 结果

### Blast 模式（持续注入，无暂停）

| Cap | sustained_tps | avg_block_time | peak_block_tps | 备注 |
|-----|--------------|----------------|----------------|------|
| 20K | ~8,000 | - | - | backpressure 模式，stop-start 导致低效 |
| 40K | 20,072 | ~2.0s | 40,752 | pool=100K 时链 stall 60s+ |
| **48K** | **39,055** | **~1.2s** | **64,000** | **最佳结果** |
| 60K | 35,209 | ~1.7s | 60,000 | build_ms 增加抵消了更大 cap |

### Wave 模式（注入 cap txs → 等区块 → 再注入）

| Cap | sustained_tps | 备注 |
|-----|--------------|------|
| 48K | 34,669 | RPC 注入速率瓶颈（~20K tx/s），inject_ms 过长 |

**结论**: Blast 模式 > Wave 模式，因为 blast 持续填充 pool 而 wave 受 RPC 吞吐限制。

## 关键发现

### 1. 48K Cap 是 LAN 最优点
- 39K TPS 是当前架构在 LAN 中的极限
- 比之前 28K cap 的 14K TPS 提升 **2.8x**
- 比之前 Optimistic Voting 后的 13.8K TPS 提升 **2.8x**

### 2. Pool Overload 是稳定性风险
- pool_pending > 100K 时，im::OrdMap 迭代变慢：iter_ms 49→498ms
- build_ms 从 176ms 膨胀到 1130ms
- 40K cap 在 pool=100K 时直接 stall
- **解决**：将 pending_limit 从 200K 降至 100K

### 3. RPC 注入是真正瓶颈
- JSON-RPC batch 注入极限约 20K tx/s（7 RPC endpoints）
- Chain 处理能力 > 注入能力：48K tx/block @ ~1.2s = 40K tx/s 处理速率
- 未来可通过 binary protocol 或直接 pool 注入突破

### 4. Build_ms 方差大
- pool=100K 时：evm_exec_ms 101-876ms（state cache 冷热差异）
- iter_ms 49-498ms（OrdMap at high pool depth）
- pool_overhead 74-516ms

## 代码变更

### 1. Pool 限制下调 (`crates/n42-node/src/pool.rs`)
```rust
pending_limit: SubPoolLimit { max_txs: 100_000, max_size: 200 * 1024 * 1024 },
```
从 200K → 100K，防止 pool overload 导致 build_ms 膨胀和链 stall。

### 2. Wave 模式 (`bin/n42-stress/src/main.rs`)
- 新增 `--wave <cap>` CLI 参数
- `run_wave_mode()`: 注入 cap txs → poll eth_blockNumber → 检测新块 → 重复
- 所有 RPC batch 并行发送（非顺序），最小化 inject_ms

## 架构瓶颈分析

```
当前链路:
  Stress Tool → JSON-RPC → Pool (100K limit) → Packing → EVM → Broadcast → Consensus
                  ↑                               ↑
              ~20K tx/s                       48K tx @ 176-1130ms
              (瓶颈)                          (pool depth 敏感)

LAN 极限 TPS = min(注入速率, 处理速率) ≈ 39K TPS
```

## Phase 2: Binary TCP Injection

### 设计

绕过 JSON-RPC 开销，通过原生 TCP 直接向节点的 transaction pool 注入 EIP-2718 编码的交易。

**协议** (每批次):
```
Client → Server: [u32 LE num_txs] [u16 LE tx_len, tx_bytes] × num_txs
Server → Client: [u32 LE accepted_count]
num_txs = 0 → graceful close
```

**Server 端** (`crates/n42-node/src/inject.rs`):
- TCP 监听器，端口由 `N42_INJECT_PORT` 环境变量控制
- 每个连接独立处理，解码 EIP-2718 tx → recover sender → `pool.add_external_transactions()` 批量插入
- 统计监控：received, accepted, decode_errors, pool_errors

**Client 端** (`bin/n42-stress/src/main.rs`):
- `--inject host:port,...` CLI 参数
- `load_presigned_binary()`: 加载预签名文件为原始字节（不做 hex 编码）
- 每个 endpoint 一个 TCP 连接，流式发送批次

**关键教训**:
- Presigned 文件按 RPC 分组，每组内同账户 nonce 有序
- 必须保持原始分组映射到 inject endpoints（不能 round-robin 重分配），否则 nonce 乱序导致大量 queued

### 测试结果

| 模式 | Cap | overall_tps | injection_tps | p50_tps | 备注 |
|------|-----|------------|---------------|---------|------|
| JSON-RPC blast | 48K | 39,055 | ~20K | 48,000 | 之前最佳 |
| **TCP inject** | **48K** | **47,527** | **122,763** | **48,000** | **新最佳 (+22%)** |
| TCP inject | 60K | 42,024 | 26,976 | 48,384 | pool overload 限制 |

### 关键观察

1. **注入速率**: TCP 122K tx/s vs JSON-RPC 20K tx/s — **6x 提升**
2. **48K cap**: overall_tps 从 39K → 47.5K (+22%)，p50 块打满 cap
3. **60K cap**: pool=100K 时注入速率暴跌至 27K（pool.add_external_transactions 阻塞），overall_tps 反而低于 48K
4. **Pool 是新瓶颈**: add_external_transactions 在 pool 满时阻塞，注入速率受限于 pool 消化速度
5. **Errors**: 395K txs 被 pool 拒绝（容量限制），实际接受率 ~80%

### 架构

```
Before (JSON-RPC):
  Stress Tool → HTTP/JSON-RPC → hex decode → pool.add_external_transaction() [single]
  ~20K tx/s injection

After (TCP inject):
  Stress Tool → TCP binary → decode EIP-2718 → pool.add_external_transactions() [batch]
  ~122K tx/s injection (pool 未满时)
  ~27K tx/s injection (pool=100K 时)
```

### 代码变更

1. **新增 `crates/n42-node/src/inject.rs`** — TCP binary injection server
2. **修改 `bin/n42-node/src/main.rs`** — N42_INJECT_PORT 环境变量控制启动
3. **修改 `bin/n42-stress/src/main.rs`** — `--inject` 模式、`load_presigned_binary()`
4. **修改 `scripts/testnet.sh`** — N42_INJECT_PORT 自动递增

## 优化方向

| 方向 | 预期收益 | 难度 |
|------|---------|------|
| ~~Binary injection~~ | ~~+50%~~ ✅ **+22% (47.5K TPS)** | 已完成 |
| Pool 深度控制（auto-tune cap vs pool size） | 稳定 build_ms | 低 |
| 直接 pool inject（跳过 ECDSA recovery） | +30% 注入速率 | 中 |
| Parallel EVM (80K tx blocks) | 对 48K cap 无显著收益 | 高 |
| Sub-block / Multi-proposer | 理论 100K+ TPS | 极高 |
