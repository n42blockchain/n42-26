# Devlog 55: Go 审计修复 + reth 升级 + Tier 1 功能实现

> Date: 2026-03-26
> Scope: HotStuff-2 审计修复、reth 依赖同步、架构审计、Tier 1 功能

---

## Phase 1: Go 审计修复（审计声明验证 + 2 项修复）

### 审计验证结论

对照 Go 版 (n42-gov5) 审计报告逐项验证 Rust 实现：

| 审计声明 | 验证 |
|---------|------|
| Validator reconfig: ⚠️ 仅 epoch manager | 部分正确 — API 完整，缺 RPC 端点 → **已修复** |
| Rotor relay: ❌ | 正确 → **本次实现** |
| 崩溃恢复: ❌ | **审计错误** — ConsensusSnapshot 持久化完整 |
| 区块随机数: ❌ | 正确 → **已修复** |
| Baby Raptr DA: ❌ | 正确 → **本次实现（字段+存储）** |

### 实现：prev_randao 从 CommitQC 派生

- `prev_randao = keccak256(commit_qc.aggregate_signature.to_bytes())`
- 缓存在 `prev_randao_cache: B256` 避免重复哈希
- 崩溃恢复时通过 `with_recovered_commit_qc()` 从 snapshot 恢复

### 实现：Validator Reconfig RPC

- `AdminCommand` 枚举 + `mpsc` channel 连接 RPC 和 orchestrator
- `n42_proposeAddValidator(admin_token, address, bls_pubkey)`
- `n42_proposeRemoveValidator(admin_token, address)`
- Bearer token 鉴权：`N42_ADMIN_TOKEN` 环境变量

---

## Phase 2: reth 依赖升级

- reth: `ae2c916f61` → `e3dbdbb115` (+102 commits)
- alloy: `1.7.3` → `1.8.2`
- alloy-evm: `0.28.0` → `0.29.2`
- revm: `34.0.0` → `36.0.0`
- `reth-primitives-traits`: local path → crates.io `v0.1.0`
- **解决了 revm-database v10/v12 版本冲突**
- 适配 API 变更：`EthPayloadBuilderAttributes` 移除、`fork_choice_updated` 签名变化、`BuildArguments::new` 新参数
- 重新生成 `reth-n42.patch` 适配新 reth

---

## Phase 3: 生产接入审计

逐模块检查 main.rs 的实际接入链路：

| 模块 | 状态 | 修复 |
|------|------|------|
| JMT | ❌ STUB — 从未构造 | → `N42_JMT=1` 环境变量启用 |
| Parallel EVM | ❌ 死代码 | 标记，待清理 |
| Admin RPC | ⚠️ 无鉴权 | → Bearer token |
| 其他 12 个模块 | ✅ 完整接入 | — |

---

## Phase 4: Tier 1 功能（Go 借鉴）

### Rotor 中继

- **算法**：`rotor.rs` — Fisher-Yates(SHA256(view)) 确定性 shuffle 选 k=3 个 relay 节点
- **Leader 集成**：Proposal 通过 Rotor 直发 relay → GossipSub 保底
- **Relay 转发**：NetworkService 收到 leader 直发后，转发给分配的 target 节点
- **防循环**：只转发来自 leader 的消息（`source == leader_peer` 检查）
- **Metrics**：`n42_rotor_direct_sends`、`n42_rotor_fallback_used`、`n42_rotor_relayed_msgs`

### Baby Raptr DA

- Proposal 增加 `tx_root_hash: Option<B256>` 字段（`#[serde(default)]` 向后兼容）
- `ConsensusEvent::BlockReady(B256, Option<B256>)` 携带 tx_root
- `pending_tx_roots: HashMap<B256, B256>` 存储待验证的 tx root
- 完整 MPT 校验暂缓（reth `new_payload` 已验证 `block_hash`）

### 随机数 EVM 预编译 (0x0302)

- **纯逻辑**：`precompile_random.rs` — `getRandom()`/`getRandomInRange(max)`/`getRandomWithSeed(seed)`
- **EVM 注册**：`N42EvmFactory` 实现 `EvmFactory` trait，`EthEvmConfig::new_with_evm_factory()` 注入
- **prevrandao 注入**：`thread_local! { BLOCK_PREVRANDAO }` + `create_evm()` 中自动设置
- Gas：100（basic）、150（range）

### Admin RPC 鉴权

- `N42_ADMIN_TOKEN` 环境变量设置 token
- 未设置时 admin RPC 返回 "not enabled"
- Token 不匹配返回 "invalid admin token"

---

## 完成状态

- [x] prev_randao 从 CommitQC 派生 + 缓存 + 崩溃恢复
- [x] Validator reconfig RPC + admin channel + 鉴权
- [x] reth 升级到 e3dbdbb11 + 依赖对齐
- [x] 架构文档更新 + 生产接入审计表
- [x] JMT 接入生产 (N42_JMT=1)
- [x] Rotor 中继算法 + 网络集成 + relay 转发
- [x] Baby Raptr DA tx_root_hash 字段
- [x] 随机数预编译 0x0302 + EVM 注册
- [ ] 编译验证（需 reth 依赖环境 / WSL2）
- [ ] 7 节点 48K cap 压测（需 Linux/WSL2）
