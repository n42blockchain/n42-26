# 开发日志-20：TPS 优化 Phase 2 — Builder Time Budget

> 日期：2026-03-06
> 目标：消除高负载下的 block stall，提升 P50 TPS 稳定性

---

## 背景

Phase 1（Leader Deferred Import）实现后，P50 TPS 达到 1300-1400。但在 target 5000+ 时出现严重 stall：
- avg_block_time 飙到 12s（正常应为 4s）
- 60s 内仅产出 3-4 个区块

**根因**：reth 的 `default_ethereum_payload` builder 在一次 `try_build` 中处理 pool 中**所有交易**。
当 pool 有 20K+ txs 时，sequential EVM 需要 14s+ 完成构建，远超 4s slot time。
builder 的 deadline（默认 12s）和 `WaitForPending` resolve 策略都无法中断已 spawn 的 build task。

---

## 关键发现：EVM 速度远超预期

通过日志分析发现：MacBook M 系列上 sequential EVM 实际执行速度约 **5700 tx/s**（简单转账），
远高于之前估计的 1400 tx/s。block 95 在一次构建中处理了 17125 txs（360M gas），
全部在 3s 内完成。

之前 P50 只有 ~1400 TPS 的原因不是 EVM 慢，而是：
1. **Stress tool 发送速率受限**：签名 ~173µs/tx，batch_interval + 签名 = 37ms/batch，上限 ~2700 tx/s
2. **Pipeline 总延迟**：build + consensus + import + slot alignment > 4s
3. **CPU 竞争**：7 节点共享 8 核 MacBook，互相争抢 CPU

---

## 实施细节

### 改动 1：EthereumBuilderConfig 添加 build_time_budget（reth 侧）

**文件**：`reth/crates/ethereum/payload/src/config.rs`

```rust
pub struct EthereumBuilderConfig {
    // ... existing fields ...
    /// Maximum wall-clock time the builder may spend packing transactions.
    pub build_time_budget: Option<Duration>,
}
```

### 改动 2：default_ethereum_payload 循环添加时间检查（reth 侧）

**文件**：`reth/crates/ethereum/payload/src/lib.rs`

在 `while let Some(pool_tx) = best_txs.next()` 循环中添加：
```rust
let build_start = Instant::now();
let build_time_budget = builder_config.build_time_budget;
let mut tx_count: u64 = 0;

// 每 64 txs 检查一次时间（摊薄 Instant::now() 系统调用开销）
if let Some(budget) = build_time_budget {
    if tx_count & 63 == 0 && tx_count > 0 && build_start.elapsed() >= budget {
        break;
    }
}
```

### 改动 3：N42 配置 3s 默认 budget

**文件**：`crates/n42-node/src/payload.rs`

通过环境变量 `N42_BUILD_TIME_BUDGET_MS` 配置（默认 3000ms）：
```rust
let build_budget = Duration::from_millis(
    std::env::var("N42_BUILD_TIME_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000),
);
```

---

## 压测结果对比（600 accounts, 7 nodes, 4s slot, 500M gas）

### Phase 1 基线 vs Phase 2（3s budget）vs Phase 2（2s budget）

| Target | Phase 1 P50 | 3s Budget P50 | 2s Budget P50 | 最佳改善 |
|--------|------------|--------------|--------------|---------|
| 2000 | 1369 | 1435 | **1487** | +8.6% |
| 3000 | 1170 | **1366** | 1346 | +16.8% |
| 5000 | 1150 | **1305** | 1204 | +13.5% |
| 7500 | 1115 | 1057 | 1144 | +2.6% |

**关键改进**：
- Target 5000 的 avg_block_time 从 **12.0s → 4.0s**（stall 消除）
- Target 3000 的 P50 从 1170 → **1366**（+17%）
- 整体 block time 稳定性显著提升

---

## 当前瓶颈分析

1. **Stress tool 发送速率**：签名阻塞主循环，有效发送 ~2000 tx/s
2. **CPU 竞争**：7 节点 × 全部组件 = 56+ 线程争 8 核
3. **Pipeline 延迟**：build(2-3s) + consensus(~1s) + import(2-3s)，部分可并行但总时间 >4s
4. **Sequential EVM**：虽然速度 5700 tx/s 已够快，但并行可进一步减少 build time

---

## 文件变更清单

| 文件 | 改动 |
|------|------|
| `reth/crates/ethereum/payload/src/config.rs` | 新增 build_time_budget 字段 |
| `reth/crates/ethereum/payload/src/lib.rs` | 添加时间预算检查到 tx 打包循环 |
| `crates/n42-node/src/payload.rs` | 配置 3s 默认 build time budget |

---

## 下一步：Phase 3 — 并行 EVM（参考 Sei Network）

### Sei V2 并行执行核心设计
- **乐观并行执行**：所有 tx 并行运行，追踪 read/write set
- **冲突检测与重执行**：冲突 tx 串行重执行，递归至无冲突
- **SeiDB**：针对 EVM state access pattern 优化的存储层

### Rust 生态参考
- **RISE PEVM** (github.com/risechain/pevm)：Rust 原生并行 EVM
- **FAFO**：单节点 110 万 TPS（native transfers），基于 REVM

### N42 集成路径
1. 在 builder 的 `try_build` 中替换 sequential 执行为并行执行
2. 使用 rayon 线程池 + 乐观并发控制
3. 对简单转账（不同 sender/receiver）实现零冲突并行
4. 预期 4-8x 提升 → build time 从 2s 降到 0.3-0.5s
