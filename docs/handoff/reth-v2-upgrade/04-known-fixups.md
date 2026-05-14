# 04 — 已定位但未实施的修复点

apply 完 patches + rebase 完 reth hook 之后，下面是我在 Windows 短暂尝试 build 时**已确认会出现**的剩余编译错误，以及修复方向。在 Mac 上跑 cargo check 后按这个清单逐个解决。

## A. `NextBlockEnvAttributes::slot_number` 新字段

**症状**：n42-node 或 n42-execution 构造 `NextBlockEnvAttributes { ... }` 处编译失败，"missing field `slot_number`"。

**位置**：`crates/n42-node/src/orchestrator/execution_bridge.rs` `build_payload_attributes` 或 `do_trigger_payload_build` 附近。

**修复**：
```rust
NextBlockEnvAttributes {
    timestamp,
    suggested_fee_recipient,
    prev_randao,
    gas_limit,
    withdrawals: Some(withdrawals),
    parent_beacon_block_root,
    slot_number: None,  // ← 新增字段；N42 用 HotStuff-2 view 而非 PoS slot，传 None 让 reth 默认
}
```

**开放问题**：N42 是否应该把 HotStuff-2 view 映射到 slot_number？目前建议传 `None` —— 如果 reth 内部对 slot 假设 sequential / monotonic，N42 view 在 timeout / view jump 后**不严格 monotonic**，传 view 反而会破坏 reth 假设。先 `None` 安全。

## B. `BlockBuilder::finish(state_provider, ???)` 第二参数

**症状**：调用 `.finish(provider)` 处编译失败，"expected 2 arguments, found 1"。

**位置**：reth path 内部（不是 N42 代码），但 N42 patch 可能在重写的 payload builder 里有调用点。grep `\.finish(` in reth-n42.patch 改 hunk。

**修复**：第二参数是 `Option<TrieUpdates>`。传 `None` 让 reth 自己计算（与 v1 等价语义）。

**警惕**：传 `None` ≠ 传 `Some(TrieUpdates::default())`。后者会破坏 sparse trie 状态（参考 `docs/devlog-32-compact-block-delayed-sr.md` 的 v1 失败教训）。所以一定传 `None`。

## C. `BlockExecutorFor` 不再 public re-export

**症状**：n42-execution 中 `use reth_evm::BlockExecutorFor;` 失败。

**修复**：
- 选项 1：从 `alloy_evm::block::BlockExecutorFor` 直接 import（v2 把它推到 alloy）
- 选项 2：如果只用作 type bound，看能否去掉这个 import（v2 可能内化为 trait bound）

具体哪个对，看编译错误的 "did you mean" 建议。

## D. `EthEvmContext<DB>` 在 0.34 的 generic 参数变化

**症状**：N42EvmFactory 的 `type Context<DB: Database> = EthEvmContext<DB>;` 处编译失败，因为 v0.34 中：
```rust
// alloy-evm 0.34
type Context<DB: Database> = Context<BlockEnv, TxEnv, CfgEnv, DB>;
```

但 `EthEvmContext<DB>` 在 0.34 中是 `pub type EthEvmContext<DB> = Context<BlockEnv, TxEnv, CfgEnv, DB, Journal<DB>, ()>;` —— 比 `Context<BlockEnv, TxEnv, CfgEnv, DB>` 多了 Journal + Halt extra params。可能编译器期望前者。

**修复**：把 `type Context<DB: Database> = EthEvmContext<DB>;` 换成 `Context<BlockEnv, TxEnv, CfgEnv, DB>`，或者让 trait bound 兼容。具体看错误信息。

## E. `Precompile::new` 是否还在

**症状**：`evm_factory.rs` 中 `Precompile::new(id, addr, fn_)` 失败，"function takes N arguments, supplied M"。

**修复**：v0.34 中 `Precompile::new` 应该还在（grep `/c/Users/.../alloy-evm-0.34.0/` 里有），3 参数版应该保留。如果签名变了，新签名应该在 patch 里——比较错误信息和 alloy-evm 0.34 源码。

## F. revm 38 各种 `gas_used` deprecation

`tx_gas_used()` 是 transaction-level gas，对应旧 `gas_used()`。grep 其他位置：
```bash
cd ~/N42/n42-26
rg "\.gas_used\(\)" --type rust
```

每处都判断：是要 tx 总 gas（→ `tx_gas_used`），还是 state gas（→ `state_gas_used`），还是某种 sum（→ 显式加）。一般 tx-level 是对的。

## G. reth 内部 alloy-evm 类型别名

reth v2.2.0 内部可能引入了新的 type alias 替代旧 path。如果 n42-node 或 n42-execution import 失败，常见替换：

| v1 import | v2 替换 |
|---|---|
| `reth_evm::EvmEnv` | `alloy_evm::EvmEnv` 或 `reth_evm::EvmEnvFor<Self>` |
| `reth_evm::ConfigureEvm` | 一样（trait 不变，但 super-trait bound 加了，见下） |
| `reth_evm_ethereum::EthEvmConfig` | 一样，但 `new_with_evm_factory` 签名可能变 |
| `reth_primitives_traits::NodePrimitives` | 一样，但 v0.3.1 比 v0.1.0 多了 super-trait bound |
| `reth_revm::witness::ExecutionWitnessRecord` | 一样 |

## H. Test: `cargo test -p n42-execution` 的 mock 类型

测试代码里 mock `ChainSpec` / `PayloadAttributes` 可能因为 v2 加了新字段失败。修复方式与 A/B 类似——加默认字段。

## I. `crates/n42-node/src/payload.rs` 自定义 payload 类型

如果 N42 自定义了 payload (`N42PayloadAttributes` 等)，需要适配新 trait bound。grep:
```bash
rg "PayloadAttributes for" crates/n42-node/src/
rg "PayloadTypes for" crates/n42-node/src/
```

对每个 impl 检查 v2.2.0 的 trait 定义有没有新方法/关联类型。

## J. workspace 其他依赖版本陷阱

可能遇到的次级版本不匹配（少数情况）：

- `alloy-rlp` workspace 写 0.3.13，reth v2 也用 0.3.13，应该一致
- `alloy-primitives` 1.5.6，reth v2 用 1.5.6（v2.0.4 是 alloy-consensus 等，不是 alloy-primitives）
- `revm-primitives` 23.0.0（reth v2 lock），workspace 不显式 pin → 由 alloy-evm/revm 拉出

如果遇到双版本（cargo tree -i 显示多个版本号），同时存在 alloy-* 1.8.2 和 2.0.4 这种，说明 patch 1 (workspace deps) 没全 apply 到。检查 `Cargo.toml` 那 11 行 alloy/revm pin 是否都已切到新版。

## K. 上游 main 已修但 v2.2.0 tag 没合的其他 cherry-pick

我只发现了 `40c30dbc7 chore(db): derive Eq for IntegerList` 这一个（已通过 roaring bump 间接修）。如果你 build 时遇到 reth path **自身**报奇怪错误（不是 N42 hook 处），值得查：

```bash
cd ~/N42/reth
git log v2.2.0..origin/main --oneline -- <出错的文件相对路径>
```

看是否有 fix commit。有的话 cherry-pick 到你的 `n42-v2-upgrade` 分支。

## L. clippy 可能的新 lint

Rust 1.93 + clippy 可能有新的 default deny lint。本项目用 `-D warnings`，所以新的 lint 会变成 error。修复方式：
- 简单 lint：按 clippy suggest 修
- 难修的 lint：加 `#[allow(clippy::lint_name)]` + 注释解释（参考 stage1 我对 `map_entry` 的处理）

---

## 接下来读 `05-verify-and-handback.md`
