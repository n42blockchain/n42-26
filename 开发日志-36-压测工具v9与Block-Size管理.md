# 开发日志 36 — 压测工具 v9 与 Block Size 管理

## 日期: 2026-03-09

## 一、问题背景

日志-35 的 pipeline profiling 发现：
- 整个出块管线仅占 2s slot 的 5%（~100ms）
- Sustained TPS 14.8K 的瓶颈是 **压测工具注入速率**（v8 串行签名限制在 ~18K TPS）
- 节点单块峰值可达 43,469 TPS（86,939 tx），但 sustained 受限于 tx 供给

## 二、改动

### 2.1 压测工具 v9 — Pipeline 签名/发送

**设计决策**：v8 的 sender_loop 是串行 sign → send → sleep。v9 将 sign 和 send 拆分为两个独立 task，通过 bounded channel 连接。

```
[Signer Task] → channel(depth=4) → [Sender Task] → HTTP fire-and-forget
     │                                      │
     └── sign 下一批的同时 ──────────────── send 当前批
```

关键改动（`bin/n42-stress/src/main.rs`）：
- `SignedBatch` 结构体：封装签名结果用于 channel 传输
- `tokio::sync::mpsc::channel(pipeline_depth=4)` 连接 signer 和 sender
- Rate limiter 修复：`batch_size.min((target_tps * 2).max(50))` 防止低 TPS 时注入过快

### 2.2 Block Size 管理 — 三重限制

**问题**：v9 提升注入率后，节点在高压下打包 95K tx 的超大块，EVM 非线性膨胀（30K tx=32ms → 95K tx=1045ms），导致 block time 从 2s 膨胀到 3.7-4.6s。

**方案**：三重限制防止 block 过大

1. **`N42_MAX_TXS_PER_BLOCK`**：100K → **40K**（`reth/crates/ethereum/payload/src/lib.rs` + `scripts/testnet.sh`）
   - 40K tx × 2s slot = 20K TPS 理论上限，足够当前目标
   - 保持在 EVM 线性执行区间（<30K tx ≈ <100ms EVM）

2. **`N42_BUILD_TIME_BUDGET_MS`**：1500ms → **1000ms**（`reth/crates/ethereum/payload/src/lib.rs`）
   - 留 1000ms 给 finish() + broadcast + import + consensus
   - 检查频率从每 1000 tx → **每 256 tx**（Instant::now() 开销 ~20ns 可忽略）

3. **Gas Limit**：2G（不变，但不再是约束因素）

### 修改文件
- `reth/crates/ethereum/payload/src/lib.rs` — block size 管理参数
- `scripts/testnet.sh` — N42_MAX_TXS_PER_BLOCK 环境变量 100K→40K
- `bin/n42-stress/src/main.rs` — v9 pipeline 架构

## 三、压测结果

### 3.1 v9 + 40K Block Limit（7 nodes, 2s slot, 2G gas）

| Target TPS | Effective TPS | avg_block_time | overall_tps | max_tx_in_block | Gas Util |
|---|---|---|---|---|---|
| 1,000 | 1,950 | **2.0s** | 2,024 | 4,108 | 4.2% |
| 3,000 | 5,948 | **2.0s** | 6,171 | 14,697 | 12.7% |
| 5,000 | 9,844 | **2.0s** | 10,133 | 21,186 | 20.9% |
| 7,500 | 13,701 | 2.7s | 12,149 | **40,000** | 33.3% |
| 10,000 | 13,591 | 2.5s | 13,036 | **40,000** | 33.4% |
| 15,000 | 14,733 | 3.0s | 12,518 | **40,000** | 38.8% |
| 20,000 | 13,399 | 3.0s | 12,038 | **40,000** | 37.3% |
| 30,000 | 14,425 | 2.8s | 11,063 | **40,000** | 31.9% |
| 40,000 | 14,140 | 3.0s | 12,212 | **40,000** | 37.5% |
| **50,000** | **16,494** | **2.9s** | **13,535** | **40,000** | **39.9%** |

### 3.2 与 v8 无限制对比

| 指标 | v8 无限制 (日志-34) | v9 + 40K Limit | 变化 |
|---|---|---|---|
| Peak sustained TPS | 14,820 | **13,535** | -8.7%（trade-off） |
| Max block TPS | 43,469 | **20,000** | 受限于 40K cap |
| Max tx/block | 86,939 | **40,000** | 限制生效 |
| avg_block_time @高压 | 3.7-4.6s | **2.5-3.0s** | **改善 20-35%** |
| Gas utilization (max) | 91.3% | 42.0% | 受限于 tx cap |
| Block time 稳定性 | 波动大 | 更稳定 | 改善 |

### 3.3 关键发现

1. **40K 限制精确生效**：Leader 日志确认 `tx_count=40000` 命中上限，packing_ms=395-521ms
2. **低 TPS 区间不受影响**：1K-5K TPS 保持完美 2.0s block time
3. **高压区 block time 改善但未达 2.0s**：7.5K+ TPS 时 avg_block_time=2.5-3.0s vs 之前 3.7-4.6s
4. **40K tx 块的 EVM 时间**：221-348ms（线性区间内），远好于 95K tx 的 1045ms
5. **瓶颈转移**：现在是 40K tx 块的 finish()+broadcast+consensus 需要 0.5-1.0s 额外时间

### 3.4 Leader Packing 时序

从节点日志提取：

| tx_count | packing_ms | evm_exec_ms | pool_overhead_ms |
|---|---|---|---|
| 40,000 | 521ms | 221ms | 300ms |
| 40,000 | 395ms | 348ms | 46ms |
| 23,157 | 301ms | 263ms | 37ms |

Pool overhead 波动大（37-300ms），可能是 pool 迭代器排序成本。

## 四、Block Time 膨胀分析

40K tx 块在 2s slot 中的时间分配：

```
Packing (40K tx):   400-520ms  │████████░░░░░░░░░░░░│ 20-26%
Finish (SR):        25-40ms    │░░░░░░░░░░░░░░░░░░░░│ 1-2%
Broadcast (zstd):   50-100ms   │█░░░░░░░░░░░░░░░░░░░│ 2.5-5%
Follower import:    3-5ms      │░░░░░░░░░░░░░░░░░░░░│ 0.15%
Consensus vote:     20-30ms    │░░░░░░░░░░░░░░░░░░░░│ 1-1.5%
                               ├────────────────────┤
Pipeline 总计:      ~600ms     │████████████░░░░░░░░│ 30%
```

但实际 block time 是 2.5-3.0s（需额外 0.5-1.0s），说明存在其他开销：
- FCU 处理 + persist queue check
- 下一个 slot 的 forkchoice_updated 等待
- Pool 重新排序大量 pending tx

## 五、下一步

1. **降低 MAX_TXS 到 30K**：尝试保持 avg_block_time ≤ 2.2s
   - 30K tx × 21K gas = 630M gas（31.5% of 2G）
   - EVM 时间约 100-150ms，packing < 300ms
   - 理论 TPS = 30K / 2 = 15K（与当前 sustained 13.5K 相近）

2. **优化 pool overhead**：300ms pool 迭代开销异常高
   - 可能是 BTreeMap 排序 20W+ pending tx 的成本
   - 考虑预排序或缓存 best iterator

3. **1s Slot Time 评估**：如果 block time 稳定在 ≤ 2.2s，可以考虑 1s slot
   - 需要 pipeline 总时间 < 800ms
   - 当前 30K tx 块的 pipeline ≈ 400-500ms，理论可行

4. **压测工具进一步优化**：v9 注入能力 ~16.5K TPS，接近但未超过节点处理能力
   - 增加 RPC 端点数量（7→14）
   - 增大 pipeline_depth（4→8）
