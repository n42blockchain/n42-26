# 开发日志 38 — Hot State 零拷贝与 Timeout Collector Bug 修复

## 日期: 2026-03-09

## 一、Hot State Cache 零拷贝（Phase 1 完成）

### 1.1 设计决策

**问题**：`snapshot_hot_state()` 每次构建 payload 时全量 clone `CachedReads`（O(n_accounts × avg_storage)），随着状态增长导致 EVM 非线性膨胀（30K tx=32ms → 95K tx=1045ms）。

**方案选择**：Arc 共享 + COW（选择方案 A，放弃 DashMap 方案 B）
- Arc 共享：`snapshot_hot_state()` 从 O(n) clone 变为 O(1) Arc::clone
- 三层查找：overlay（per-build 可变）→ hot_backing（Arc 共享只读）→ DB（libmdbx）
- COW 写入：`update_hot_state()` 使用 `Arc::make_mut()` 写时复制

### 1.2 实现细节

**修改文件**：
- `reth/crates/revm/src/cached.rs` — 核心零拷贝实现
- `reth/crates/ethereum/payload/src/lib.rs` — payload builder 适配新 API

**关键结构变化**：
```rust
// HOT_STATE 从 Mutex<CachedReads> 改为 RwLock<Arc<CachedReads>>
static HOT_STATE: OnceLock<RwLock<Arc<CachedReads>>> = OnceLock::new();

// CachedReadsDbMut 新增 hot_backing 字段
pub struct CachedReadsDbMut<'a, DB> {
    pub cached: &'a mut CachedReads,      // per-build overlay
    pub hot_backing: Option<Arc<CachedReads>>,  // shared snapshot
    pub db: DB,
}
```

**三层查找策略（以 basic() 为例）**：
1. 先查 overlay（per-build HashMap）
2. 未命中则查 hot_backing（Arc 共享）→ 命中后提升到 overlay
3. 最后查 DB → 结果存入 overlay

**code_by_hash() 特殊处理**：找到 hot_backing 中的 bytecode 后直接返回，不提升到 overlay（避免大对象重复存储）

### 1.3 测试

3 个单元测试全部通过：
- `test_extend_with_two_cached_reads` — 原有测试
- `test_hot_backing_basic_lookup` — 验证 hot backing 查找和提升
- `test_hot_backing_code_not_promoted` — 验证 bytecode 不提升

## 二、Timeout Collector 重建 Bug（链 Stall 根因）

### 2.1 问题发现

零拷贝实现后压测（N42_MAX_TXS_PER_BLOCK=80000）出现链 stall：
- Step 15K: avg_block_time=6.0s
- Step 20K: blocks=0（完全停滞）

通过节点日志分析发现：
- 链在 view 424 永久卡死
- 所有 7 个节点都在 "view timed out (repeat, resetting pacemaker only)" 循环
- View change 协议无法推进到 view 425

**各节点同步状态**：
| 节点 | latest_block | 状态 |
|------|-------------|------|
| Validator 1 | 417 | 正常 |
| Validator 2 | 417 | 正常 |
| Validator 3 | 417 | 正常 |
| Validator 4 | 405 | 落后 12 块 |
| Validator 5 | 406 | 落后 11 块 |
| Validator 6 | 417 | 正常 |
| Validator 7 | - | 无数据 |

Validator-4 是 view 424 的 leader，但它在 block 405，无法出块。

### 2.2 根因分析

**Bug 1：`on_timeout()` 无条件重建 timeout collector**

`crates/n42-consensus/src/protocol/timeout.rs:42`：
```rust
// BUG: 丢弃已收集的投票
self.timeout_collector = Some(TimeoutCollector::new(view, n_validators));
```

从 validator-1 日志追踪时序：
1. 收到 sender 3,2,6 的 timeout → collector 已有 3 票
2. 自己超时 → `on_timeout()` 重建 collector → **丢失 3 票**
3. 新 collector 收到 sender 1,4,5,0 → 只有 4 票
4. Quorum=5（7 节点 BFT 需要 ⌈2n/3⌉+1=5），差 1 票

先超时的 3 个验证器已进入 TimedOut 状态，repeat timeout 不再重新广播 → **永远无法达到 quorum**。

**Bug 2：repeat timeout 不重新广播**

重复超时只重置 pacemaker 定时器，不重新发送 timeout 消息。如果有验证器因网络延迟错过了第一次广播，将永远无法收到该投票。

### 2.3 修复

**Bug 1 修复**：将无条件替换改为条件插入
```rust
// 修复: 保留已收集的投票
let n_validators = self.validator_set().len();
self.timeout_collector
    .get_or_insert_with(|| TimeoutCollector::new(view, n_validators));
```

**Bug 2 修复**：repeat timeout 时重新广播 + 重检 quorum
```rust
if self.round_state.phase() == Phase::TimedOut {
    // 重新广播 timeout 消息
    self.emit(EngineOutput::BroadcastMessage(
        ConsensusMessage::Timeout(timeout_msg),
    ))?;
    // 重检 quorum — 等待期间可能收到了新投票
    if let Some(ref collector) = self.timeout_collector {
        if collector.has_quorum(quorum_size) && next_leader == self.my_index {
            self.try_form_tc_and_advance(view, next_view)?;
        }
    }
    return Ok(());
}
```

**安全性**：`advance_to_view()` 会清除 `timeout_collector = None`，所以 `get_or_insert_with` 只会保留当前 view 的投票，不会跨 view 泄漏。

### 2.4 与零拷贝的关系

**零拷贝实现没有 bug**。链 stall 是共识层的 timeout 处理 bug，在以下条件同时满足时触发：
1. 某些验证器落后（无法出块作为 leader）
2. 需要通过 view change 跳过落后 leader
3. Timeout collector 因重建丢弃了足够多的投票
4. 这在之前较少发生是因为 block 较小、所有验证器能跟上

80K tx 块使得部分验证器无法跟上 import 速度（validator-4 落后 12 块），暴露了这个潜在 bug。

## 三、下一步

1. **重新压测验证**：timeout bug 修复 + 零拷贝，测试 80K tx 块是否稳定
2. **观察 EVM 线性性**：零拷贝消除了 O(n) clone，40K-80K tx 块的 EVM 时间应回到线性区间
3. **Phase 2**：Block time 优化（slot 边界对齐、pool 迭代深度）
