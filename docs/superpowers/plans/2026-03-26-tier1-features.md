# Tier 1 功能实现计划：Rotor 中继 + 随机数预编译 + Baby Raptr DA + Admin RPC 鉴权

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现 Go 审计中识别的 3 项共识关键功能 + 修复 admin RPC 无鉴权安全漏洞。

**Architecture:**
- Rotor：确定性 Fisher-Yates shuffle 选中继节点，利用已有的 `consensus_direct` request-response 协议直接发送，失败回退 GossipSub
- 随机数预编译：在 `N42EvmConfig` 中注册自定义预编译合约（地址 `0x0302`），从 `prev_randao` 读取
- Baby Raptr DA：`Proposal` 增加 `tx_root_hash` 字段，follower import 后比对
- Admin 鉴权：`N42_ADMIN_TOKEN` 环境变量，RPC 调用时验证 bearer token

**Tech Stack:** Rust, libp2p request-response, alloy-evm precompiles, sha2, HMAC

---

## Task 1：Rotor 中继 — 选举与分配算法

**Files:**
- Create: `crates/n42-consensus/src/rotor.rs`
- Modify: `crates/n42-consensus/src/lib.rs` (pub mod rotor)

**设计：**
Go 版的核心算法（`rotor.go:116-176`）：用 SHA256(view) 做 Fisher-Yates shuffle，选前 k 个节点做中继，其余 round-robin 分配到中继。

- [ ] **Step 1: 编写 rotor.rs 核心算法**

```rust
// crates/n42-consensus/src/rotor.rs
use alloy_primitives::B256;
use sha2::{Sha256, Digest};

/// Number of relay nodes per view.
const DEFAULT_RELAY_COUNT: usize = 3;

/// Relay assignment: which relay forwards to which validators.
#[derive(Debug, Clone)]
pub struct RelayAssignment {
    /// (relay_validator_index, target_validator_indices)
    pub relays: Vec<(u32, Vec<u32>)>,
}

/// Compute deterministic relay assignment for a given view.
///
/// Algorithm: Fisher-Yates shuffle seeded by SHA256(view_le_bytes).
/// First `relay_count` shuffled validators become relays.
/// Remaining validators are round-robin assigned to relays.
pub fn compute_relay_assignment(
    view: u64,
    validator_count: u32,
    leader: u32,
    relay_count: usize,
) -> RelayAssignment {
    if validator_count <= 1 || relay_count == 0 {
        return RelayAssignment { relays: vec![] };
    }

    let relay_count = relay_count.min((validator_count - 1) as usize);

    // Build candidate list: all validators except leader
    let mut candidates: Vec<u32> = (0..validator_count).filter(|&v| v != leader).collect();

    // Deterministic Fisher-Yates shuffle
    let seed = Sha256::digest(view.to_le_bytes());
    let n = candidates.len();
    for i in (1..n).rev() {
        let pos_input = [seed.as_slice(), &(i as u64).to_le_bytes()].concat();
        let pos_hash = Sha256::digest(&pos_input);
        let j = u64::from_le_bytes(pos_hash[0..8].try_into().unwrap()) as usize % (i + 1);
        candidates.swap(i, j);
    }

    // First relay_count candidates are relays
    let relay_indices: Vec<u32> = candidates[..relay_count].to_vec();
    let remaining: Vec<u32> = candidates[relay_count..].to_vec();

    // Round-robin assign remaining validators to relays
    let mut relays: Vec<(u32, Vec<u32>)> = relay_indices
        .iter()
        .map(|&r| (r, vec![r])) // Each relay also delivers to itself
        .collect();

    for (i, &validator) in remaining.iter().enumerate() {
        relays[i % relay_count].1.push(validator);
    }

    RelayAssignment { relays }
}
```

- [ ] **Step 2: 编写单元测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_relay_assignment_deterministic() {
        let a1 = compute_relay_assignment(42, 7, 0, 3);
        let a2 = compute_relay_assignment(42, 7, 0, 3);
        assert_eq!(a1.relays.len(), a2.relays.len());
        for (r1, r2) in a1.relays.iter().zip(a2.relays.iter()) {
            assert_eq!(r1.0, r2.0);
            assert_eq!(r1.1, r2.1);
        }
    }

    #[test]
    fn test_relay_covers_all_validators() {
        let assignment = compute_relay_assignment(1, 7, 0, 3);
        let mut covered: Vec<u32> = assignment.relays.iter()
            .flat_map(|(_, targets)| targets.iter().copied())
            .collect();
        covered.sort();
        covered.dedup();
        // All non-leader validators covered
        assert_eq!(covered.len(), 6);
        assert!(!covered.contains(&0)); // leader excluded
    }

    #[test]
    fn test_single_node_empty() {
        let assignment = compute_relay_assignment(1, 1, 0, 3);
        assert!(assignment.relays.is_empty());
    }

    #[test]
    fn test_different_views_different_relays() {
        let a1 = compute_relay_assignment(1, 21, 0, 3);
        let a2 = compute_relay_assignment(2, 21, 0, 3);
        // Very unlikely to produce identical relay selection
        let r1: Vec<u32> = a1.relays.iter().map(|(r, _)| *r).collect();
        let r2: Vec<u32> = a2.relays.iter().map(|(r, _)| *r).collect();
        assert_ne!(r1, r2);
    }
}
```

- [ ] **Step 3: 验证编译和测试**

Run: `cargo test -p n42-consensus rotor`
Expected: 4 tests PASS

- [ ] **Step 4: 提交**

```bash
GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
  git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" \
  -m "feat(consensus): add Rotor relay selection algorithm"
```

---

## Task 2：Rotor 中继 — 网络层集成

**Files:**
- Modify: `crates/n42-node/src/orchestrator/consensus_loop.rs` (BroadcastMessage handler)
- Modify: `crates/n42-node/src/orchestrator/mod.rs` (add relay_count field)

**设计：**
替换 leader 的 `BroadcastMessage` 处理：当自己是 leader 时，先尝试 Rotor 直发，失败回退 GossipSub。非 leader 收到中继请求后转发给自己的 target 列表。

- [ ] **Step 1: 在 orchestrator 中添加 Rotor broadcast 方法**

在 `consensus_loop.rs` 的 `handle_engine_output` 中，替换 `BroadcastMessage` 分支：

```rust
EngineOutput::BroadcastMessage(msg) => {
    // Use Rotor relay when we're the leader broadcasting a Proposal
    if matches!(&msg, ConsensusMessage::Proposal(_)) && self.engine.is_current_leader() {
        self.broadcast_via_rotor(msg).await;
    } else {
        if let Err(e) = self.network.broadcast_consensus_reliable(msg).await {
            error!(target: "n42::cl::consensus_loop", error = %e, "failed to broadcast");
        }
    }
}
```

在 `mod.rs` 中添加 `broadcast_via_rotor` 方法：

```rust
async fn broadcast_via_rotor(&mut self, msg: ConsensusMessage) {
    use n42_consensus::rotor::compute_relay_assignment;

    let view = self.engine.current_view();
    let validator_count = self.engine.validator_set().len();
    let leader = self.engine.current_leader_index();
    let assignment = compute_relay_assignment(view, validator_count, leader, 3);

    let mut all_sent = true;
    for (relay_idx, _targets) in &assignment.relays {
        if let Some(peer_id) = self.network.validator_peer(*relay_idx) {
            if let Err(e) = self.network.send_direct_reliable(peer_id, msg.clone()).await {
                warn!(target: "n42::cl::rotor", relay = relay_idx, error = %e, "relay direct send failed");
                all_sent = false;
            }
        } else {
            all_sent = false;
        }
    }

    // Always broadcast as safety net (matching Go behavior)
    if let Err(e) = self.network.broadcast_consensus_reliable(msg).await {
        error!(target: "n42::cl::rotor", error = %e, "gossipsub fallback failed");
    }

    if all_sent {
        metrics::counter!("n42_rotor_direct_sends").increment(1);
    } else {
        metrics::counter!("n42_rotor_fallback_used").increment(1);
    }
}
```

- [ ] **Step 2: 编译检查**

Run: `cargo check -p n42-node`

- [ ] **Step 3: 提交**

```bash
GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
  git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" \
  -m "feat(node): integrate Rotor relay into leader proposal broadcast"
```

---

## Task 3：Baby Raptr DA — Proposal 增加 tx_root_hash

**Files:**
- Modify: `crates/n42-primitives/src/consensus/messages.rs` (Proposal struct)
- Modify: `crates/n42-consensus/src/protocol/proposal.rs` (creation + validation)

**设计：**
在 Proposal 中加 `tx_root_hash: Option<B256>`（optional 保持向后兼容）。Leader 创建 proposal 时包含 block 的 tx root。Follower 在 `BlockImported` 时校验。

- [ ] **Step 1: 修改 Proposal struct**

在 `crates/n42-primitives/src/consensus/messages.rs` 的 `Proposal` struct 中添加：

```rust
pub struct Proposal {
    pub view: ViewNumber,
    pub block_hash: B256,
    pub justify_qc: QuorumCertificate,
    pub proposer: ValidatorIndex,
    pub signature: BlsSignature,
    pub prepare_qc: Option<QuorumCertificate>,
    /// Transaction root hash for Baby Raptr DA verification.
    /// Followers compare this against the actual tx root after block import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_root_hash: Option<B256>,
}
```

`#[serde(default)]` 确保旧序列化格式向后兼容（缺失字段反序列化为 `None`）。

- [ ] **Step 2: 修改 proposal 创建（on_block_ready）**

在 `crates/n42-consensus/src/protocol/proposal.rs` 的 `on_block_ready` 中：

当前签名是 `fn on_block_ready(&mut self, block_hash: B256)`。需要新增参数或通过其他方式传入 tx_root_hash。

方案：扩展 `ConsensusEvent::BlockReady` 携带 tx_root_hash：

修改 `crates/n42-consensus/src/protocol/state_machine.rs` 中的 `ConsensusEvent`：
```rust
pub enum ConsensusEvent {
    BlockReady(B256, Option<B256>),  // (block_hash, tx_root_hash)
    // ... other variants
}
```

然后在 `on_block_ready` 中：
```rust
pub(super) fn on_block_ready(&mut self, block_hash: B256, tx_root_hash: Option<B256>) -> ConsensusResult<()> {
    // ... existing code ...
    let proposal = Proposal {
        view,
        block_hash,
        justify_qc,
        proposer: self.my_index,
        signature,
        prepare_qc: piggybacked_qc,
        tx_root_hash,
    };
    // ...
}
```

- [ ] **Step 3: 修改 proposal 验证（process_proposal）**

在 `process_proposal` 中，proposal 接收时不需要立即验证 tx_root_hash（区块数据还没到）。只需存储 pending_tx_roots 供后续 BlockImported 时校验。

在 `ConsensusEngine` 中添加字段：
```rust
pending_tx_roots: HashMap<B256, B256>,  // block_hash -> expected tx_root_hash
```

在 `process_proposal` 末尾：
```rust
if let Some(tx_root) = proposal.tx_root_hash {
    self.pending_tx_roots.insert(proposal.block_hash, tx_root);
}
```

在 `process_event(ConsensusEvent::BlockImported(hash))` 中：
```rust
if let Some(expected) = self.pending_tx_roots.remove(&hash) {
    // Caller should supply actual_tx_root; for now log a check
    // Full verification requires passing actual tx root from execution layer
}
```

注意：完整验证需要 orchestrator 在 `handle_execute_block` 后将实际 tx_root 传回 consensus engine。这增加了 orchestrator 和 engine 之间的接口。本任务先添加字段和存储，完整校验作为 follow-up。

- [ ] **Step 4: 编译 + 测试**

Run: `cargo check -p n42-consensus -p n42-primitives --all-targets`
Run: `cargo test -p n42-consensus`

- [ ] **Step 5: 提交**

```bash
GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
  git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" \
  -m "feat(consensus): add tx_root_hash to Proposal for Baby Raptr DA verification"
```

---

## Task 4：随机数 EVM 预编译（0x0302）

**Files:**
- Create: `crates/n42-execution/src/precompile_random.rs`
- Modify: `crates/n42-execution/src/evm_config.rs` (注册预编译)
- Modify: `crates/n42-execution/src/lib.rs` (pub mod)

**设计：**
预编译地址 `0x0302`，3 个函数通过输入首字节区分：
- `0x00`: `getRandom()` → `keccak256(prevrandao)` (100 gas)
- `0x01`: `getRandomInRange(max: u256)` → `keccak256(prevrandao) % max` (150 gas)
- `0x02`: `getRandomWithSeed(seed: bytes32)` → `keccak256(prevrandao || seed)` (100 gas)

`prevrandao` 已从 CommitQC 聚合签名派生并写入 block header。

- [ ] **Step 1: 创建 precompile_random.rs**

```rust
// crates/n42-execution/src/precompile_random.rs
use alloy_primitives::{Address, Bytes, B256, U256, address, keccak256};
use revm::precompile::{PrecompileOutput, PrecompileResult, PrecompileWithAddress};
use revm::primitives::Env;

/// N42 randomness precompile address.
pub const RANDOMNESS_ADDRESS: Address = address!("0000000000000000000000000000000000000302");

/// Gas costs.
const GAS_RANDOM: u64 = 100;
const GAS_RANDOM_RANGE: u64 = 150;

/// N42 randomness precompile: derives on-chain randomness from block's prevrandao
/// (which is keccak256 of CommitQC aggregate BLS signature — threshold VUF).
pub fn n42_randomness_precompile(input: &Bytes, gas_limit: u64, env: &Env) -> PrecompileResult {
    let selector = input.first().copied().unwrap_or(0x00);
    let prevrandao = env.block.prevrandao.unwrap_or(B256::ZERO);

    match selector {
        // getRandom() → keccak256(prevrandao)
        0x00 => {
            if gas_limit < GAS_RANDOM {
                return Err(revm::precompile::PrecompileErrors::Error(
                    revm::precompile::PrecompileError::OutOfGas.into(),
                ));
            }
            let hash = keccak256(prevrandao);
            Ok(PrecompileOutput::new(GAS_RANDOM, hash.to_vec().into()))
        }
        // getRandomInRange(max: u256) → keccak256(prevrandao) % max
        0x01 => {
            if gas_limit < GAS_RANDOM_RANGE {
                return Err(revm::precompile::PrecompileErrors::Error(
                    revm::precompile::PrecompileError::OutOfGas.into(),
                ));
            }
            if input.len() < 33 {
                return Err(revm::precompile::PrecompileErrors::Error(
                    revm::precompile::PrecompileError::other("input too short: need 1 + 32 bytes").into(),
                ));
            }
            let max = U256::from_be_slice(&input[1..33]);
            if max.is_zero() {
                return Err(revm::precompile::PrecompileErrors::Error(
                    revm::precompile::PrecompileError::other("max cannot be zero").into(),
                ));
            }
            let hash = U256::from_be_bytes(keccak256(prevrandao).0);
            let result = hash % max;
            let mut output = [0u8; 32];
            result.to_big_endian(&mut output);
            Ok(PrecompileOutput::new(GAS_RANDOM_RANGE, output.to_vec().into()))
        }
        // getRandomWithSeed(seed: bytes32) → keccak256(prevrandao || seed)
        0x02 => {
            if gas_limit < GAS_RANDOM {
                return Err(revm::precompile::PrecompileErrors::Error(
                    revm::precompile::PrecompileError::OutOfGas.into(),
                ));
            }
            if input.len() < 33 {
                return Err(revm::precompile::PrecompileErrors::Error(
                    revm::precompile::PrecompileError::other("input too short: need 1 + 32 bytes").into(),
                ));
            }
            let mut data = Vec::with_capacity(64);
            data.extend_from_slice(prevrandao.as_ref());
            data.extend_from_slice(&input[1..33]);
            let hash = keccak256(&data);
            Ok(PrecompileOutput::new(GAS_RANDOM, hash.to_vec().into()))
        }
        _ => Err(revm::precompile::PrecompileErrors::Error(
            revm::precompile::PrecompileError::other("unknown selector").into(),
        )),
    }
}
```

注意：上面的代码使用了 revm 预编译接口。具体的 trait 签名和类型需要根据 revm v36 的 API 调整。需要读取 `revm::precompile` 模块确认准确的类型。

- [ ] **Step 2: 在 N42EvmConfig 中注册预编译**

修改 `crates/n42-execution/src/evm_config.rs`。当前 `N42EvmConfig` 直接委托给 `EthEvmConfig`。需要 override `ConfigureEvm` 的预编译注册方法。

具体实现取决于 reth 最新版的 `ConfigureEvm` trait 接口 — 需要检查是否有 `additional_precompiles()` 或类似 hook。如果没有，可能需要包装 `Evm` 实例在创建后注入预编译。

这是本任务的主要不确定性点。需要在实现时确认 revm/alloy-evm 的预编译注册机制。

- [ ] **Step 3: 编写预编译测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_random_basic() {
        let mut env = Env::default();
        env.block.prevrandao = Some(B256::repeat_byte(0x42));
        let input = Bytes::from(vec![0x00]);
        let result = n42_randomness_precompile(&input, 200, &env).unwrap();
        assert_eq!(result.output.len(), 32);
        assert_eq!(result.gas_used, 100);
    }

    #[test]
    fn test_get_random_deterministic() {
        let mut env = Env::default();
        env.block.prevrandao = Some(B256::repeat_byte(0x42));
        let input = Bytes::from(vec![0x00]);
        let r1 = n42_randomness_precompile(&input, 200, &env).unwrap();
        let r2 = n42_randomness_precompile(&input, 200, &env).unwrap();
        assert_eq!(r1.output, r2.output);
    }

    #[test]
    fn test_get_random_in_range() {
        let mut env = Env::default();
        env.block.prevrandao = Some(B256::repeat_byte(0x42));
        let mut input = vec![0x01];
        input.extend_from_slice(&U256::from(100).to_be_bytes::<32>());
        let result = n42_randomness_precompile(&Bytes::from(input), 200, &env).unwrap();
        let value = U256::from_be_slice(&result.output);
        assert!(value < U256::from(100));
    }

    #[test]
    fn test_out_of_gas() {
        let env = Env::default();
        let input = Bytes::from(vec![0x00]);
        let result = n42_randomness_precompile(&input, 50, &env);
        assert!(result.is_err());
    }
}
```

- [ ] **Step 4: 编译 + 测试**

Run: `cargo test -p n42-execution precompile_random`

- [ ] **Step 5: 提交**

```bash
GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
  git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" \
  -m "feat(execution): add N42 randomness precompile at 0x0302"
```

---

## Task 5：Admin RPC 鉴权 — Bearer Token

**Files:**
- Modify: `crates/n42-node/src/rpc.rs` (添加 token 验证)
- Modify: `crates/n42-node/src/consensus_state.rs` (存储 admin token)
- Modify: `bin/n42-node/src/main.rs` (从环境变量读取 token)

**设计：**
最简方案：`N42_ADMIN_TOKEN` 环境变量设置 admin token。RPC 调用 `proposeAddValidator`/`proposeRemoveValidator` 时必须在参数中提供 `admin_token` 字段。Token 为空字符串时禁用 admin RPC（默认）。

不使用 HTTP header（jsonrpsee 不方便访问 HTTP headers），而是在 RPC 方法参数中加 `admin_token: String`。

- [ ] **Step 1: 在 N42RpcServer 中添加 admin_token 字段**

```rust
pub struct N42RpcServer {
    consensus_state: Arc<SharedConsensusState>,
    staking_manager: Option<Arc<Mutex<StakingManager>>>,
    jmt: Option<Arc<Mutex<ShardedJmt>>>,
    zk_scheduler: Option<Arc<ProofScheduler>>,
    admin_token: Option<String>,
}
```

在 `new()` 中初始化为 `None`。添加 builder：
```rust
pub fn with_admin_token(mut self, token: String) -> Self {
    self.admin_token = Some(token);
    self
}
```

添加验证方法：
```rust
fn verify_admin_token(&self, provided: &str) -> RpcResult<()> {
    match &self.admin_token {
        Some(expected) if expected == provided => Ok(()),
        Some(_) => Err(ErrorObjectOwned::owned(-32001, "invalid admin token", None::<()>)),
        None => Err(ErrorObjectOwned::owned(-32001, "admin RPC not enabled (N42_ADMIN_TOKEN not set)", None::<()>)),
    }
}
```

- [ ] **Step 2: 修改 RPC trait 方法签名**

在 `N42Api` trait 中：
```rust
#[method(name = "proposeAddValidator")]
async fn propose_add_validator(
    &self,
    admin_token: String,
    address: Address,
    bls_pubkey: String,
) -> RpcResult<String>;

#[method(name = "proposeRemoveValidator")]
async fn propose_remove_validator(
    &self,
    admin_token: String,
    address: Address,
) -> RpcResult<String>;
```

- [ ] **Step 3: 在方法实现中添加验证**

在每个 admin 方法开头：
```rust
self.verify_admin_token(&admin_token)?;
```

- [ ] **Step 4: 在 main.rs 中读取 token 并传给 RPC server**

```rust
let admin_token = std::env::var("N42_ADMIN_TOKEN").ok();
if admin_token.is_some() {
    info!(target: "n42::cli", "Admin RPC enabled (N42_ADMIN_TOKEN set)");
}
// ... 在 extend_rpc_modules 中：
if let Some(token) = admin_token.clone() {
    rpc_server = rpc_server.with_admin_token(token);
}
```

- [ ] **Step 5: 编译 + 测试**

Run: `cargo check -p n42-node --all-targets`

- [ ] **Step 6: 提交**

```bash
GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
  git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" \
  -m "feat(rpc): add admin token authentication for validator reconfig endpoints"
```

---

## 依赖关系

```
Task 1 (Rotor 算法)      → Task 2 (Rotor 网络集成) 依赖 Task 1
Task 3 (Baby Raptr DA)    独立
Task 4 (随机数预编译)      独立（依赖已完成的 prev_randao 派生）
Task 5 (Admin RPC 鉴权)   独立
```

Task 1、3、4、5 可并行执行。Task 2 依赖 Task 1。

## 不确定性点

1. **随机数预编译注册**：需要确认 revm v36 / alloy-evm v0.29 的预编译注册 API。`ConfigureEvm` trait 可能没有直接的 `add_precompile` 方法，需要在 `Evm` 实例创建时注入。
2. **Baby Raptr DA 完整校验**：本计划只添加字段和存储。完整的 import-time 校验需要 orchestrator 传实际 tx root 给 consensus engine，这是跨模块接口变更，建议作为 follow-up。
3. **Rotor relay 消息**：当前 `consensus_direct` 协议传输 `ConsensusMessage`。Rotor 中继需要 relay 节点识别"这是中继请求"并转发。可以复用 `ConsensusMessage` 但需要在网络层添加"relay forward"逻辑。本计划的 Task 2 做了简化版（leader 直发 relay，relay 不转发，GossipSub 保底），完整版 relay 转发是 follow-up。
