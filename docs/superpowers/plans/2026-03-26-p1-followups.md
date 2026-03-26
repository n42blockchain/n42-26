# P1 Follow-ups 实现计划：预编译 EVM 注册 + DA 完整校验 + Rotor 中继转发

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 完成三个 follow-up 功能，将已实现的核心逻辑完整接入生产路径。

**Architecture:**
- 预编译注册：实现自定义 `EvmFactory`，使用 `EthEvmConfig::new_with_evm_factory()` 注入 `N42EvmFactory`，在 `create_evm()` 中扩展 `Precompiles` 集合
- DA 校验：orchestrator 在 block data 到达时提取 `transactions_root`，通过新 `ConsensusEvent::TxRootVerified` 传给 engine 做比对
- Rotor 转发：NetworkService 收到 direct message 后，检查自己是否是该 view 的 relay，如果是则转发给分配的 target 节点

**Tech Stack:** revm v36 `PrecompilesMap`, alloy-evm `EvmFactory`, reth `EthEvmConfig::new_with_evm_factory`

---

## Task 1：随机数预编译 EVM 注册

**Files:**
- Create: `crates/n42-execution/src/evm_factory.rs`
- Modify: `crates/n42-execution/src/evm_config.rs`
- Modify: `crates/n42-execution/src/lib.rs`
- Modify: `crates/n42-node/src/components.rs`

**参考实现：** `D:\N42\reth-latest\examples\custom-evm\src\main.rs:44-118`

### 设计

reth 的 `EthEvmConfig` 支持 `new_with_evm_factory(chain_spec, factory)` 方法。我们创建 `N42EvmFactory` 实现 `alloy_evm::EvmFactory` trait，在 `create_evm()` 中扩展标准预编译集合。

- [ ] **Step 1: 创建 `N42EvmFactory`**

在 `crates/n42-execution/src/evm_factory.rs` 中：

```rust
use alloy_evm::precompiles::PrecompilesMap;
use alloy_primitives::{address, Address};
use revm::context::{Context, TxEnv, BlockEnv, CfgEnv};
use revm::context_interface::result::EVMError;
use revm::database_interface::Database;
use revm::handler::{EthPrecompiles, EthFrame, EthEvm};
use revm::inspector::{Inspector, NoOpInspector};
use revm::interpreter::interpreter::EthInterpreter;
use revm::precompile::{Precompile, Precompiles};
use revm::primitives::hardfork::SpecId;
use std::sync::OnceLock;

use crate::precompile_random;

/// N42 randomness precompile address: 0x0302
pub const RANDOMNESS_PRECOMPILE: Address = address!("0000000000000000000000000000000000000302");

/// Static precompiles including N42 randomness.
fn n42_precompiles(spec: SpecId) -> &'static Precompiles {
    // Use a spec-indexed OnceLock to cache per-spec precompile sets.
    // For simplicity, build dynamically (Precompiles clones are cheap).
    static PRAGUE: OnceLock<Precompiles> = OnceLock::new();
    // In practice, use the same set for all specs since randomness is always available.
    PRAGUE.get_or_init(|| {
        let mut precompiles = Precompiles::new(spec.into()).clone();
        precompiles.extend([
            Precompile::new(
                RANDOMNESS_PRECOMPILE,
                randomness_precompile_fn,
            ),
        ]);
        precompiles
    })
}

/// Adapter: revm precompile function signature wrapping our pure logic.
fn randomness_precompile_fn(
    input: &[u8],
    gas_limit: u64,
    env: &impl revm::context_interface::Cfg,
) -> revm::precompile::PrecompileResult {
    // Extract prevrandao from the block environment.
    // Note: The precompile function signature may need adjustment
    // based on the exact revm v36 PrecompileFn type.
    // This is the main integration uncertainty point.
    todo!("Bridge between revm PrecompileFn signature and precompile_random::execute_randomness")
}
```

**重要不确定性：** revm v36 的 `PrecompileFn` 签名和 `Precompile::new()` 构造器需要在实现时确认。上面的代码是伪代码 — 实际签名可能是 `Fn(&Bytes, u64) -> PrecompileResult` 或包含上下文参数。

实现时需要：
1. 检查 `revm::precompile::Precompile` 的确切构造方式
2. 检查预编译函数是否能访问 block env（获取 prevrandao）
3. 如果不能直接访问 env，需要用 `thread_local!` 或 `static` 注入 prevrandao（类似 Go 版的 `SetBlockRandomness`）

- [ ] **Step 2: 实现 EvmFactory trait**

```rust
#[derive(Debug, Clone, Default)]
pub struct N42EvmFactory;

impl alloy_evm::EvmFactory for N42EvmFactory {
    type Evm<DB: Database, I: Inspector<...>> = EthEvm<DB, I, PrecompilesMap>;
    type Context<DB: Database> = revm::context::EthEvmContext<DB>;
    type Tx = TxEnv;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: alloy_evm::EvmEnv<Self::Spec, Self::BlockEnv, Self::Tx>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let spec = input.cfg_env.spec;
        let evm = Context::mainnet()
            .with_db(db)
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .build_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(
                PrecompilesMap::from_static(n42_precompiles(spec))
            );
        EthEvm::new(evm, false)
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>, EthInterpreter>>(
        &self,
        db: DB,
        input: alloy_evm::EvmEnv<Self::Spec, Self::BlockEnv, Self::Tx>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let inner = self.create_evm(db, input).into_inner();
        EthEvm::new(inner.with_inspector(inspector), true)
    }
}
```

**注意：** 上面的类型参数是近似的。实现时需要根据 `alloy_evm::EvmFactory` trait 的确切 associated types 调整。参考 `D:\N42\reth-latest\examples\custom-evm\src\main.rs`。

- [ ] **Step 3: 修改 N42EvmConfig 使用自定义 factory**

在 `crates/n42-execution/src/evm_config.rs` 中：

将 `type InnerConfig = EthEvmConfig<ChainSpec>;` 改为：
```rust
type InnerConfig = EthEvmConfig<ChainSpec, N42EvmFactory>;
```

修改 `N42EvmConfig::new()`:
```rust
pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
    Self {
        inner: EthEvmConfig::new_with_evm_factory(chain_spec, N42EvmFactory),
    }
}
```

- [ ] **Step 4: 编译 + 测试**

Run: `cargo check -p n42-execution -p n42-node --all-targets`
Run: `cargo test -p n42-execution`

- [ ] **Step 5: 提交**

```bash
GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
  git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" \
  -m "feat(execution): register N42 randomness precompile via custom EvmFactory"
```

---

## Task 2：Baby Raptr DA 完整校验

**Files:**
- Modify: `crates/n42-consensus/src/protocol/state_machine.rs` (新 ConsensusEvent)
- Modify: `crates/n42-consensus/src/protocol/proposal.rs` (验证方法)
- Modify: `crates/n42-node/src/orchestrator/consensus_loop.rs` (提取 tx_root + 发送事件)

**设计：**
orchestrator 在处理 block data 时（`handle_execute_block` 或 `handle_block_data_arrived`），从 payload JSON 中解析 header 的 `transactions_root`。将其通过新的 `ConsensusEvent::TxRootVerified` 传给 consensus engine 做比对。

- [ ] **Step 1: 添加 ConsensusEvent::TxRootVerified**

在 `crates/n42-consensus/src/protocol/state_machine.rs` 的 `ConsensusEvent` enum 中添加：

```rust
pub enum ConsensusEvent {
    Message(ConsensusMessage),
    BlockReady(B256, Option<B256>),
    BlockImported(B256),
    /// Actual transaction root extracted from block data, for DA verification.
    TxRootVerified { block_hash: B256, actual_tx_root: B256 },
}
```

在 `process_event` match 中添加处理：
```rust
ConsensusEvent::TxRootVerified { block_hash, actual_tx_root } => {
    self.verify_tx_root(block_hash, actual_tx_root)
}
```

- [ ] **Step 2: 实现 verify_tx_root 方法**

在 `state_machine.rs` 的 `ConsensusEngine` impl 中：

```rust
fn verify_tx_root(&mut self, block_hash: B256, actual_tx_root: B256) -> ConsensusResult<()> {
    if let Some(expected) = self.pending_tx_roots.remove(&block_hash) {
        if expected != actual_tx_root {
            warn!(
                target: "n42::consensus",
                %block_hash,
                %expected,
                %actual_tx_root,
                "Baby Raptr DA: tx_root_hash mismatch"
            );
            // For now, log the mismatch. In production, this would reject the block.
            // return Err(ConsensusError::DaVerificationFailed { ... });
        }
    }
    Ok(())
}
```

初期只做告警，不拒绝区块（避免网络分叉）。待充分测试后升级为硬拒绝。

- [ ] **Step 3: 在 orchestrator 中提取 tx_root 并发送事件**

在 `consensus_loop.rs` 的 block data 处理路径中，当 block data 到达且成功解析后：

找到 `handle_execute_block` 方法（约 line 287），在 `self.pending_block_data.contains_key(&block_hash)` 检查后，解析 payload 提取 transactions_root：

```rust
// Extract tx_root from block data for DA verification
if let Some(data) = self.pending_block_data.get(&block_hash) {
    if let Some(tx_root) = Self::extract_transactions_root(block_hash, data) {
        if let Err(e) = self.engine.process_event(
            ConsensusEvent::TxRootVerified { block_hash, actual_tx_root: tx_root }
        ) {
            warn!(target: "n42::cl", %block_hash, error = %e, "tx_root verification failed");
        }
    }
}
```

需要添加 `extract_transactions_root` 辅助方法：
```rust
fn extract_transactions_root(block_hash: B256, data: &[u8]) -> Option<B256> {
    let broadcast = Self::decode_block_data_broadcast(block_hash, data, "tx_root")?;
    let payload_json = super::decompress_payload(&broadcast.payload_json)?;
    // Parse the ExecutionPayload JSON to extract header.transactions_root
    let payload: serde_json::Value = serde_json::from_slice(&payload_json).ok()?;
    let tx_root_hex = payload.get("transactionsRoot")?.as_str()?;
    let tx_root_bytes = hex::decode(tx_root_hex.strip_prefix("0x").unwrap_or(tx_root_hex)).ok()?;
    B256::try_from(tx_root_bytes.as_slice()).ok()
}
```

注意：具体 JSON 字段名取决于 `ExecutionPayload` 序列化格式。需要在实现时确认。

- [ ] **Step 4: 编译 + 测试**

Run: `cargo check -p n42-consensus -p n42-node --all-targets`
Run: `cargo test -p n42-consensus`

- [ ] **Step 5: 提交**

```bash
GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
  git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" \
  -m "feat(consensus): Baby Raptr DA full tx_root verification in orchestrator"
```

---

## Task 3：Rotor 中继转发

**Files:**
- Modify: `crates/n42-network/src/service.rs` (relay forwarding in handle_consensus_direct_event)
- Modify: `crates/n42-network/src/service.rs` (NetworkService 新增 validator context)

**设计：**
当 relay 节点收到 leader 的直发 Proposal 时，检查自己是否是该 view 的 relay 节点。如果是，计算自己的 target 列表并转发。

关键挑战：NetworkService 需要知道自己的 validator index 和当前 validator 数量。

- [ ] **Step 1: 在 NetworkService 中添加 validator context**

在 `crates/n42-network/src/service.rs` 的 `NetworkService` struct 中添加：

```rust
/// Local validator index (None if not a validator)
my_validator_index: Option<u32>,
/// Current validator count (needed for relay computation)
validator_count: u32,
```

在构造函数中初始化为 `None` / `0`。

添加 `NetworkCommand` 变体和 `NetworkHandle` 方法来设置：
```rust
// NetworkCommand
SetValidatorContext { my_index: u32, validator_count: u32 },

// NetworkHandle method
pub fn set_validator_context(&self, my_index: u32, validator_count: u32) {
    let _ = self.command_tx.try_send(NetworkCommand::SetValidatorContext {
        my_index,
        validator_count,
    });
}
```

orchestrator 在启动时和 epoch transition 时调用此方法。

- [ ] **Step 2: 在 handle_consensus_direct_event 中添加 relay 转发**

当收到直发的 `ConsensusMessage::Proposal` 时：

```rust
fn handle_consensus_direct_event(&mut self, event: ...) {
    // ... existing decode logic ...
    let message = decode_consensus_message(&request.message_bytes)?;

    // Check if we should relay this proposal
    if let ConsensusMessage::Proposal(ref proposal) = message {
        self.maybe_relay_proposal(proposal, &request.message_bytes);
    }

    // Emit event as before
    self.emit_event(NetworkEvent::ConsensusMessage { source: peer, message: Box::new(message) });
}
```

实现 `maybe_relay_proposal`：
```rust
fn maybe_relay_proposal(&mut self, proposal: &Proposal, raw_bytes: &[u8]) {
    let Some(my_index) = self.my_validator_index else { return };
    if self.validator_count < 2 { return; }

    use n42_consensus::rotor::compute_relay_assignment;
    let assignment = compute_relay_assignment(
        proposal.view,
        self.validator_count,
        proposal.proposer,
        3,
    );

    // Find if I'm a relay for this view
    let my_targets = assignment.relays.iter()
        .find(|(relay, _)| *relay == my_index)
        .map(|(_, targets)| targets.clone());

    let Some(targets) = my_targets else { return };

    // Forward to my assigned targets
    let map = self.validator_peer_map.read().unwrap_or_else(|e| e.into_inner());
    for target_idx in &targets {
        if *target_idx == my_index { continue; } // Don't send to self
        if let Some(&peer_id) = map.get(target_idx) {
            let req = ConsensusDirectRequest { message_bytes: raw_bytes.to_vec() };
            self.swarm.behaviour_mut().consensus_direct.send_request(&peer_id, req);
            metrics::counter!("n42_rotor_relayed_msgs").increment(1);
        }
    }
}
```

- [ ] **Step 3: orchestrator 调用 set_validator_context**

在 `crates/n42-node/src/orchestrator/mod.rs` 的 `run()` 方法开始时：

```rust
// Set validator context for Rotor relay forwarding
self.network.set_validator_context(
    self.engine.my_index(),
    self.engine.validator_set().len(),
);
```

在 `EpochTransition` handler 中也更新：
```rust
EngineOutput::EpochTransition { new_epoch, validator_count } => {
    // ... existing code ...
    self.network.set_validator_context(
        self.engine.my_index(),
        validator_count,
    );
}
```

- [ ] **Step 4: 防止无限转发**

relay 收到的消息可能来自另一个 relay（如果 GossipSub 也在传播）。需要确保不会无限转发。

简单方案：只在收到 **direct** 消息时做 relay，不在收到 **GossipSub** 消息时做 relay。当前 `handle_consensus_direct_event` 只处理 direct 消息，所以天然不会循环。

但如果 relay A 的 target 列表包含 relay B，B 收到后也会尝试转发。解决：relay 不转发已经是 relay 角色的消息（可以在 raw_bytes 中加标记，或者只允许 leader 发出的消息被 relay）。

最简方案：检查消息来源。如果 source peer 是 leader，则 relay；如果 source 是另一个 relay，不 relay。

```rust
fn maybe_relay_proposal(&mut self, proposal: &Proposal, raw_bytes: &[u8], source: PeerId) {
    // Only relay if the sender is the leader
    let leader_peer = {
        let map = self.validator_peer_map.read().unwrap_or_else(|e| e.into_inner());
        map.get(&proposal.proposer).copied()
    };
    if leader_peer != Some(source) { return; } // Not from leader, don't relay
    // ... rest of relay logic
}
```

- [ ] **Step 5: 编译 + 测试**

Run: `cargo check -p n42-network -p n42-node --all-targets`

- [ ] **Step 6: 提交**

```bash
GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
  git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" \
  -m "feat(network): Rotor relay forwarding — relay nodes forward proposals to targets"
```

---

## 依赖关系

```
Task 1 (预编译 EVM 注册) → 独立
Task 2 (DA 完整校验)      → 独立
Task 3 (Rotor 转发)       → 独立
```

三个 Task 完全独立，可并行执行。

## 风险评估

| Task | 风险 | 缓解 |
|------|------|------|
| Task 1 | **高** — revm v36 的 `EvmFactory` trait 关联类型复杂，预编译函数签名不确定 | 参考 `reth-latest/examples/custom-evm` 实现；如果 prevrandao 不可直接访问，用 thread_local 注入 |
| Task 2 | **低** — 路径清晰，只是 JSON 解析 + 事件传递 | ExecutionPayload JSON 字段名需确认 |
| Task 3 | **中** — NetworkService 需要 validator context，跨模块状态同步 | 用 command channel 异步设置，不阻塞启动 |
