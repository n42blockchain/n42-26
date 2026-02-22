# 开发日志 12 — 全项目代码简化

> 日期：2026-02-22
> 目标：对 n42-26 全部源码、测试、脚本、配置进行系统化简化，提升可维护性

---

## 设计决策

### 1. 大文件拆分策略

三个超大文件采用 Rust 多文件 `impl` 块模式拆分：

| 原文件 | 行数 | 拆分方式 |
|--------|------|----------|
| `state_machine.rs` | 3299 | → proposal/voting/decision/timeout 4 个子模块 + 主文件 |
| `orchestrator.rs` | 1990 | → consensus_loop/execution_bridge/state_mgmt 3 个子模块 + mod.rs |
| `mobile-ffi/lib.rs` | 1618 | → context/transport 2 个子模块 + 主文件 |

**为什么选择多文件 impl 块**：Rust 允许一个类型在多个文件中分别有 `impl` 块，子模块通过 `pub(super)` 暴露方法给父模块。这样保持了类型的完整性，同时使每个文件聚焦于单一职责。

**替代方案**：考虑过 trait 拆分（将方法分散到不同 trait），但这会增加不必要的抽象层且降低 IDE 导航体验。

### 2. 批次执行与模型分级

将全项目分为 10 个批次，按复杂度选择模型：
- **haiku**：数据结构、纯函数、测试文件、脚本（简单文件，节约额度）
- **sonnet**：网络层、共识核心、大文件拆分（需要理解复杂上下文）

最后三个批次（测试 + E2E + 脚本）并行执行，节约时间。

### 3. 简化原则

- 删除冗余注释（与代码/函数名重复的注释）
- 提取重复代码为共享辅助函数
- 应用 clippy 建议（`is_none_or`、`collapsible_if`、`ok_or` 等）
- 不改变公共 API 和行为
- 不修改架构层面的 clippy 警告（`large_enum_variant`、`too_many_arguments`）

---

## 实施细节

### 大文件拆分结构

**consensus/protocol/**
```
state_machine.rs (1888行) — 主协调器：ConsensusEngine struct、构造、事件分发
├── proposal.rs (217行)   — on_block_ready, process_proposal, send_vote
├── voting.rs (221行)     — process_vote, try_form_prepare_qc/commit_qc
├── decision.rs (77行)    — process_decide
└── timeout.rs (257行)    — on_timeout, process_timeout, process_new_view
```

**n42-node/orchestrator/**
```
mod.rs (749行)              — struct 定义、构造、run() 事件循环
├── consensus_loop.rs (303行)     — handle_engine_output, schedule_payload_build
├── execution_bridge.rs (522行)   — do_trigger_payload_build, import_and_notify
└── state_mgmt.rs (339行)        — 状态持久化、P2P 状态同步
```

**mobile-ffi/**
```
lib.rs (~550行)       — FFI 导出函数 + 测试
├── context.rs       — VerifierContext, QuicConnection, VerifyStats
└── transport.rs     — connect_quic, recv_loop, PinnedCertVerification
```

### 关键提取

- `NetworkHandle::send()` 私有辅助方法：消除 9 个方法中重复的 `map_err` 模式
- `build_peer_score_params()`：从 transport 函数中提取重复的 peer score 构建逻辑
- E2E `test_helpers.rs`：`compute_peer_id`/`wait_for_sync`/`cleanup_nodes` 等 6 个公共辅助函数
- `derive_ed25519_keypair()`：消除 main.rs 中两处完全相同的 keypair 派生逻辑

---

## 变更统计

```
82 files changed, 6026 insertions(+), 9886 deletions(-)
净减约 3860 行
```

| 批次 | 范围 | 文件数 | 关键成果 |
|------|------|--------|----------|
| 1 | Foundation (primitives/chainspec/execution) | 11 | 净减 215 行 |
| 2 | Mobile (mobile/mobile-ffi) | 10 | lib.rs 拆分为 3 子模块 |
| 3 | Network | 13 | 提取辅助函数，修复 lint |
| 4 | Consensus Core | 4→9 | state_machine.rs 拆分为 5 文件 |
| 5 | Consensus Remaining | 6 | 精简注释和辅助方法 |
| 6 | Orchestrator | 1→4 | orchestrator.rs 拆分为 4 文件 |
| 7 | Node + bin | 13 | 提取公共辅助函数 |
| 8 | Unit/Integration Tests | 5 | 减少 397 行 |
| 9 | E2E Tests | 13 | 新增 test_helpers.rs |
| 10 | Scripts + Config | 7 | 精简注释和控制流 |

---

## 验证结果

- `cargo check --workspace --all-targets` — 通过
- `cargo clippy --workspace --all-targets` — 零错误
- `cargo test --workspace --lib` — 569 个测试全部通过
- Python/Shell 脚本语法检查 — 通过

---

## 后续计划

- 编译 release 版本替换测试网节点
- 长时间运行验证功能无回退
- 考虑修复剩余的架构级 clippy 警告（`large_enum_variant` 等）
