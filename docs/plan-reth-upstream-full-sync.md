# 计划：reth fork 全量同步 upstream main

> 状态：**待执行**（独立大工程，需专门排期 + CI/mac E2E）。本文是 roadmap，不是已完成记录。
> 背景见 `docs/devlog-58/59/60` + CLAUDE.md「reth fork 基线」。

## 1. 目标与现状

把 `../reth`（n42 fork，分支 `chore/merge-upstream-main`，HEAD `53034fea4`）同步到 upstream
`paradigmxyz/reth` main 最新，保留全部 N42 hooks。

| 维度 | 数值（截至本文撰写） |
|------|---------------------|
| fork 落后 upstream main | **~330 commits** |
| 总 diff | **415 files / +25957 −15531** |
| merge 冲突 | **~50 文件**（分类见 §3） |
| **依赖版本差异** | **已无**：本轮已把 fork + n42-26 对齐到 upstream main 的 `alloy-evm 0.36 / revm 40.0.3 / alloy-primitives 1.6.0 / reth-primitives-traits 0.4.0`（commit fork `04c7f29f`、n42-26 `ce16528`） |

**关键**：依赖版本已经对齐,全量 merge **不会再引入依赖升级**;难点纯粹是**代码层面把 330 commits 的
upstream 变化融进 N42 hooks**。

## 2. 前置（必做）

1. 确认 `../reth` 干净 + 在 `chore/merge-upstream-main`：`git -C ../reth status -s` 应空。
2. **建备份分支**：`git -C ../reth branch backup/pre-upstream-sync-$(date +%Y%m%d)`（本机无 Date，
   手动填日期）。merge 高风险,务必可回退。
3. `git -C ../reth fetch origin main`（origin = paradigmxyz）。
4. 环境：**mac/Linux**（reth 全量编译 + 测试网 E2E；Windows 编得过但跑不了测试网）。
5. n42-26 侧依赖 pin 不动（已对齐）；merge 后 n42-26 `git diff Cargo.toml` 应仍为空。

## 3. 冲突分类与策略（~50 文件）

### A. 琐碎，直接取 upstream（~22 文件）
`.github/scripts/*`、`.github/workflows/bench.yml`、`docs/vocs/docs/pages/cli/reth/*`（16 个 mdx）、
`deny.toml`。N42 无实质改动 → `git checkout --theirs <file>`。

### B. 依赖/配置，对齐合并（~5 文件）
`Cargo.toml`、`Cargo.lock`、`crates/ethereum/node/Cargo.toml`、`crates/rpc/rpc-eth-types/Cargo.toml`、
`crates/tracing/Cargo.toml`。取 upstream 的依赖版本（已对齐），保留 N42 新增的 path/feature 条目。
`Cargo.lock` 冲突直接删冲突标记后 `cargo update -p <changed>` 重新生成。

### C. 🔴 核心 N42 hooks，逐个理解后融合（~15 文件，主要工作量）
**这些是 N42 对 reth 的执行/共识/缓存/payload 改动，必须保留 N42 语义 + 吸收 upstream 新逻辑。**
本轮 alloy-evm 0.36 适配已经动过其中 5 个（标 ✦），那些 merge 时 N42 侧已是 0.36，冲突更小。

| 文件 | N42 hook 职责（保留什么） |
|------|--------------------------|
| `crates/chain-state/src/deferred_trie.rs` | N42 状态缓存：compact block + 执行输出缓存（命中 ~3ms） |
| `crates/chain-state/src/state_trie_overlay.rs` | N42 state trie overlay 缓存层 |
| `crates/chain-state/src/in_memory.rs` | 上述缓存接入 in-memory state |
| `crates/engine/tree/src/tree/payload_processor/mod.rs` ✦ | N42 payload 处理 + state-hook（0.36 已适配 test） |
| `crates/engine/tree/src/tree/payload_processor/prewarm.rs` | N42 预热 + HashedStateUpdate 流 |
| `crates/engine/tree/src/tree/payload_validator.rs` ✦ | N42 import 校验 + state-hook（0.36 已适配） |
| `crates/ethereum/consensus/src/lib.rs` | N42 共识：`validate_block_post_execution(.., block_access_list_hash)` 多参 |
| `crates/ethereum/consensus/src/validation.rs` | N42 校验逻辑（BAL/receipt root） |
| `crates/ethereum/engine-primitives/src/payload.rs` | N42 payload 类型（`block_to_payload(.., None)` 等签名） |
| `crates/ethereum/node/src/engine_ssz_proxy.rs` | N42 新增：engine API SSZ proxy |
| `crates/ethereum/payload/src/config.rs` | N42 payload 配置 |
| `crates/ethereum/payload/src/lib.rs` ✦ | N42 sparse-trie 增量 state root hook（0.36 已适配） |
| `crates/evm/evm/src/execute.rs` ✦ | N42 BAL（`take_bal`/`BlockAccessList`）+ state-hook（0.36 已适配） |
| `crates/evm/execution-types/src/chain.rs` | N42 execution outcome（BAL 字段） |
| `crates/trie/parallel/src/state_root_task.rs` ✦ | N42 `Source`/BAL state-root task（0.36 已适配，`Source::Evm` 无参） |

**策略**：对每个文件 `git log origin/main -- <file>` 看 upstream 改了什么 → 手动 merge：以 N42 侧为基，
逐块吸收 upstream 的非冲突改进；冲突块判断是「upstream 重构了 N42 也碰的代码」还是「两者独立改不同部分」。
✦ 文件因 0.36 已对齐，优先处理（最简单）。

### D. 其它 reth 内部，看 upstream 变化轻量解决（~8 文件）
`bin/reth-bb/src/evm.rs`、`bin/reth/tests/it/main.rs`、`crates/cli/commands/src/download/mod.rs`、
`crates/ethereum/node/tests/e2e/eth.rs`、`crates/net/network/src/peers.rs`、
`crates/node/core/src/args/database.rs`、`crates/rpc/rpc-eth-types/src/simulate.rs`、
`crates/tracing/src/lib.rs`。多为 upstream 重命名/签名调整，N42 改动浅 → 多数可取 upstream，少数补 N42 的小改。

## 4. 分阶段执行

1. **A+D 批量**：先解决琐碎 + 浅冲突（取 upstream 为主），缩小冲突面到核心 hooks。
2. **B 依赖**：解决 Cargo.toml/lock，`cargo metadata` 确认依赖树。
3. **C 核心 hooks 逐个**：按上表，✦ 文件先做。每个文件解决后 `cargo check -p <对应 crate>` 即时验证。
4. **fork 全量编译**：`cargo check --workspace`（在 ../reth），逐个修 330-commit 带来的 API 适配
   （参考本轮 0.36 的 OnStateHook 适配模式：upstream 改了签名就照 upstream 改 N42 调用点）。
5. **n42-26 适配**：`cargo check -p n42-node-bin`（n42-26 侧），修 reth API 变化导致的 n42 适配。
6. **测试**：`cargo test` reth 关键 crate（evm/engine-tree/trie/consensus）+ n42 全 crate。
7. **E2E（mac/CI）**：跑测试网 + 既有 E2E 场景（1,3,4,5,8,12）+ SBMT proof E2E
   （`docs/codex-e2e-sbmt-verification.md`）。这是上线前唯一能验证 N42 hooks 没被 merge 破坏的手段。

## 5. 验收标准

- [ ] `../reth` `cargo check --workspace` 通过
- [ ] n42-26 `cargo check --all-targets` 通过；`git diff Cargo.toml` 仅含预期依赖变化
- [ ] reth evm/engine-tree/trie/consensus 单测通过；n42 全 crate 单测通过
- [ ] 测试网出块正常；E2E 场景 1,3,4,5,8,12 通过；SBMT `n42_jmtProof` 往返通过
- [ ] N42 关键能力无回归：compact block 缓存命中、BAL（EIP-7928）、sparse-trie 增量 state root、
      手机 proof

## 6. 风险与回退

- **最大风险**：核心 hooks（chain-state 缓存、evm BAL、consensus 校验）被 upstream 重构覆盖，
  导致执行/共识语义偏差，单测难发现 → **靠 E2E + 状态根一致性兜底**。
- **回退**：`git -C ../reth reset --hard backup/pre-upstream-sync-<date>`。
- **分批落地**：可考虑不一次合 330 commits，而是分几段（按 upstream tag/周）多次 merge，每段验证，
  降低单次冲突规模。
- **不要为编过而降级依赖**（见 CLAUDE.md 基线表 + devlog-60 教训）。

## 7. 参考

- 本轮 alloy-evm 0.36 适配（OnStateHook：`with_state_hook`→`set_state_hook`、`StateChangeSource`
  移除）：fork commit `04c7f29f`，可作为「upstream 改签名 → 照改 N42 调用点」的模板。
- N42 hooks 总览：CLAUDE.md「关键设计」+ `docs/01-System-Architecture.md`。
