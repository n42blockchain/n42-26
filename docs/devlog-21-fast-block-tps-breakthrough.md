# 开发日志-21：快速出块与 TPS 突破 — 15s → 1s 出块，680 → 1667 TPS

> 日期：2026-03-06
> 目标：消除 15s 出块间隔，实现连续 1s 出块，突破 1500 TPS

---

## 背景

Phase 1-2 优化后，block time 从 8s 降到约 4s，但实际运行中出现 15s 的出块间隔。
原因是共识引擎在 commit 后没有立即通知 orchestrator 视图已切换，
导致 leader 等待 pacemaker 超时（15s）才触发下一次构建。

---

## 根因分析

### 问题 1：ViewChanged 事件缺失
`try_form_commit_qc` 和 `process_decide` 在提交区块后调用 `advance_to_view(next_view)`，
但没有发出 `EngineOutput::ViewChanged`。Orchestrator 的 `handle_view_changed` 从不触发，
leader 只能等 pacemaker 超时。

### 问题 2：fork_choice_updated 时间戳竞争
快速连续出块时，`last_committed_timestamp` 跟踪滞后于 reth 内部 head，
导致 FCU 返回 "invalid payload attributes"（时间戳 <= head.timestamp）。

### 问题 3：并发后台导入竞争
View N+1 的 `new_payload` 在 View N 导入完成前发送，reth 返回 `Syncing`
（父块未知）。并发 spawn 导致导入顺序不确定。

### 问题 4：JSON 时间戳提取不可靠
通过 `serde_json::Value::get("timestamp")?.as_str()` 解析执行载荷 JSON
提取时间戳经常失败或得到错误值，导致 `last_committed_timestamp` 始终落后 1。

---

## 解决方案

### 修改 1：共识引擎发出 ViewChanged（voting.rs, decision.rs）

在 `try_form_commit_qc` 和 `process_decide` 中，commit/decide 后立即发出 ViewChanged：

```rust
self.advance_to_view(next_view)?;
let actual_view = self.round_state.current_view();
self.emit(EngineOutput::ViewChanged { new_view: actual_view })
```

### 修改 2：FCU 重试机制（execution_bridge.rs）

FCU 失败时以 `last_committed_timestamp + 2` 重试一次：

```rust
for attempt in 0..2u8 {
    let try_attrs = if attempt == 0 { attrs.clone() } else {
        let bumped_ts = self.last_committed_timestamp.max(attrs.timestamp) + 2;
        let mut retry_attrs = attrs.clone();
        retry_attrs.timestamp = bumped_ts;
        retry_attrs
    };
    match beacon_engine.fork_choice_updated(fcu_state, Some(try_attrs), ...).await {
        Ok(result) => { /* success */ break; }
        Err(e) => { last_err = Some(e); }
    }
}
```

### 修改 3：串行后台导入队列（consensus_loop.rs, mod.rs）

用 `bg_import_in_flight` + `bg_import_queue: VecDeque` 替代并发 spawn：

- `enqueue_bg_import`: 若有导入在运行则入队，否则立即 spawn
- `handle_import_done`: 完成后出队下一个，或者如果队列空且自己是 leader 则立即触发构建
- 父块失败时清空队列并触发 sync

### 修改 4：BlockDataBroadcast 直接存储时间戳（mod.rs）

```rust
pub(crate) struct BlockDataBroadcast {
    pub(crate) block_hash: B256,
    pub(crate) view: u64,
    pub(crate) payload_json: Vec<u8>,
    #[serde(default)]
    pub(crate) timestamp: u64,  // 新增
}
```

Leader 构建时从 `payload.block().header().timestamp` 取值，
follower 直接使用此字段，避免 JSON 解析。

### 修改 5：保留 pending_block_data 跨视图（consensus_loop.rs）

`handle_view_changed` 不再清空 `pending_block_data`。
在快速共识中，block data 常在 Decide 之前到达；清空会导致
`finalize_committed_block` 进入 Case C（延迟终结），触发级联超时。

### 修改 6：leader_payload_rx 恢复（consensus_loop.rs）

`finalize_committed_block` 在 Case C 前先尝试 `leader_payload_rx.try_recv()`
恢复自己的 block data，避免因 select loop 顺序问题丢失数据。

---

## 改动文件

| 文件 | 改动 |
|------|------|
| `crates/n42-consensus/src/protocol/voting.rs` | ViewChanged after commit |
| `crates/n42-consensus/src/protocol/decision.rs` | ViewChanged after Decide |
| `crates/n42-node/src/orchestrator/mod.rs` | timestamp 字段、import 队列字段 |
| `crates/n42-node/src/orchestrator/execution_bridge.rs` | FCU 重试、直接时间戳 |
| `crates/n42-node/src/orchestrator/consensus_loop.rs` | 串行队列、数据恢复 |
| `crates/n42-node/src/orchestrator/observer.rs` | timestamp: 0 兼容 |
| `crates/n42-node/src/orchestrator/state_mgmt.rs` | timestamp: 0 兼容 |

---

## 测试结果（7 节点本地测试网，MacBook M 系列）

| 目标 TPS | 实际 TPS | 失败率 | 峰值 tx/block |
|----------|----------|--------|--------------|
| 500 | 474 | 0.2% | — |
| 1000 | 910 | 0.0% | — |
| 2000 | 1667 | 0.0% | 23,809 |

- 出块时间：稳定 1.0s（从 15s 降至 1s）
- Gas 利用率：500M gas limit 下峰值 100%
- FCU 错误：正常负载下为 0

---

## 关键经验

1. **共识-执行耦合**是 TPS 瓶颈的核心。ViewChanged 事件是连接两者的关键纽带。
2. **时间戳跟踪**必须可靠。JSON 解析在生产环境中不可靠，直接结构体字段才是正确方案。
3. **串行导入**比并发安全得多。区块链的父子依赖天然要求顺序处理。
4. **FCU 重试**是必要的容错机制，时间戳竞争在快速出块时不可避免。
