# 00 — 完整上下文

## 项目结构（Mac 上重建后等价）

```
~/N42/                      ← 你在 Mac 上选个目录，建议保留这个布局
├── n42-26/                 ← 主仓库（包含本 handoff 目录）
│   ├── Cargo.toml          ← workspace
│   ├── bin/n42-node/
│   ├── crates/
│   │   ├── n42-primitives/ n42-chainspec/ n42-consensus/ n42-execution/
│   │   ├── n42-network/ n42-node/ n42-parallel-evm/ n42-jmt/ n42-zkproof/
│   │   ├── n42-mobile/ n42-mobile-ffi/
│   │   └── n42-zkproof-guest/ (workspace excluded — SP1 RISC-V toolchain)
│   └── docs/handoff/reth-v2-upgrade/ ← 你在这里
└── reth/                   ← reth path 依赖（n42-26 用 `path = "../reth"`）
```

`n42-26` 的 `Cargo.toml` 中所有 `reth-*` 依赖都是 `path = "../reth/..."`，所以两个 repo 必须放在同一父目录。

## 当前两个 repo 的精确状态（Windows 端）

### `n42-26` git status (working tree, 未 commit)

```
M  Cargo.lock
M  Cargo.toml                                          ← 阶段 1: alloy-evm 0.30 → 0.29.2
M  crates/n42-consensus/src/protocol/bounded_fifo.rs   ← 阶段 1: map_entry clippy
M  crates/n42-consensus/src/protocol/state_machine.rs  ← 主路径优化: process_event tracing span
M  crates/n42-network/src/transport.rs                 ← 主路径优化: N42_GOSSIP_MESH_D env override
M  crates/n42-node/src/mobile_bridge.rs                ← 阶段 1: collapsible_if clippy
M  crates/n42-node/src/orchestrator/consensus_loop.rs  ← 主路径优化: 4 metric + collapsible_if
M  crates/n42-node/src/orchestrator/execution_bridge.rs ← 主路径优化: outcome label + tracing instrument + too_many_args
M  crates/n42-parallel-evm/src/lib.rs                  ← 主路径优化: parallel_execute tracing instrument
M  crates/n42-parallel-evm/src/mv_memory.rs            ← 主路径优化: clear_tx 索引化 + histogram
M  docs/01-System-Architecture.md                      ← 主路径优化: JMT 状态修正
M  docs/02-Core-Flows.md                               ← 主路径优化: JMT 状态修正

?? .mcp.json
?? docs/rfc/                                           ← 2 个 RFC + 本 handoff 目录
?? reth-local-mods-pre-upgrade.patch                   ← 备份 (在 patches/ 里有副本)
?? reth-n42.patch.bak                                  ← 备份
?? reth-payload-cache-pre-upgrade.rs                   ← 备份 (在 patches/ 里有副本)
?? stage2-*.patch                                      ← 4 个阶段 2 patch (在 patches/ 里有副本)
```

**branch**: `dev2603`, base commit `248caaa` ("test: add symmetric partition test + view/height ratio check")

### `reth` git status (working tree, 未 commit)

```
* (HEAD detached at 27781443a)
* branch: n42-pre-v2-upgrade (本地 backup branch，指向 27781443a + 后述 working tree mods)

M  Cargo.lock
M  Cargo.toml                                          ← 阶段 1 backing: alloy-evm 0.29.2 / revm 36
M  crates/engine/tree/src/tree/payload_validator.rs    ← N42 hook (skip post-validation 等)
M  crates/ethereum/payload/src/lib.rs                  ← N42 hook (leader broadcast cache)
M  crates/evm/evm/Cargo.toml
M  crates/evm/evm/src/execute.rs
M  crates/evm/evm/src/lib.rs
M  crates/fs-util/src/lib.rs
M  crates/rpc/rpc-eth-types/src/utils.rs
?? crates/evm/evm/src/payload_cache.rs                 ← 新增 file (74 行)
```

**HEAD**: `27781443a6e6e71c93bbbe05012f0ac9595f9dac` (上游某早期版本 + 8 modified + 1 untracked)
**上游 v2.2.0**: `88505c7fc`
**上游 main 最新**: `968e50b35` (本会话拉取时)

## 阶段 1 已完成（commit 前的 working tree 状态）

阶段 1 的目的：让 base 编译通过。改动很小但关键：

1. **workspace `Cargo.toml`**：`alloy-evm` 从 `0.30.0` → `0.29.2`（与 reth 1.11.3 实际使用版本对齐）。revm 仍是 36，所有 alloy-* 仍是 1.8.2。
2. **base bug fix** (clippy lints，全是预先存在但 base 编译不过没人发现的):
   - `crates/n42-consensus/src/protocol/bounded_fifo.rs` — `map_entry` lint，加 `#[allow]` + 注释解释为何不能用 `Entry` API（borrow checker 冲突）
   - `crates/n42-node/src/mobile_bridge.rs:228` — collapsible `if let` 合并 `&&`
   - `crates/n42-node/src/orchestrator/consensus_loop.rs:473` — 同上
   - `crates/n42-node/src/orchestrator/execution_bridge.rs:1179` — `broadcast_block_data` 加 `#[allow(clippy::too_many_arguments)]`

阶段 1 完成后：
- `cargo check -p {n42-execution,n42-consensus,n42-node,n42-network,n42-jmt,n42-parallel-evm,n42-primitives,n42-chainspec,n42-mobile}` ✓
- `cargo clippy --lib -- -D warnings` ✓（剩余警告全在 reth path 内）
- `cargo test --lib` → **937/937 passed** (execution 39, consensus 64, node 206, network 117, jmt 181, parallel-evm 2, primitives 91, chainspec 31, mobile 28+其他若干)

## 阶段 2 已完成的代码工作（patch 文件已生成）

### Patch 1: `stage2-workspace-deps.patch` (48 行, 改 n42-26 Cargo.toml)

```diff
-alloy-consensus = "1.8.2"            (+ alloy-eips/genesis/rpc-types-engine/signer/signer-local/network/rpc-types-eth)
+alloy-consensus = "2.0.4"
-alloy-evm       = "0.29.2"
+alloy-evm       = "0.34.0"
-revm            = "36.0.0"
+revm            = "38.0.0"
-reth-primitives-traits = "0.1.0"
+reth-primitives-traits = "0.3.1"     (v2 把它从 reth path 移到 crates.io 独立发布)
```

注意：注释提到"Stage 2 受阻"那段在 patch 里是阶段 1 状态，apply patch 后会被换成阶段 2 的注释。

### Patch 2: `stage2-n42-execution.patch` (155 行, 改 3 个文件)

#### a) `crates/n42-execution/src/evm_factory.rs` — 重写

**旧** (alloy-evm 0.29.2):
```rust
let evm = Context::mainnet()
    .with_db(db).with_cfg(input.cfg_env).with_block(input.block_env)
    .build_mainnet_with_inspector(NoOpInspector {})
    .with_precompiles(PrecompilesMap::from_static(n42_precompiles(spec)));
EthEvm::new(evm, false)
```

**新** (alloy-evm 0.34, 用 `EthEvmBuilder`):
```rust
EthEvmBuilder::new(db, evm_env)
    .precompiles(PrecompilesMap::from_static(n42_precompiles(spec)))
    .build()
```

`PrecompileSpecId::from(spec)` 改名为 `PrecompileSpecId::from_spec_id(spec)`。
注意 `n42_precompiles` 函数里的 `Precompile::new(PrecompileId::custom(...), addr, fn_)` — 第 3 参数 `fn_` 现在指向新签名的 `revm_precompile_fn`（见下）。

#### b) `crates/n42-execution/src/precompile_random.rs` — 三处改

1. `use revm::precompile::{PrecompileError, ...}` → `{PrecompileHalt, ...}` (新拆分)
2. `revm_precompile_fn(input: &[u8], gas_limit: u64) -> PrecompileResult`
   →
   `revm_precompile_fn(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult` (EIP-8037 state gas split)
3. `Err(PrecompileError::OutOfGas)` → `Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir))`
   `Err(PrecompileError::other(...))` → `Ok(PrecompileOutput::halt(PrecompileHalt::other(...), reservoir))`
   `Ok(PrecompileOutput::new(gas, bytes))` → `Ok(PrecompileOutput::new(gas, bytes, reservoir))` (third arg added)

**语义**：revm 38 把"非致命挂起"(OutOfGas、输入格式错等) 与"致命错误"(EVM abort) 拆开。N42 randomness precompile 的所有错误本来都应该是非致命，所以走 `Ok(Halt)` 路径。

#### c) `crates/n42-execution/src/witness.rs`

```rust
use reth_trie_common::{ExecutionWitnessMode, HashedPostState};
// ...
let record = ExecutionWitnessRecord::from_executed_state(state, ExecutionWitnessMode::Legacy);
```

第 2 参数 `mode` 是 v2 引入的。`Legacy` 保留 v1 行为（不去重 / 不排序 codes）——必须用 Legacy 才能维持 N42 mobile packet 兼容性，**绝对不要切到 Canonical**。

### Patch 3: `stage2-n42-parallel-evm.patch` (165 行)

只有一处实质改动：`result.gas_used()` → `result.tx_gas_used()`。EIP-8037 之后 `gas_used()` deprecated，因为 ambiguous between 执行 gas 和状态 gas。N42 收据合计要的是**交易级**总 gas，等价于 `tx_gas_used()`。

### Patch 4: `stage2-reth-roaring-bump.patch` (13 行, 改 ../reth/Cargo.toml)

```diff
-roaring = "0.11.3"
+roaring = "0.11.4"
```

这是 **reth v2.2.0 tag 上的 base bug**：上游已 derive `Eq` for `IntegerList(RoaringTreemap)`（commit `40c30dbc7`），但 roaring 0.11.3 没 impl `Eq`，0.11.4 才有。上游已修但**未合入 v2.2.0 tag**。Apply 这个 patch 才能让 `reth-db-api` 编译。

## 我撞到的卡点（你应该不会撞到，因为你在 Linux/macOS）

**`crates/static-file/types/src/changeset_offsets.rs`** (reth v2.x 上游) 用 `std::os::unix::fs::FileExt`，并用 `#[cfg(all(feature = "std", unix))]` 把 `ChangesetOffsetReader/Writer` 类型导出守门。但 `crates/storage/provider/src/providers/static_file/{writer,mod}.rs` **无条件** import 这两个类型 — Windows 上 → 11 个 unresolved import + 后续 type inference error。

我看了上游 `origin/main` 的版本，**main 上仍然是 Unix-only**。所以这是 reth 上游真实的 Windows compat 缺陷。

Mac 是 Unix，**不受影响**。你在 macOS / Linux 上 build 应该直接通过。如果你在 Mac 上**也**撞到这个，那就是别的问题（可能是 macOS 与 Linux Unix 实现略有差异），到时再说。

## 接下来读 `01-prerequisites.md`
