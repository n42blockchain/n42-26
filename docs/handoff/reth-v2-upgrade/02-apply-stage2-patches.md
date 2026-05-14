# 02 — 按顺序 apply 阶段 2 patches

前置：完成 `01-prerequisites.md`，确保 baseline 编译通过。

## 顺序很重要

1. `stage2-reth-roaring-bump.patch` — 改 `~/N42/reth/Cargo.toml`
2. `stage2-workspace-deps.patch` — 改 `~/N42/n42-26/Cargo.toml`
3. `stage2-n42-execution.patch` — 改 `~/N42/n42-26/crates/n42-execution/src/{evm_factory,precompile_random,witness}.rs`
4. `stage2-n42-parallel-evm.patch` — 改 `~/N42/n42-26/crates/n42-parallel-evm/src/lib.rs`

> 这 4 个 patch 都是 **在当前 reth 1.11.3 base 上**生成的（基于 Cargo.lock 中现有的依赖结构）。它们 apply 到 reth working tree 没切到 v2.2.0 时也成立。但**你不能就这么 build**——下一步 `03-rebase-n42-patches-onto-v2.md` 才是把 reth path 切到 v2.2.0。
>
> 所以这一步 apply 完后**不要 cargo check**，先做完 03 再 build。

## 1. roaring bump

```bash
cd ~/N42/reth
git apply ~/N42/n42-26/docs/handoff/reth-v2-upgrade/patches/stage2-reth-roaring-bump.patch
git diff Cargo.toml | head -5
# Expected:
# -roaring = "0.11.3"
# +roaring = "0.11.4"
```

## 2. n42-26 workspace deps

```bash
cd ~/N42/n42-26
git apply docs/handoff/reth-v2-upgrade/patches/stage2-workspace-deps.patch
git diff Cargo.toml | grep -E "alloy-(evm|consensus|signer|network)|^[+-]revm =|reth-primitives-traits"
# Expected (示意):
# -alloy-consensus = "1.8.2"
# +alloy-consensus = "2.0.4"
# -alloy-evm       = "0.29.2"
# +alloy-evm       = "0.34.0"
# -revm            = "36.0.0"
# +revm            = "38.0.0"
# -reth-primitives-traits = "0.1.0"
# +reth-primitives-traits = "0.3.1"
```

## 3. n42-execution 三处适配

```bash
git apply docs/handoff/reth-v2-upgrade/patches/stage2-n42-execution.patch
git status --short crates/n42-execution/src/
# Expected:
# M crates/n42-execution/src/evm_factory.rs
# M crates/n42-execution/src/precompile_random.rs
# M crates/n42-execution/src/witness.rs
```

如果 `git apply` 失败（之前你已经手动改了文件），用：
```bash
git apply --3way docs/handoff/reth-v2-upgrade/patches/stage2-n42-execution.patch
```

### Patch 内容速查

**evm_factory.rs** 的核心改动：
```rust
// 旧: Context::mainnet().with_db().with_cfg().with_block().build_mainnet_with_inspector(...).with_precompiles(...)
// 新:
EthEvmBuilder::new(db, evm_env)
    .precompiles(PrecompilesMap::from_static(n42_precompiles(spec)))
    .build()
```

**precompile_random.rs** 关键改动：
- 函数签名加 `reservoir: u64` 第 3 参数
- `Err(PrecompileError::OutOfGas)` → `Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir))`
- `PrecompileOutput::new(gas, bytes)` → `PrecompileOutput::new(gas, bytes, reservoir)`

**witness.rs** 关键改动：
```rust
let record = ExecutionWitnessRecord::from_executed_state(state, ExecutionWitnessMode::Legacy);
```

## 4. n42-parallel-evm gas_used → tx_gas_used

```bash
git apply docs/handoff/reth-v2-upgrade/patches/stage2-n42-parallel-evm.patch
```

实际变更只有 1 行（`crates/n42-parallel-evm/src/lib.rs:256`）：`result.gas_used()` → `result.tx_gas_used()`。

## 验证 patch 都 apply 上了

```bash
cd ~/N42/n42-26
git status --short | grep -E "Cargo\.toml|n42-execution|n42-parallel-evm" | head
# Expected (排除阶段 1 / 主路径优化的其他 modified files):
# M Cargo.toml
# M crates/n42-execution/src/evm_factory.rs
# M crates/n42-execution/src/precompile_random.rs
# M crates/n42-execution/src/witness.rs
# M crates/n42-parallel-evm/src/lib.rs
```

```bash
cd ~/N42/reth
git status --short
# 在原 8 modified + 1 untracked 基础上多一行：
# M Cargo.toml  (变成 roaring 0.11.4)
```

## 不要现在 cargo check

下一步 `03-rebase-n42-patches-onto-v2.md` 会把 reth path 切到 v2.2.0；只有切完之后 cargo lock 才能正确解析 alloy 2.0.4 / revm 38。

如果你出于好奇 `cargo check` 了，会看到一大堆 reth 1.11.3 内部代码相关的错误（因为现在 lock 是混合状态）。**忽略这些**，继续 03。
