# 开发日志 37 — 43K+ TPS 突破计划

## 日期: 2026-03-09

## 一、目标

将 sustained TPS 从 13.5K 提升到 **43K+**（匹配单块峰值 43,469 TPS）。

## 二、瓶颈分层分析

### 2.1 第一层：EVM 非线性膨胀（最关键，挡住了一切）

数据点：
- 30K tx = 32ms EVM (1.07µs/tx) — 线性区间
- 40K tx = 221-348ms (5.5-8.7µs/tx) — **5-8x 膨胀**
- 95K tx = 1,045ms (11µs/tx) — **10x 膨胀**

根因分析（reth/crates/revm/src/cached.rs）：

| 操作 | 文件:行 | 复杂度 | 影响 |
|------|---------|--------|------|
| snapshot_hot_state() 全量 clone | cached.rs:262 | O(n_accounts × avg_storage) | **主因** |
| AccountInfo clone (每次 basic()) | cached.rs:110 | O(1) 但高频调用 | 次要 |
| Bytecode clone (每次 code_by_hash()) | cached.rs:120 | O(code_size) | 次要 |
| update_hot_state() 全迭代 | cached.rs:234-247 | O(changed × slots) | 构建后开销 |
| HashMap 扩容 rehash | (运行时) | O(n) 偶发 | 尖峰 |

**核心问题**：hot state cache 设计为"每块 clone 一份快照"，在账户数增长时成本线性膨胀。

### 2.2 第二层：Block Time 膨胀

当前 40K tx 块：avg_block_time 2.5-3.0s（vs 理想 2.0s）

| 延迟源 | 时间 | 位置 |
|--------|------|------|
| Slot 边界对齐等待 | 0-50ms | consensus_loop.rs:73-91 next_slot_boundary() |
| Builder warmup 固定延迟 | 10ms | execution_bridge.rs:104-112 |
| best_transactions 池迭代 | 200-450ms | reth resolve_kind(WaitForPending) |
| Pool overhead (300ms spike) | 0-300ms | 大池排序竞争 |
| **总额外开销** | **260-530ms** | |

### 2.3 第三层：压测注入率

v9 上限 ~16.5K TPS。

| 瓶颈 | 说明 |
|------|------|
| Rate limiter 计算错误 | `interval = 1/ceil(tps/batch_size)` — batch_size 越大 interval 越大 |
| 签名串行化 | 每 RPC 1 个 signer，2000 tx × 400µs = 800ms > interval |
| 固定 7 RPC | 单机 7 端点，连接受限 |

## 三、分阶段解决方案

### Phase 1: Hot State Cache 零拷贝（预计 +100% TPS）⬅️ 最高优先

**目标**：消除 snapshot_hot_state() 的 O(n) clone，让 EVM 回到线性区间。

**方案 A — Arc 共享（推荐）**：
```rust
// Before: clone 整个 CachedReads
static HOT_STATE: OnceLock<Mutex<CachedReads>> = OnceLock::new();
pub fn snapshot_hot_state() -> CachedReads {
    cache.clone()  // ← O(n_accounts × storage)
}

// After: Arc 共享 + COW 写时复制
static HOT_STATE: OnceLock<RwLock<Arc<CachedReads>>> = OnceLock::new();
pub fn snapshot_hot_state() -> Arc<CachedReads> {
    hot.read().unwrap().clone()  // ← O(1)，只克隆 Arc 指针
}

// CachedReadsDbMut 持有 Arc<CachedReads> 作为只读后备，
// 写入到本地 overlay（per-build 的 fresh HashMap）。
// Build 完成后，overlay 合并回 HOT_STATE。
```

**方案 B — DashMap 并发读写**：
```rust
use dashmap::DashMap;

pub struct HotState {
    accounts: DashMap<Address, CachedAccount>,
    contracts: DashMap<B256, Bytecode>,
}
// 无需 clone，直接引用。读并发，写锁粒度小。
```

**预期效果**：
- 95K tx 的 EVM 时间从 1,045ms → ~100-150ms（消除 clone + rehash 开销）
- 可以安全将 MAX_TXS_PER_BLOCK 从 40K 提升到 80K-100K
- 立即将单块 TPS 理论上限从 20K 提升到 40K-50K

**修改文件**：
- `reth/crates/revm/src/cached.rs` — Arc 包装 + overlay 模式
- `reth/crates/ethereum/payload/src/lib.rs` — 适配新 API

### Phase 2: 消除 Block Time 膨胀（预计 -0.5s）

**2a: 移除 slot 边界强制对齐**

当前 `next_slot_boundary()` 将下一次构建对齐到 2s 的整数倍。
当块 N 在 T=1.95s 提交时，下一次构建要等到 T=2.0s。

```rust
// Before (consensus_loop.rs:59-62):
let (slot_ts, delay) = self.next_slot_boundary();
let deadline = Instant::now() + delay;

// After: 立即开始，但保证最小间隔
let min_interval = Duration::from_millis(200); // 最小 200ms 间隔防止洪泛
let deadline = Instant::now() + min_interval;
```

**2b: 移除 builder warmup delay**

```rust
// execution_bridge.rs:104-112
// Before: 固定 10ms sleep
// After: 移除。reth 的 best_transactions 自身已经异步等待。
```

**2c: 限制 best_transactions 迭代深度**

reth 的 `resolve_kind(WaitForPending)` 会扫描整个 pending pool。
可以在 payload builder 的 packing 循环中增加更积极的截断：

```rust
// 已有 max_txs_per_block 和 build_time_budget
// 新增：pool 迭代器预取限制
// 在 reth payload builder 中添加 max_iterations 参数
```

**预期效果**：avg_block_time 从 2.5-3.0s → 2.0-2.2s

### Phase 3: 压测工具 v10（预计注入 40K+ TPS）

**3a: 修复 rate limiter**
```rust
// Before: batch_interval = 1/ceil(tps/batch_size)  ← 越大越慢
// After: 基于 per-tx 间隔
let tx_interval_us = 1_000_000 / target_tps;
// 发送后 sleep(batch_size * tx_interval_us)
```

**3b: 多线程预签名**
```rust
// 当前: 1 signer task/RPC, 串行签名
// 改为: 预签名线程池，N 个 worker 持续签名到 buffer
let (sign_tx, sign_rx) = crossbeam::channel::bounded(1000);
for _ in 0..num_cpus() {
    std::thread::spawn(|| loop {
        let batch = create_and_sign_batch();
        sign_tx.send(batch);
    });
}
// sender 从 sign_rx 消费并发送
```

**3c: 增加 RPC 端点到 14+**
```
// 每个节点暴露 2 个 HTTP 端口（主端口 + admin 端口）
// 7 nodes × 2 ports = 14 RPC endpoints
// 或使用 nginx 负载均衡
```

**预期效果**：注入能力从 16.5K → 50K+ TPS

### Phase 4: 1s Slot Time（预计 TPS 翻倍）

当 Phase 1-3 完成后：
- EVM 线性化：80K tx = ~80ms
- Block time 稳定：~1.0-1.2s
- 注入充足：40K+ TPS

此时将 slot time 从 2s → 1s：
- 每块 43K tx → 43K TPS
- Pipeline: 构建 ~300ms + 广播 ~50ms + import ~5ms + consensus ~20ms = ~375ms
- 仍有 625ms 余量

```bash
# testnet.sh
BLOCK_INTERVAL_MS=1000  # 2000 → 1000
```

### Phase 5: 进一步优化（如需 80K+ TPS）

| 方案 | 预期提升 | 复杂度 |
|------|---------|--------|
| Parallel EVM (修复 PEVM) | 2-4x EVM 速度 | 高 |
| State Root 异步化 | 从关键路径移除 SR | 中 |
| 更大 gas limit (5G+) | 提高上限 | 低 |
| 多 leader 分片 | 线性扩展 | 极高 |

## 四、实施优先级与预期

| Phase | 改动 | 预期 sustained TPS | 预计工时 |
|-------|------|-------------------|---------|
| 当前 | - | 13,535 | - |
| **Phase 1** | Hot cache 零拷贝 | **25-30K** | 4-8h |
| Phase 2 | Block time 优化 | 30-35K | 2-4h |
| Phase 3 | 压测 v10 | 35K+ 注入 | 4-6h |
| **Phase 4** | 1s slot | **40-50K** | 1h (config) |
| Phase 5 | Parallel EVM etc | 80K+ | 数天 |

## 五、关键风险

1. **Phase 1 修改 reth 代码**：CachedReads 是 reth 核心组件，Arc 包装需要仔细处理生命周期
2. **Phase 2 移除 slot 对齐**：可能影响多 leader 轮换场景的时序
3. **Phase 4 (1s slot)**：persist 异步 720ms-1.2s，在 1s slot 下可能积压
4. **大块网络传输**：80K tx 块的 zstd 压缩数据约 8-10MB，广播延迟可能增加

## 六、立即开始

Phase 1（Hot State Cache 零拷贝）是最高优先级，因为它：
- 直接消除 EVM 非线性膨胀的根因
- 允许安全提升 MAX_TXS_PER_BLOCK
- 是所有后续优化的前提
