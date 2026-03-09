# 开发日志 33 — 压测工具 v8 与 Gas Limit 突破

## 日期: 2026-03-08

## 一、问题分析

v7 压测工具在 7500 TPS 目标时仅达到 4,417 TPS（37% gas 利用率），链远未被推满。瓶颈在工具侧：

1. **账户不足（600）**：nonce 通道少，一旦出现 gap，大量 tx 变成 queued
2. **单循环架构**：一个主循环串行签名+发送，受 sleep 间隔限制
3. **RPC 负载不均**：每批 tx 全部发到第一个账户的 RPC，其他 RPC 空闲
4. **每账户每批 tx 太多**：500/50=10 txs/account/batch，一批失败产生 10 个 nonce gap
5. **Genesis 仅预分配 600 账户**：扩大账户数需同步更新 genesis

## 二、v8 架构改进

### 并行发送器架构
- 每个 RPC endpoint 一个独立 sender_loop task
- 账户按 rpc_idx 分区：2000 / 7 = ~286 accounts/RPC
- 各 sender 独立签名、发送、nonce 管理
- 消除 RPC 竞争和负载不均

### 账户与 nonce 优化
- 默认账户数：600 → **2000**（3.3x 更多 nonce 通道）
- accounts_per_batch: 20 → **100**
- 每账户每批仅 ~10 txs（vs 原来 71 txs），减少 nonce gap 损伤
- Genesis 预分配：600 → **5000** 账户

### 批量与并发
- batch_size: 500 → **1000**
- concurrency: 512 → **1024**
- max_pool: 30000 → **80000**
- spawn_blocking 签名：不阻塞 async runtime
- 独立背压监控任务（共享 AtomicBool）

### Step 模式扩展
- TPS 级别：`[500, 1000, 2000, 3000, 5000, 7500, 10000, 12000, 15000, 20000, 30000, 50000]`
- 高 TPS 自动放大 batch_size (2000) 和 accounts_per_batch (500)
- 智能 batch_size 缩放：不超过 per_rpc_tps，避免低 TPS 时过度注入

### 修改文件
- `bin/n42-stress/src/main.rs` — v8 完整重写
- `scripts/testnet.sh` — NUM_ACCOUNTS 600 → 5000

## 三、压测结果 (7 nodes, 2s slot, 500M gas, compact block)

| Target TPS | Injection TPS | Sustained TPS | Gas Util | Max Block TPS | Fail Rate |
|---|---|---|---|---|---|
| 500 | 487 | 502 | 4.2% | 721 | 0% |
| 1,000 | 967 | 978 | 8.1% | 1,320 | 0% |
| 2,000 | 1,914 | 1,966 | 16.2% | 2,503 | 0% |
| 3,000 | 2,832 | 2,902 | 23.9% | 3,922 | 0% |
| 5,000 | 4,560 | 4,666 | 38.4% | 5,735 | 0% |
| **7,500** | **9,980** | **9,410** | **85.4%** | **11,904** | 0% |
| 10,000 | 9,726 | 10,088 | 83.0% | 11,904 | 1.6% |
| 12,000 | 9,821 | 9,879 | 81.3% | 11,904 | 0% |
| **15,000** | **14,942** | **10,908** | **89.8%** | **11,904** | 0.1% |
| 20,000 | 18,162 | 3,484* | 28.7%* | 11,904 | 2.3% |
| **30,000** | **22,468** | **11,028** | **89.6%** | **11,904** | 3.6% |
| 50,000 | 35,495 | STALL | — | — | STALL |

\* 20000 步的低 sustained 因为分析窗口包含了上一步的排空期

### 关键发现
- **Gas Limit 已被推满**：多个步骤达到 100% gas limit（23809 tx/block = 500M gas）
- **Peak sustained block TPS: ~11,000**（理论极限 11,905 的 92%）
- **50K TPS 导致链 stall**：pool 溢出（75K pending txs），payload builder 超时
- **0 个 state root mismatch**：7 个节点全部健康

## 四、对比基线

| 指标 | v7 (日志-32) | v8 (本次) | 提升 |
|---|---|---|---|
| Peak sustained TPS | 4,417 | **11,028** | **2.5x** |
| Max block TPS | 6,607 | **11,904** | **1.8x** |
| Gas utilization (peak) | 37% | **89.8%** | **2.4x** |
| Error rate (peak) | 0% | 3.6% (30K) | — |
| Accounts | 600 | 2,000 | 3.3x |
| Parallel senders | 1 | 7 (per-RPC) | 7x |

## 五、瓶颈转移

之前的瓶颈是压测工具 → 现在工具已不再是瓶颈。

新的瓶颈：**500M gas limit 本身**
- 500M / 21K gas per tx / 2s = 11,905 TPS 理论极限
- 已达到 92% 利用率，接近物理上限

## 六、50K TPS Stall 分析

- 注入 4.3M txs，pool 累积到 75K pending + 25K queued
- Pacemaker timeout at view 673
- 原因：payload builder 从巨大 pool 中打包 tx 超时
- **不是 state root 问题**，不是共识问题 — 纯粹是 pool 管理极限

## 七、下一步

要突破当前 ~11K TPS 天花板，需要：
1. **增大 gas limit**（如 1G 或 2G）→ 理论上可达 23K-47K TPS
2. **Phase B1: In-memory State Cache** → 减少 reth import 的 disk I/O
3. **1s slot time** → 降低延迟（需 import < 500ms）
4. **Pool 管理优化** → 防止 50K+ 时 stall
