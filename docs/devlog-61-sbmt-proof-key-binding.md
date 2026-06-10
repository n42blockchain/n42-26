# devlog-61 — SBMT 证明 key 绑定安全修复 + shard_index/key 派生去重

## 背景

在做 #10（清理去重）时审查 `n42-bmt-core` / `n42-mobile` / `n42-mobile-ffi`
的证明验证路径，发现一个**轻客户端 soundness 安全缺口**（记为审计 #11），
其严重性高于 #10 的机械去重，遂优先修复，并把 #10 的 shard_index 去重一并完成。

## 设计决策

### 审计 #11：SBMT 证明未绑定到被查询的 key（安全缺口）

**问题**：`ShardedBmtProof::verify(combined_root)` 以及上层
`verify_state_proof(proof, state_root)` / FFI `n42_verify_state_proof(...)`
都**只验证证明对 `combined_root` 自洽**，不接收"客户端实际查询的 key/address"，
也不校验证明里的叶子 key 与 shard 归属。后果：

1. **答非所问**：客户端问账户 A 的状态，恶意/有 bug 的服务器返回**另一个账户 B
   的合法证明**，`verify` 返回 valid，客户端误以为是 A 的数据。
2. **错 shard 伪造排除证明**：对 A 在它**真实 shard 之外**的某个 shard 上构造排除
   证明（A 在那个 shard 确实不存在），`verify` 通过，客户端被骗"A 不存在"——尽管
   A 在其正确 shard（`key[0]>>4`）里真实存在。

这直接打穿了 SBMT 轻客户端验证的全部意义（不信任服务器、只信任共识给出的 state
root）。如果信任服务器，根本不需要证明。

**修复**：在验证时绑定两件事：
- 证明叶子 key `inner.key == 期望 key`；
- `shard_index == shard_index_for_key(期望 key)`（堵住错 shard 排除证明）。

为什么两者都要：只绑定 key 不够——攻击者仍可对真实 key K 在错误 shard 上构造排除
证明（K 在该 shard 真实缺失），因此必须同时绑定 shard。

**API 取舍**：
- 保留旧的 `verify` / `verify_state_proof` / `n42_verify_state_proof`（不破坏既有
  E2E），但在文档里明确标注"仅验自洽、不绑定查询，untrusted 服务器场景禁用"。
- 新增**防误用**的绑定接口，让调用方传 address（而非自行算 key，避免派生不一致）：
  - `n42_bmt_core::ShardedBmtProof::verify_for_key(combined_root, expected_key)`
  - `n42_mobile::state_proof::verify_account_proof(proof, root, address)`
  - `n42_mobile::state_proof::verify_storage_proof(proof, root, address, slot)`
  - FFI `n42_verify_account_proof(proof, len, state_root, address_20)`
  - FFI `n42_verify_storage_proof(proof, len, state_root, address_20, slot_32)`

### 审计 #10：key 派生 + shard_index 单一来源化

绑定校验的 soundness 前提是"验证侧派生的 key 与构建侧字节一致"。借此把派生逻辑
收敛到 `n42-bmt-core`（zero-dep，mobile/FFI 可用）作为唯一来源：

- `n42_bmt_core::account_key(&[u8;20])` / `storage_key(&[u8;20], &[u8;32])`：与原
  `n42-jmt::keys` 的 domain-separated blake3 编码字节一致。
- `n42-jmt::keys::account_key/storage_key` 改为**委托** `n42-bmt-core`（保留
  `KeyHash` 包装），消除重复的 domain+blake3 代码。
- `n42_bmt_core::shard_index_for_key(&Hash)`：统一 `key[0]>>4` 的 4-bit/16-way
  分片逻辑（`debug_assert` 记录与 `SHARD_COUNT==16` 的耦合）。
- n42-jmt 5 处 `>>4`（`sharded_bmt.rs`、`sharded.rs`、`snapshot.rs`、`proof.rs`，
  以及 proof 测试里保留一处独立 `>>4` 作为交叉校验 oracle）统一改用 canonical。

## 实施细节

- `crates/n42-bmt-core/src/lib.rs`：
  - 新增 `account_key` / `storage_key` / `shard_index_for_key`；
  - `BmtVerifyError` 新增 `KeyMismatch`、`WrongShardForKey { expected, got }`；
  - `ShardedBmtProof::verify_for_key(combined_root, expected_key)`。
- `crates/n42-jmt/src/keys.rs`：`account_key/storage_key` 委托 bmt-core；新增
  `jmt_and_bmt_core_keys_agree` 测试锁定字节一致不变量。
- `crates/n42-jmt/src/{sharded_bmt,sharded,snapshot,proof}.rs`：shard_index 统一。
- `crates/n42-mobile/src/state_proof.rs`：新增 `verify_account_proof` /
  `verify_storage_proof` + 绑定测试（key 不符 → KeyMismatch、错 shard →
  WrongShardForKey）。
- `crates/n42-mobile-ffi/src/lib.rs`：新增两个绑定 FFI + 测试
  `test_verify_account_proof_binds_address`（正确 address 通过、错 address 返回 2、
  并演示旧的 unbound API 仍会接受——说明为何要绑定）。

## 遇到的问题及解决方案

- `n42-bmt-core` 必须 zero-dep：key 派生只用 blake3（已有依赖）+ 原始字节
  `&[u8;20]`/`&[u8;32]`，不引入 alloy，保持 mobile 轻量。
- 派生一致性是 soundness 命门：用 `jmt_and_bmt_core_keys_agree` 显式断言两侧
  字节相等，避免未来某侧改了 domain/编码而静默分叉。

## 阶段完成状态

- [x] #11 证明 key+shard 绑定（core/mobile/FFI 三层）
- [x] #10 key 派生 + shard_index 单一来源化
- [x] 单测：bmt-core 6 / jmt 85 / mobile 104 / mobile-ffi 45 全绿；clippy `-D warnings` 干净
- [ ] codex 在 mac 侧用 node RPC 真实证明做对抗性复核（正确 vs 错误 address；错 shard）

## 后续计划

- codex 拉取后独立复核 #11，并补一条 mac E2E：从 `n42_jmtProof` 取真实证明，
  `n42_verify_account_proof` 对正确 address 通过、对错误 address/错 shard 拒绝。
- 剩余：#5（leader drain 最终化，codex 进行中）。
