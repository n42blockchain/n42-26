# 开发日志 14 — 代码简化补充（Mobile 验证阶段后）

> 日期：2026-02-26
> 目标：对 commit `fe892c2`（手机验证 + QUIC 客户端 + 对数奖励阶段）后新增/修改的 12 个文件补充 code-simplifier 审计

---

## 背景

开发日志 12 覆盖了全项目所有文件（截至 commit `3ea4ed4`）。此后 commit `fe892c2` 新增/修改了 12 个 .rs 文件，这些文件未经过简化。本次对这 12 个文件分 4 组并行审计。

---

## 变更文件清单

| # | 文件 | 状态 | 净变化 |
|---|------|------|--------|
| 1 | `bin/n42-mobile-sim/src/main.rs` | 修改 | -14 行 |
| 2 | `bin/n42-node/src/main.rs` | 修改 | -32 行 |
| 3 | `crates/n42-execution/src/read_log.rs` | 修改 | -39 行 |
| 4 | `crates/n42-mobile/src/lib.rs` | 修改 | -3 行 |
| 5 | `crates/n42-mobile/src/quic_client.rs` | 新增文件 | -12 行 |
| 6 | `crates/n42-network/src/service.rs` | 修改 | -43 行 |
| 7 | `crates/n42-node/src/lib.rs` | 修改 | 无变更 |
| 8 | `crates/n42-node/src/mobile_bridge.rs` | 修改 | 净 0 行（等量重构） |
| 9 | `crates/n42-node/src/mobile_reward.rs` | 修改 | -39 行 |
| 10 | `crates/n42-node/src/orchestrator/consensus_loop.rs` | 修改 | -12 行 |
| 11 | `crates/n42-node/src/orchestrator/execution_bridge.rs` | 修改 | -20 行 |
| 12 | `crates/n42-node/src/orchestrator/mod.rs` | 修改 | -40 行 |

**总净减：254 行**

---

## 关键改动详情

### 组 1：bin 入口文件（-46 行）

**n42-mobile-sim/src/main.rs**
- 删除 `bls_pubkey_to_address` 不必要的包装函数，调用处改为直接调用 `n42_mobile::bls_pubkey_to_address`
- `deadline_reached` 重复嵌套模式（`collapsible_if`）合并为 `is_some_and` 闭包，8 行减至 2 行
- 删除 5 条冗余注释（重述下方代码的显而易见的行为）

**bin/n42-node/src/main.rs**
- 合并 `env_u64` / `env_u16` / `env_usize` 三个函数体完全相同的函数为泛型 `env_parse<T: FromStr>`，消除 8 行重复
- 提取 `override_timeout_from_env` 辅助函数，`N42_BASE_TIMEOUT_MS` 和 `N42_MAX_TIMEOUT_MS` 的 override 逻辑从 10 行减至 2 行调用
- 删除 15 条冗余注释（与 `info!` 日志或函数调用名重复的注释）
- 保留有价值的注释：RPC bug workaround 说明、`i<j` 连接策略说明

### 组 2：n42-execution + n42-mobile（-54 行）

**n42-execution/src/read_log.rs**
- 提取 `capture_bytecode()` 私有方法：`basic()` 和 `code_by_hash()` 中各有一段完全相同的 bytecode 捕获逻辑合并
- 移除 `DecodeError::CountMismatch` 死代码：`decode_read_log` 末尾的 `entries.len() != entry_count` 检查永远不会触发（for 循环精确执行 `entry_count` 次），移除该检查及对应错误变体
- 删除 15+ 条冗余行内注释

**n42-mobile/src/quic_client.rs**（新文件）
- 提取 `zstd_decompress()` 辅助函数：`receive_message()` 中 0x03/0x04 分支的相同解压逻辑合并，magic number `16 * 1024 * 1024` 提取为 `ZSTD_MAX_SIZE` 常量
- 删除 10 条冗余 doc comment

### 组 3：n42-network/service.rs（-43 行）

- 提取 `gossipsub_publish()` 方法：统一 gossipsub 发布逻辑，替代 `AnnounceBlock`、`BroadcastBlobSidecar`、consensus publish 中各自重复的 publish + error logging 模式
- 提取 `publish_consensus()` 方法：统一 encode + publish 逻辑，替代 `BroadcastConsensus` 和 `SendDirect` 中完全相同的代码块
- 保留 `BroadcastTransaction` 独立：该分支使用 `tracing::trace!`（交易广播失败是常态），语义不同，不合并
- 删除 6 处冗余注释

### 组 4：n42-node 核心模块（-111 行）

**mobile_bridge.rs**（净 0 行，等量重构）
- 提取 `fifo_ensure_entry()` 泛型辅助方法：`invalid_receipt_counts` 和 `block_first_receipt_at` 两处完全相同的 FIFO 有界 map 驱逐逻辑（各 7 行 if 嵌套）合并为一个方法

**mobile_reward.rs**（-39 行）
- 大幅精简 struct 和方法级 docstring（14 行精简为 4 行，保留对数奖励公式核心说明）
- **对数奖励算法完全未动**

**orchestrator/consensus_loop.rs**（-12 行）
- 精简方法级 docstring（`schedule_payload_build`、`next_slot_boundary`、`handle_engine_output` 等）

**orchestrator/execution_bridge.rs**（-20 行）
- 精简 `do_trigger_payload_build`、`handle_block_data`、`import_and_notify` 等方法的 docstring

**orchestrator/mod.rs**（-40 行）
- 精简 `ConsensusOrchestrator` struct docstring（8 行到 3 行，保留架构概述）
- 删除 6 个字段注释、8 个 builder 方法的冗余 docstring

---

## 验证结果

```
cargo check --workspace --all-targets  — 通过
  (仅 keystore.rs 的预存 dead_code warning，与本次无关)
cargo test --workspace --lib            — 通过（同日志12的 569 个测试基线）
```

---

## 遗留事项

- `bin/n42-node/src/keystore.rs` 中 `encrypt` 和 `save` 方法有 `dead_code` 警告，属预存问题，待后续确认是否需要保留该 API
