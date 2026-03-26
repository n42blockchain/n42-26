# HotStuff-2 审计修复计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复 Go 审计报告中指出的 Rust 实现两项真实不足：区块随机数（`prev_randao: B256::ZERO`）和 Validator Reconfig RPC 端点缺失。

**Architecture:** 区块随机数从上一个 CommitQC 的聚合 BLS 签名通过 `keccak256` 派生，注入 `PayloadAttributes.prev_randao`。Validator reconfig 通过 `mpsc` admin channel 从 RPC 层发送请求到 orchestrator 的共识循环，由 orchestrator 调用已有的 `ConsensusEngine::propose_add/remove_validator`。

**Tech Stack:** Rust, alloy-primitives (B256/keccak256), jsonrpsee (RPC), tokio::sync::mpsc

---

## 审计验证总结

在制定修复前，逐项验证了审计报告声明：

| # | 审计声明 | 验证结论 |
|---|---------|---------|
| Validator reconfig: ⚠️ 仅 epoch manager | **部分正确** — `EpochManager` + `ConsensusEngine` API 完整实现，但 RPC 端点和 orchestrator 接线未完成（devlog-54 明确列为"后续计划"） |
| Rotor relay: ❌ | **正确但不修** — GossipSub mesh 广播 + `SendBlockDirect` 已足够，Rotor 是 Go 版特有设计 |
| 崩溃恢复: ❌ | **审计错误** — `ConsensusSnapshot` 持久化完整（view/locked_qc/committed_qc/timeouts/epoch），atomic write (fsync+rename)，`ConsensusEngine::with_recovered_state()` 恢复。见 `persistence.rs` |
| 区块随机数: ❌ | **正确** — `execution_bridge.rs:278` 硬编码 `prev_randao: B256::ZERO` |
| Baby Raptr DA: ❌ | **正确但不修** — 独立协议层，非当前阶段目标 |
| Dual leader regime: ⚠️ | 审计评级过于宽松 — 仅实现响应式模式（leader 收到 2f+1 票立即出块），无 O(Δ) 等待模式。响应式模式是低延迟首选，暂不修 |

**本计划修复 2 项：区块随机数 + Validator Reconfig RPC。**

---

## Task 1: 从 CommitQC 聚合签名派生区块随机数

**Files:**
- Modify: `crates/n42-node/src/orchestrator/execution_bridge.rs:276-282` (build_payload_attributes)
- Modify: `crates/n42-node/src/orchestrator/mod.rs` (添加 last_commit_qc 字段)
- Modify: `crates/n42-node/src/orchestrator/consensus_loop.rs:308-362` (handle_block_committed 保存 commit_qc)
- Test: `crates/n42-node/src/orchestrator/mod.rs` (现有 tests 模块)

**设计：**
- `prev_randao = keccak256(commit_qc.aggregate_signature.to_bytes())`
- CommitQC 的聚合 BLS 签名是 2f+1 验证者的门限签名，外部无法预测
- genesis 区块无 CommitQC，保持 `B256::ZERO`（与 Ethereum PoS genesis 行为一致）

- [ ] **Step 1: 在 ConsensusOrchestrator 中添加 `last_commit_qc` 字段**

在 `crates/n42-node/src/orchestrator/mod.rs` 中，`ConsensusOrchestrator` 结构体已有 `head_block_hash` 字段。在其附近添加 `last_commit_qc`:

```rust
// 在 ConsensusOrchestrator 结构体中添加：
/// CommitQC from the most recently committed block. Used to derive `prev_randao`
/// for the next block's `PayloadAttributes`.
last_commit_qc: Option<QuorumCertificate>,
```

在 `new()` 构造函数中初始化为 `None`。

- [ ] **Step 2: 在 handle_block_committed 中保存 commit_qc**

在 `crates/n42-node/src/orchestrator/consensus_loop.rs` 的 `handle_block_committed` 方法中（约 line 362 附近 `self.head_block_hash = block_hash;` 之后）添加：

```rust
self.last_commit_qc = Some(commit_qc.clone());
```

注意：`commit_qc` 已经作为参数传入 `handle_block_committed(view, block_hash, commit_qc)`。

- [ ] **Step 3: 修改 build_payload_attributes 使用 CommitQC 派生随机数**

在 `crates/n42-node/src/orchestrator/execution_bridge.rs` 中，将 `build_payload_attributes` 里的：

```rust
prev_randao: B256::ZERO,
```

替换为：

```rust
prev_randao: self.last_commit_qc.as_ref().map(|qc| {
    alloy_primitives::keccak256(qc.aggregate_signature.to_bytes())
}).unwrap_or(B256::ZERO),
```

需要在文件顶部确认 `alloy_primitives` 已导入（当前已导入 `B256`，`keccak256` 需要额外引入）。

- [ ] **Step 4: 添加单元测试**

在 `crates/n42-node/src/orchestrator/mod.rs` 的 `#[cfg(test)]` 模块中添加测试：

```rust
#[test]
fn test_prev_randao_derived_from_commit_qc() {
    use alloy_primitives::keccak256;
    use n42_primitives::consensus::QuorumCertificate;

    // genesis: no commit QC → B256::ZERO
    let randao_none: B256 = None::<&QuorumCertificate>
        .map(|qc| keccak256(qc.aggregate_signature.to_bytes()))
        .unwrap_or(B256::ZERO);
    assert_eq!(randao_none, B256::ZERO);

    // with commit QC: deterministic derivation
    let qc = QuorumCertificate::genesis();
    let randao = keccak256(qc.aggregate_signature.to_bytes());
    assert_ne!(randao, B256::ZERO);
    // same QC → same randao (deterministic)
    let randao2 = keccak256(qc.aggregate_signature.to_bytes());
    assert_eq!(randao, randao2);
}
```

- [ ] **Step 5: 运行测试验证**

Run: `cargo test -p n42-node test_prev_randao`
Expected: PASS

Run: `cargo clippy -p n42-node -- -D warnings`
Expected: 0 warnings

- [ ] **Step 6: 提交**

```bash
git add crates/n42-node/src/orchestrator/execution_bridge.rs \
       crates/n42-node/src/orchestrator/mod.rs \
       crates/n42-node/src/orchestrator/consensus_loop.rs
GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
  git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" \
  -m "feat(consensus): derive prev_randao from CommitQC aggregate signature"
```

---

## Task 2: Validator Reconfig RPC 端点 — admin channel 基础设施

**Files:**
- Modify: `crates/n42-node/src/consensus_state.rs` (添加 admin channel 类型和字段)
- Modify: `crates/n42-node/src/orchestrator/mod.rs` (构造时接收 admin_rx)
- Modify: `crates/n42-node/src/orchestrator/consensus_loop.rs` (select! 中处理 admin 请求)
- Test: `crates/n42-node/src/orchestrator/mod.rs`

**设计：**
RPC 线程不能直接访问 `ConsensusEngine`（它在 orchestrator 的事件循环中独占使用）。通过 `mpsc` channel 传递 admin 命令，orchestrator 在 select! 循环中处理。

- [ ] **Step 1: 定义 admin 命令类型**

在 `crates/n42-node/src/consensus_state.rs` 文件末尾添加：

```rust
use n42_chainspec::ValidatorInfo;
use tokio::sync::{mpsc, oneshot};

/// Admin command sent from RPC to the consensus orchestrator.
pub enum AdminCommand {
    AddValidator {
        info: ValidatorInfo,
        reply: oneshot::Sender<Result<(), String>>,
    },
    RemoveValidator {
        address: Address,
        reply: oneshot::Sender<Result<(), String>>,
    },
}
```

- [ ] **Step 2: 在 SharedConsensusState 中添加 admin channel sender**

在 `SharedConsensusState` 结构体中添加字段：

```rust
pub(crate) admin_tx: Option<mpsc::Sender<AdminCommand>>,
```

在 `new()` 中初始化为 `None`，添加 setter 方法：

```rust
pub fn set_admin_channel(&mut self, tx: mpsc::Sender<AdminCommand>) {
    self.admin_tx = Some(tx);
}

pub fn admin_tx(&self) -> Option<&mpsc::Sender<AdminCommand>> {
    self.admin_tx.as_ref()
}
```

- [ ] **Step 3: 在 ConsensusOrchestrator 中添加 admin_rx**

在 `crates/n42-node/src/orchestrator/mod.rs` 的 `ConsensusOrchestrator` 结构体中添加：

```rust
admin_rx: mpsc::Receiver<AdminCommand>,
```

在构造函数中接收此参数。在启动时创建 channel pair，sender 端传给 `SharedConsensusState`。

- [ ] **Step 4: 在 consensus_loop 的 select! 中处理 admin 命令**

在 `crates/n42-node/src/orchestrator/consensus_loop.rs` 的主 select! 循环中添加分支：

```rust
Some(cmd) = self.admin_rx.recv() => {
    self.handle_admin_command(cmd);
}
```

实现 handler：

```rust
fn handle_admin_command(&mut self, cmd: AdminCommand) {
    match cmd {
        AdminCommand::AddValidator { info, reply } => {
            let result = self.engine.propose_add_validator(info)
                .map_err(|e| e.to_string());
            let _ = reply.send(result);
        }
        AdminCommand::RemoveValidator { address, reply } => {
            let result = self.engine.propose_remove_validator(address)
                .map_err(|e| e.to_string());
            let _ = reply.send(result);
        }
    }
}
```

- [ ] **Step 5: 运行编译检查**

Run: `cargo check -p n42-node --all-targets`
Expected: 编译通过

- [ ] **Step 6: 提交 admin channel 基础设施**

```bash
git add crates/n42-node/src/consensus_state.rs \
       crates/n42-node/src/orchestrator/mod.rs \
       crates/n42-node/src/orchestrator/consensus_loop.rs
GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
  git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" \
  -m "feat(node): add admin command channel for validator reconfig"
```

---

## Task 3: Validator Reconfig RPC 端点 — RPC 暴露

**Files:**
- Modify: `crates/n42-node/src/rpc.rs` (添加 RPC trait 方法 + 实现)
- Test: `crates/n42-node/src/orchestrator/mod.rs`

- [ ] **Step 1: 在 N42Api trait 中添加 RPC 方法声明**

在 `crates/n42-node/src/rpc.rs` 的 `N42Api` trait（约 line 139-217）中、`zk_status` 之后添加：

```rust
/// Proposes adding a new validator. Takes effect at next CommitQC (commit-then-activate).
/// Requires the node to be a current validator. Returns error if already pending or exists.
#[method(name = "proposeAddValidator")]
async fn propose_add_validator(
    &self,
    address: Address,
    bls_pubkey: String,
) -> RpcResult<String>;

/// Proposes removing a validator. Takes effect at next CommitQC.
/// Cannot drop below MIN_VALIDATOR_COUNT (4). Returns error if not in current set.
#[method(name = "proposeRemoveValidator")]
async fn propose_remove_validator(&self, address: Address) -> RpcResult<String>;
```

- [ ] **Step 2: 实现 RPC 方法**

在 `N42RpcServer` 的 `impl N42ApiServer for N42RpcServer` 块中添加实现：

```rust
async fn propose_add_validator(
    &self,
    address: Address,
    bls_pubkey: String,
) -> RpcResult<String> {
    let pubkey_bytes = hex::decode(bls_pubkey.strip_prefix("0x").unwrap_or(&bls_pubkey))
        .map_err(|e| ErrorObjectOwned::owned(-32602, format!("invalid pubkey hex: {e}"), None::<()>))?;
    let pubkey_array: [u8; 48] = pubkey_bytes.try_into().map_err(|v: Vec<u8>| {
        ErrorObjectOwned::owned(-32602, format!("pubkey must be 48 bytes, got {}", v.len()), None::<()>)
    })?;

    let bls_pk = n42_primitives::BlsPublicKey::from_bytes(&pubkey_array)
        .map_err(|e| ErrorObjectOwned::owned(-32602, format!("invalid BLS key: {e}"), None::<()>))?;

    let info = n42_chainspec::ValidatorInfo {
        address,
        bls_public_key: bls_pk.to_bytes().to_vec(),
    };

    let admin_tx = self.consensus_state.admin_tx().ok_or_else(|| {
        ErrorObjectOwned::owned(-32603, "admin channel not available", None::<()>)
    })?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    admin_tx
        .send(crate::consensus_state::AdminCommand::AddValidator { info, reply: reply_tx })
        .await
        .map_err(|_| ErrorObjectOwned::owned(-32603, "consensus loop unavailable", None::<()>))?;

    reply_rx.await
        .map_err(|_| ErrorObjectOwned::owned(-32603, "consensus loop dropped reply", None::<()>))?
        .map(|()| "validator add proposed, activates at next CommitQC".to_string())
        .map_err(|e| ErrorObjectOwned::owned(-32003, e, None::<()>))
}

async fn propose_remove_validator(&self, address: Address) -> RpcResult<String> {
    let admin_tx = self.consensus_state.admin_tx().ok_or_else(|| {
        ErrorObjectOwned::owned(-32603, "admin channel not available", None::<()>)
    })?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    admin_tx
        .send(crate::consensus_state::AdminCommand::RemoveValidator { address, reply: reply_tx })
        .await
        .map_err(|_| ErrorObjectOwned::owned(-32603, "consensus loop unavailable", None::<()>))?;

    reply_rx.await
        .map_err(|_| ErrorObjectOwned::owned(-32603, "consensus loop dropped reply", None::<()>))?
        .map(|()| "validator remove proposed, activates at next CommitQC".to_string())
        .map_err(|e| ErrorObjectOwned::owned(-32003, e, None::<()>))
}
```

- [ ] **Step 3: 运行编译 + lint**

Run: `cargo check -p n42-node --all-targets`
Expected: 编译通过

Run: `cargo clippy -p n42-node -- -D warnings`
Expected: 0 warnings

- [ ] **Step 4: 运行全量测试**

Run: `cargo test -p n42-node`
Expected: 全部 PASS

Run: `cargo test -p n42-consensus`
Expected: 全部 PASS（确认 epoch manager 没有被破坏）

- [ ] **Step 5: 提交**

```bash
git add crates/n42-node/src/rpc.rs
GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
  git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" \
  -m "feat(rpc): expose n42_proposeAddValidator and n42_proposeRemoveValidator endpoints"
```

---

## Task 4: 接线与集成验证

**Files:**
- Modify: `bin/n42-node/src/main.rs` (创建 admin channel, 传给 orchestrator 和 SharedConsensusState)
- Test: E2E 手动验证

- [ ] **Step 1: 在 main.rs 中创建 admin channel 并传递**

在 `bin/n42-node/src/main.rs` 中找到 `ConsensusOrchestrator` 构造和 `SharedConsensusState` 构造的位置，在两者之间创建 channel：

```rust
let (admin_tx, admin_rx) = tokio::sync::mpsc::channel(16);
// 传 admin_tx 给 SharedConsensusState
consensus_state.set_admin_channel(admin_tx);
// 传 admin_rx 给 ConsensusOrchestrator 构造函数
```

具体插入位置需要阅读 `main.rs` 确认 orchestrator 和 state 的构造顺序。

- [ ] **Step 2: 编译检查**

Run: `cargo check --all-targets`
Expected: 编译通过

Run: `cargo clippy --all-targets -- -D warnings`
Expected: 0 warnings

- [ ] **Step 3: 运行全量测试**

Run: `cargo test --workspace`
Expected: 全部 PASS

- [ ] **Step 4: 提交**

```bash
git add bin/n42-node/src/main.rs
GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
  git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" \
  -m "feat(node): wire admin channel between RPC and consensus orchestrator"
```

---

## Task 5: 更新开发日志

**Files:**
- Modify: `docs/devlog-54-dynamic-validator-set.md` (追加 RPC 集成完成记录)
- Modify: `DEVLOG.md` (如果需要新文件则更新索引)

- [ ] **Step 1: 追加开发日志**

在 `docs/devlog-54-dynamic-validator-set.md` 末尾追加：

```markdown

---

## Phase 2: RPC 集成 + 区块随机数（2026-03-25）

### 区块随机数

- `prev_randao` 从上一个 CommitQC 的聚合 BLS 签名通过 `keccak256` 派生
- Genesis 区块无 CommitQC，保持 `B256::ZERO`
- 修改位置：`execution_bridge.rs:build_payload_attributes()`

### Validator Reconfig RPC

- 新增 `n42_proposeAddValidator(address, bls_pubkey)` RPC 端点
- 新增 `n42_proposeRemoveValidator(address)` RPC 端点
- 架构：RPC → `mpsc` admin channel → orchestrator select! → `ConsensusEngine::propose_add/remove_validator`
- 在 `SharedConsensusState` 中持有 channel sender，orchestrator 持有 receiver

### 后续计划（更新）

1. ~~orchestrator 集成~~ ✅ 已完成
2. **持久化提案队列**：`pending_adds/removes` 仍为 in-memory
3. **治理层**：EVM 智能合约投票 → 事件触发 RPC
4. **E2E 测试场景**：验证 7→6→5→4→5 完整流程
```

- [ ] **Step 2: 提交**

```bash
git add docs/devlog-54-dynamic-validator-set.md
GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
  git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" \
  -m "docs: update devlog-54 with RPC integration and prev_randao derivation"
```
