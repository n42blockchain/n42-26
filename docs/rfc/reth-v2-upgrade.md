# RFC — reth v1.11.3 → v2.2.0 升级手册

> Status: Plan · Date: 2026-05-13 · Owner: TBD · Stage 2 of `优化主执行路径` work

## Context

阶段 1（同 PR 内）已经把 workspace alloy-evm 对齐到本地 reth 实际使用的 0.29.2 / revm 36.0.0，base 编译恢复（731/731 unit tests pass）。本 RFC 规划阶段 2：把本地 reth path 升到上游 v2.2.0，同步 bump alloy-evm 0.34 / revm 38，重写 N42EvmFactory 适配新 trait。

这是 5-15 天量级独立工程，需要专人专注推进。

## Scope

**In scope**:
- `D:\N42\reth` path 从当前 commit `27781443` rebase 到 `v2.2.0` 或更新
- 705 行 `reth-n42.patch` 在新 base 上 rebase
- 9 个 reth 本地 modified files (`payload_validator.rs` / `ethereum/payload/lib.rs` / `evm/evm/src/{execute,lib,Cargo}` / `fs-util/lib` / `rpc-eth-types/utils.rs` 等) 重新应用
- workspace Cargo.toml: alloy-evm → 0.34, revm → 38（确切版本以 v2.2.0 Cargo.toml 为准）
- `crates/n42-execution/src/evm_factory.rs` 重写 EvmFactory trait 实现
- `crates/n42-execution/src/evm_config.rs` 重适配 ConfigureEvm trait
- 全 workspace 编译 / clippy / 单元测试通过
- 7-node testnet 30 min 压测无回归

**Out of scope**:
- reth v2.2.0 → 上游 main（额外 460+ commit）— 留下个迭代
- 大型功能新增（仅迁移现有功能）

## Version Matrix

| 组件 | 阶段 1 (current) | 阶段 2 目标 |
|---|---|---|
| reth (path) | 1.11.3 + 705 patch + 9 local mods | v2.2.0 + rebased patch |
| alloy-evm | 0.29.2 | 0.34.0 |
| revm | 36.0.0 | 38.0.0 |
| revm-primitives | 22.1.0 | 23.0.0 |
| revm-interpreter | 34.0.0 | 35.0.0 |
| revm-bytecode | 9.0.0 | 10.0.0 |
| revm-database | 12.0.0 | 13.0.0 |
| revm-state | 10.0.0 | 11.0.0 |
| revm-database-interface | 10.0.0 | 11.0.0 |
| revm-inspectors | 0.36.0 | 0.39.0 |

## Known Breaking Changes (重点)

### reth ConfigureEvm trait

`crates/evm/evm/src/lib.rs` 接口变化（从 v1.11.3 → v2.2.0 diff 抽取）：

- `NextBlockEnvAttributes` 新增 `slot_number: Option<u64>` 字段 — N42 用 HotStuff-2 view 而非 PoS slot，需要决定如何映射（建议传 `None` 或共识 view）
- `BlockBuilder::finish(state_provider)` → `finish(state_provider, None)` — 第二个参数大概率是 `Option<TrieUpdates>`（与 sparse trie 集成关联）
- `BlockExecutorFor` 从 public 重导出列表移除 — N42 若有依赖需改 import 路径
- ConfigureEvm trait body 内部进一步重构（commit 范围 832 个）

### alloy-evm 0.29 → 0.34

- `EvmFactory::create_evm` / `create_evm_with_inspector` 签名调整概率高
- `EthEvm` / `EthEvmContext` 关联类型可能扩展
- `PrecompilesMap` 内部表示可能变（影响 `n42_precompiles` 的 leak 策略）

### revm 36 → 38

- 跨 2 major
- 影响 `n42-parallel-evm`（DatabaseRef / Bytecode / AccountInfo 类型）
- `Context::mainnet()` / `build_mainnet_with_inspector` API 可能改

### reth-n42.patch（705 行）

当前 patch 覆盖：
- reth_evm crate 的 N42 跳过/延后 state root 钩子（`n42_skip_state_root`, `n42_defer_state_root`）
- payload_validator.rs 中 N42 fast path（skip 后置验证）
- ethereum/payload/lib.rs 中 leader broadcast cache
- payload_cache.rs 新文件（双槽缓存）

Rebase 风险：reth v2 重构后这些 hook 位置可能不存在/重命名，需要重新找到对应 hook 点。

## 实施步骤

### Step A — 准备 (0.5 天)

1. 在 `D:\N42\reth` 创建升级分支：`git checkout -b n42-v2-upgrade-attempt`
2. 在 `D:\N42\n42-26` 创建对应分支：`git checkout -b reth-v2-upgrade-attempt`（基于本 RFC 落地后的 dev2603）
3. 备份当前 705 行 patch 到 `reth-n42-v1.patch.bak`
4. 备份 reth 当前 9 个 modified files 到 patch：`cd /d/N42/reth && git diff > /d/N42/n42-26/reth-local-mods-v1.patch`

### Step B — reth 主线 fast-forward (1 天)

1. `cd /d/N42/reth && git stash` 保存本地改动
2. `git fetch origin --tags`
3. `git reset --hard v2.2.0`
4. 尝试 `git apply /d/N42/n42-26/reth-n42.patch` — 必有 conflict
5. 用 patch 内容作为参考，**手工** 在 v2.2.0 上重新应用 N42 hooks（每个 hook 函数找 v2.2.0 对应位置）
6. 应用本地 modifications（payload_validator, ethereum/payload/lib.rs 等）— 同样 conflict 多
7. `cargo check -p reth` 通过

### Step C — workspace 依赖 bump (0.5 天)

1. `D:\N42\n42-26\Cargo.toml`:
   ```toml
   alloy-evm = { version = "0.34.0", default-features = false }
   revm = { version = "38.0.0", default-features = false, features = ["std", "serde"] }
   ```
   同时 bump alloy-eips/alloy-network/alloy-rpc-types-eth/alloy-sol-types 到对应版本
2. `cargo update -p alloy-evm -p revm`

### Step D — n42-execution 适配 (1-2 天)

1. `crates/n42-execution/src/evm_factory.rs`:
   - 重新检查 `EvmFactory` trait 关联类型（v0.34 可能新增 `BlockEnv` 类型族）
   - `create_evm` 签名调整
   - `n42_precompiles` 内部对接 `PrecompilesMap` 新 API
2. `crates/n42-execution/src/evm_config.rs`:
   - `ConfigureEvm` impl 同步适配（特别是 `NextBlockEnvAttributes` 加 `slot_number`）
3. `crates/n42-execution/src/executor.rs`:
   - `BlockBuilder::finish(provider, None)` 改两参数
   - `execute_with_state_closure` API 可能改名

### Step E — n42-parallel-evm 适配 (0.5 天)

revm 跨 2 major：
- `revm::context::TxEnv` / `BlockEnv` / `CfgEnv` 类型路径可能改
- `Context::mainnet()` / `build_mainnet_with_inspector` 验证
- `DatabaseRef` trait 可能新增方法

### Step F — n42-node / n42-consensus 余波 (1 天)

- `crates/n42-node/src/orchestrator/execution_bridge.rs` 用到 reth 内部类型，按编译错误逐个修
- `crates/n42-node/src/payload.rs` 同样
- `crates/n42-consensus` 仅依赖 n42-primitives，影响最小

### Step G — 集成验证 (1-2 天)

```powershell
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test --workspace --lib
cargo test -p n42-consensus --test integration_test
cargo build --release -p n42-node-bin
./scripts/testnet.sh --nodes 7 --clean
./scripts/step_stress.sh  # 30 min sustained
```

### Step H — 回归对比 (0.5 天)

- 阶段 1 基线（dev2603 + 阶段 1 patch）：commit p50 / TPS / memory
- 阶段 2 升级版：同样指标
- 任何 > 10% 回归需调查根因

## Risk Register

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| 705 行 patch rebase conflict 严重 | 高 | 高（可能要重写 hook） | 提前阅读 reth v2 evm crate 重构 PR，找到新 hook 位置；逐 hunk 应用 |
| alloy-evm 0.34 新增的关联类型缺失 | 中 | 中（编译错误） | 比对 reth v2.2.0 内部 `EthEvmFactory` 实现作模板 |
| revm 38 的 Database trait 加新方法 | 中 | 低（compile error 显式） | 按编译器提示补 default impl |
| BlockBuilder::finish 第二参数语义破坏现有 state root 路径 | 中 | 高（safety 风险） | 仔细 review 该参数含义；不传 trie updates 可能改变 reth 内部 sparse trie 演进（参考 devlog-32 失败教训） |
| 升级后某个 reth 内部 API 不再 pub | 中 | 中 | 找上游 PR 中暴露新 API 的方案；最坏情况打补丁补 pub |
| 7-node testnet 出现 consensus livelock / state root mismatch | 低 | 严重 | 完整测试 e2e suite (1, 3, 4, 5, 8, 12) 在合并前必须通过 |
| 阶段 2 工时超估 | 高 | — | 拆分为 7 个 PR 独立合并（reth bump / workspace deps / each adapter crate） |

## 成功标准

- 所有现有单元测试通过（阶段 1 基线：731 个）
- `cargo clippy --all-targets -- -D warnings` 零警告（不计 reth 内部 warning）
- E2E 场景 1, 3, 4 通过（smoke-consensus profile）
- 7-node testnet 30 min 压测：commit p50 不退化 > 10%，TPS 不退化 > 10%
- mobile-rpc E2E 场景 5, 8, 12 通过
- 升级 PR 拆为 ≤ 7 个 atomic commit，便于 review 与回滚

## 回滚

- 各步骤独立可回滚（reset --hard 到 base + 阶段 1 状态）
- workspace alloy-evm 改回 0.29.2 + revm 36.0.0 即可立即恢复阶段 1 状态
- reth path 用 `reth-local-mods-v1.patch` + `reth-n42-v1.patch.bak` 重新应用即可恢复

## Open Questions

- `NextBlockEnvAttributes::slot_number` 在 N42 (HotStuff-2 view) 下应该传什么？建议传 view，但需确认 reth 内部对 slot 的语义假设是否兼容
- `BlockBuilder::finish` 的第二参数（疑似 `Option<TrieUpdates>`）：传 `None` 与 `devlog-32` 失败教训冲突吗？需在 PR 中证明
- reth v2 是否引入了 Engine API V3+ 的新约束（payload attributes V3, blob params 等）影响 N42 现有的 EIP-4895 withdrawals 注入路径？

## Dependencies

- 阶段 1 完成（本 PR 已就绪）
- 阅读上游 reth v1.11.x → v2.0 release notes（链接：https://github.com/paradigmxyz/reth/releases）
- 阅读 alloy-evm 0.30 → 0.34 changelog

## Stage 2 Attempt Notes (2026-05-13)

阶段 2 在本会话中已部分尝试，**因 reth v2.x 上游 Windows 兼容性 bug 受阻**，主体改动已 snapshot 到 patch 文件保留：

### 已完成的改动（保留为 patch，未应用）

- `stage2-workspace-deps.patch` (48 行)：workspace Cargo.toml deps bump（alloy-evm 0.34, revm 38, alloy-consensus/eips/genesis/rpc-types-engine/signer/signer-local/network/rpc-types-eth → 2.0.4, reth-primitives-traits → 0.3.1）
- `stage2-n42-execution.patch` (155 行)：
  - `evm_factory.rs` 重写 — 用 `EthEvmBuilder::new(db, evm_env).precompiles(...).build()` 替代 `Context::mainnet()` 模式
  - `precompile_random.rs` — `PrecompileFn` 加 `reservoir: u64` 第 3 参数（EIP-8037 状态 gas 分离）；revm 38 把 `PrecompileError` 拆为 `PrecompileError`(fatal) + `PrecompileHalt`(non-fatal)，`OutOfGas` / `Other` 走 halt 路径返回 `Ok(PrecompileOutput::halt(...))`
  - `witness.rs` — `ExecutionWitnessRecord::from_executed_state(state)` 加 `mode: ExecutionWitnessMode` 第 2 参数；N42 选 `ExecutionWitnessMode::Legacy` 维持手机端兼容
- `stage2-n42-parallel-evm.patch` (165 行)：`result.gas_used()` → `tx_gas_used()`（EIP-8037 deprecation）
- `stage2-reth-roaring-bump.patch` (13 行)：reth Cargo.toml `roaring = "0.11.3" → "0.11.4"`（v2.2.0 tag 上 `IntegerList` derive `Eq` 但 0.11.3 不 impl，这是 reth 上游已修但未合并到 v2.2.0 tag 的 base bug）

### 受阻原因 — reth v2 Windows 兼容性

reth v2.2.0 与上游 main 在 `crates/static-file/types/src/changeset_offsets.rs` 中**无条件** `use std::os::unix::fs::FileExt;`，并把 `ChangesetOffsetReader/Writer` 类型用 `#[cfg(all(feature = "std", unix))]` 守门，导致 `crates/storage/provider/src/providers/static_file/{writer,mod}.rs` 在 Windows 上 `unresolved import` × 11 errors。

这是 reth **上游本身的 Windows compat 缺陷**，与 N42 patch 无关。CI（ubuntu-latest）应该可以 build；本地 Windows 无法。

### 重启阶段 2 的前置条件（按优先级）

1. **首选**：等 reth 上游为 `changeset_offsets.rs` 加 Windows 实现（用 `std::os::windows::fs::FileExt::seek_read/seek_write` 等价物），或者把 reth-provider 中所有调用点也 `#[cfg(unix)]` 守门
2. **备选**：在 WSL2 / Docker Linux 环境内做阶段 2 升级 + 验证；CI 已是 ubuntu-latest 可直接跑
3. **不推荐**：fork reth 自己补 Windows 实现 — 维护成本高

### 重启操作步骤（Linux/WSL2 环境）

```bash
cd /path/to/n42-26
git apply stage2-workspace-deps.patch
git apply stage2-n42-execution.patch
git apply stage2-n42-parallel-evm.patch
cd ../reth
git checkout v2.2.0
git apply ../n42-26/stage2-reth-roaring-bump.patch
# rebase 705 行 reth-n42.patch + 8 个本地 modified files 到 v2.2.0（手工）
# 然后回到 n42-26 跑 cargo check + test
```

### 阶段 1 当前状态

workspace 与本地 reth (v1.11.3 base + N42 patches) 完全对齐，全核心 crate 编译通过，731/731 + 206 = 937 tests pass。继续基于阶段 1 做迭代是当前可行路径。
