# devlog-54: Commit-then-Activate 动态验证者集变更协议

> 日期：2026-03-21
> 分支：dev2603
> 类别：Feature / Consensus Safety

---

## 背景

当前 n42 共识仅支持静态 `epoch_schedule.json` 驱动的验证者集变更（启动前配置）。
为支持节点运行时通过共识投票动态增减验证者，实现完整的 Commit-then-Activate 协议，对应 HotStuff-2 §5 + Jolteon §4.3。

---

## 设计决策

### 方案选择

| 方案 | 说明 | 结论 |
|------|------|------|
| 方案 A（本方案）| 复用已有 `stage_next_epoch / advance_epoch` 机制，新增提案队列，在 CommitQC 时调用 `commit_pending_changes` | **采用** |
| 方案 B | 新增独立的 `ValidatorChangeQC` 消息类型，通过专门的共识轮次处理变更 | 过于复杂，引入新消息类型和额外的共识轮次 |
| 方案 C | 直接写入 `epoch_schedule.json`，重启激活 | 不满足运行时动态变更需求 |

选择方案 A 的原因：
1. `stage_next_epoch / advance_epoch` 已有测试和持久化支持（`scheduled_epoch_transition` 字段）
2. 不需要新消息类型，不影响已有协议消息格式
3. CommitQC 是自然的"原子提交点"，确保所有节点同步触发变更

### 安全约束（实现层面保证）

- **最小集合**：`MIN_VALIDATOR_COUNT = 4`，即 f≥1，保证 BFT 安全性
- **Quorum Overlap**：新旧集合地址交集 ≥ `current quorum_size`（2f+1），满足 Jolteon §4.3 活性
- **确定性排序**：新集合按 `Address` 字典序排列，保证所有节点计算结果一致
- **原子性**：`propose_*` 只入队，`commit_pending_changes` 在 CommitQC 时一次性 validate + stage

### 7节点场景验证

| 操作 | 新集合 | 新 f | quorum | overlap | 结果 |
|------|--------|------|--------|---------|------|
| 7→6（移除1） | 6 | 1 | 4 | 6≥5✓ | 允许 |
| 6→5（移除1） | 5 | 1 | 4 | 5≥4✓ | 允许 |
| 5→4（移除1） | 4 | 1 | 3 | 4≥3✓ | 允许（最小集合） |
| 4→3（移除1） | 3 | 0 | 1 | — | 拒绝（低于最小值4） |

---

## 实施细节

### 文件改动

#### `crates/n42-consensus/src/error.rs`
新增 4 个错误变体：
```rust
InsufficientValidators { have: usize, need: usize }
InsufficientQuorumOverlap { have: usize, need: usize }
ValidatorAlreadyExists { address: Address }
ValidatorNotFound { address: Address }
```

#### `crates/n42-consensus/src/validator/epoch.rs`
- `EpochManager` 新增字段 `pending_adds: Vec<ValidatorInfo>`, `pending_removes: Vec<Address>`
- 新增常量 `MIN_VALIDATOR_COUNT = 4`
- 新增方法：
  - `propose_add_validator` — 检查不重复（current set + pending_adds）后入队
  - `propose_remove_validator` — 检查存在性、移除后数量 ≥ MIN_VALIDATOR_COUNT、不重复后入队
  - `has_pending_changes` — 检查队列是否非空
  - `commit_pending_changes` — 计算 new_validators（sort by address）→ `validate_transition` → `stage_next_epoch` → 清空队列
  - `validate_transition`（私有）— 最小数量 + Jolteon quorum overlap 检查

#### `crates/n42-consensus/src/protocol/voting.rs`
`try_form_commit_qc()` 中 `round_state.commit()` 之后插入：
```rust
if self.epoch_manager.has_pending_changes() {
    self.epoch_manager.commit_pending_changes()?;
}
```

#### `crates/n42-consensus/src/protocol/decision.rs`
`process_decide()` 中 `round_state.commit()` 之后插入同样的 3 行 hook。

#### `crates/n42-consensus/src/protocol/state_machine.rs`
暴露两个公开方法：
```rust
pub fn propose_add_validator(&mut self, info: ValidatorInfo) -> ConsensusResult<()>
pub fn propose_remove_validator(&mut self, addr: Address) -> ConsensusResult<()>
```

### 模块依赖
```
orchestrator/consensus_loop.rs (调用 propose_*)
    → ConsensusEngine::propose_add/remove_validator
        → EpochManager::propose_*  [入队]

CommitQC 形成时（voting.rs / decision.rs）
    → epoch_manager.commit_pending_changes()
        → validate_transition()    [安全检查]
        → stage_next_epoch()       [复用已有机制]

epoch 边界（advance_to_view）
    → advance_epoch()              [激活新集合]
    → EpochTransition event        [通知 orchestrator]
```

### 不变的部分
- `persistence.rs`：`scheduled_epoch_transition` 字段自动持久化 staged 变更
- `orchestrator/consensus_loop.rs`：EpochTransition 处理逻辑无需改动
- 协议消息格式：不引入任何新消息类型

---

## 遇到的问题及解决方案

### 1. 结构体字段初始化
在添加 `pending_adds` / `pending_removes` 字段后，需要更新所有 4 个构造函数（`new`, `with_epoch_length`, `from_epoch`, `from_schedule`）。直接逐一添加 `Vec::new()` 初始化即可。

### 2. 测试中 `validate_transition` 需要可变引用
`validate_transition` 只读取 `self.current_set`，设计为 `&self` 方法，避免不必要的可变引用，测试中可以直接在非 mut 的 `EpochManager` 上调用。

### 3. unused_mut 警告
`test_validate_transition_quorum_overlap` 测试中 `let mut em` 应为 `let em`（`validate_transition` 为 `&self` 方法），编译器给出警告后直接修复。

---

## 阶段完成状态

- [x] `error.rs`：4 个新错误变体
- [x] `epoch.rs`：pending 字段、MIN_VALIDATOR_COUNT、5 个新方法
- [x] `voting.rs`：CommitQC hook（leader 路径）
- [x] `decision.rs`：CommitQC hook（follower 路径）
- [x] `state_machine.rs`：公开 API
- [x] 8 个新测试全部通过（epoch 模块共 18 个测试）
- [x] 完整测试套件通过（219 tests: 65 integration + 136 unit + 18 epoch）
- [x] 零编译警告

---

## 后续计划

1. **orchestrator 集成**：在 `consensus_loop.rs` 中提供 RPC/Admin 接口触发 `propose_add/remove_validator`
2. **持久化提案队列**：当前 `pending_adds/removes` 为 in-memory，节点重启后清空；如需跨重启保留提案，需写入持久化状态
3. **治理层**：未来可在智能合约中实现投票逻辑，通过 EVM 事件触发 orchestrator 调用 `propose_*`
4. **E2E 测试场景**：新增 E2E 场景验证 7→6→5→4→5（移除后添加回来）完整流程
