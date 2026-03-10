# 开发日志-45: Pool OrdMap 替换与 Packing Loop 优化

> 日期：2026-03-10
> 阶段：Phase A12 — TX Pool 性能优化

---

## 背景

Phase A11 压测发现 `pool_overhead_ms` 是阻碍 TPS 提升的关键瓶颈：
- 48K cap + 24K TPS 注入时，avg_block_time=2.7s（目标 2.0s）
- pool_pending > 30K 时 build_ms 从 96ms 飙升到 700-1400ms
- 根因：`PendingPool::best()` 中 `BTreeMap` 的 O(n) clone

## 设计决策

### 1. im::OrdMap 替换 BTreeMap（持久化数据结构）

**问题分析**：`best()` 方法每次 payload build 时 clone 整个 `by_id: BTreeMap`，作为 `BestTransactions` 迭代器的快照。BTreeMap clone 是 O(n) 深拷贝，70K 条目时耗时 150-500ms。

**替代方案考虑**：
- Arc<BTreeMap> 共享引用 — 不可行，迭代器需要可变操作（remove/insert）
- 无锁快照 — 过于复杂，需要改变整个 pool 架构
- **im::OrdMap** — 持久化数据结构，结构共享 clone O(log n)，API 完全兼容 BTreeMap

**选择 im::OrdMap 原因**：
- clone() 从 O(n) 降到 O(log n)：70K 条目从 ~300ms → <1ms
- API 完全兼容：get/insert/remove/range/contains_key/values/len/is_empty
- OrdMap 的 Key 要求 Ord+Clone（TransactionId 满足），Value 要求 Clone（PendingTransaction 满足）
- 单个操作 (get/insert/remove) 保持 O(log n)，常数因子比 BTreeMap 略高但可接受

**改动文件**：
- `reth/crates/transaction-pool/Cargo.toml`: 新增 `im = "15"`
- `reth/crates/transaction-pool/src/pool/pending.rs`: `by_id` 字段、`clear_transactions()`、`by_id()` 返回值
- `reth/crates/transaction-pool/src/pool/best.rs`: `all` 字段

### 2. Packing Loop 优化 — no_updates()

**问题分析**：`BestTransactions::next_tx_and_priority()` 每次调用都执行 `add_new_transactions()`，轮询 broadcast channel 最多 16 次。48K tx 打包 = 768K 次 `try_recv()` 调用。

**方案**：在 packing 开始时调用 `best_txs.no_updates()` drop receiver。packing 已基于 pool 快照工作，新交易等下一个块即可。

### 3. 跳过非 Osaka 的 RLP 长度计算

**问题分析**：`tx.inner().length()` 遍历 RLP 编码结构，每笔 ~1-2µs。`MAX_RLP_BLOCK_SIZE` 检查只在 Osaka 硬分叉生效，N42 不是 Osaka。

**方案**：`is_osaka` 为 false 时完全跳过 `length()` 和 block size 检查。

### 4. 延迟 to_consensus() + 细粒度计时

**方案**：将 `to_consensus()` 移到 gas check 之后（gas 不够的 tx 不需要转换）。同时添加 `iter_ms`、`consensus_ms`、`other_ms` 三个独立计时器。

## 压测结果

### 测试配置
- 7 nodes, 2s slot, 2G gas, compact block, Fast Propose
- blast 1.8M presigned txs, target_tps=16000, 120s

### 对比数据

| 指标 | 优化前 (BTreeMap) | OrdMap only | 全部优化 |
|------|-----------------|-------------|---------|
| sustained_tps | 11,536 | 11,742 | **13,636** |
| avg_block_time | 2.7s (stall) | 4.2s (step) | **2.8s** |
| max_block_tps | 12,000 | 12,000 | **24,000** |
| pool_overhead (48K tx) | 430-485ms | 432-485ms | **23ms** |
| pool_pending peak | 200K (stall) | 170K | 69K |
| bp_pauses | 频繁 | 0 | 0 |
| 系统稳定性 | pool>70K 瘫痪 | 170K 稳定 | 69K 稳定 |

### 细粒度 Packing 分析

| tx_count | packing_ms | evm_ms | pool_overhead_ms | iter_ms | consensus_ms | other_ms |
|----------|-----------|--------|-----------------|---------|--------------|---------|
| 17,679 | 30 | 21 | 8 | 5 | 1 | 1 |
| 29,363 | 48 | 34 | 14 | 9 | 1 | 2 |
| 46,928 | 76 | 52 | **23** | 16 | 2 | 4 |

**关键发现**：
- `no_updates()` 是最大贡献者：pool_overhead 从 ~450ms → ~23ms（**95% 降幅**）
- `consensus_ms`（to_consensus）= 2ms @ 47K tx — 不是瓶颈
- `iter_ms` 占 pool_overhead 的 70% — BestTransactions 迭代器是剩余主要开销
- 偶发异常（153-173ms）可能是磁盘 I/O 抖动导致 EVM state 读取变慢

## 遗留问题

1. 少数异常块 pool_overhead=153-173ms，iter_ms=125-151ms — 疑似 EVM state DB I/O 竞争
2. avg_block_time=2.8s 仍高于 2.0s 目标 — 可能需要调整 slot timing
3. gas_utilization=38.6% — 48K cap 限制，可提升到更高

## 文件改动清单

### reth (path dependency, ../reth)
- `crates/transaction-pool/Cargo.toml` — +im="15"
- `crates/transaction-pool/src/pool/pending.rs` — OrdMap 替换 BTreeMap (4处)
- `crates/transaction-pool/src/pool/best.rs` — OrdMap 替换 BTreeMap (2处)
- `crates/ethereum/payload/src/lib.rs` — no_updates + skip RLP + defer to_consensus + granular timing
