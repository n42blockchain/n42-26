# devlog-137：reth upstream 2026-07-26 合并评估（未切换基线）

## 起因

`../reth` 无法 pull。查明后与"冲突"无关：

- `git pull`（默认，跟踪 `n42/chore/reth-upstream-20260719`）返回 `Already up to date`；
- 本地与 n42 远端 0/0 完全同步，工作区干净；
- 冲突只会出现在**用 rebase 拉 upstream** 时。

本 fork 的历史是一串 merge commit（5 个 `Merge paradigmxyz/main into N42 fork`），
不是线性 rebase 历史。`git pull --rebase origin main` 要把 10 个提交（含 5 个 merge）
重放到 upstream 的 33 个新提交之上，必然一路冲突；而 merge 是这个 fork 的既有更新
方式，`git merge-tree --write-tree HEAD origin/main` 返回 exit=0、零冲突文件。

## 评估结论

在临时分支 `chore/reth-upstream-20260726`（已推 n42 远端）合并 upstream 的 33 个
提交，逐项验证：

| 检查项 | 结果 |
|---|---|
| git 合并冲突 | 无 |
| n42 定制点存活 | `prepare_n42_state_root_job`、`n42_skip_state_root`、`n42_defer_state_root`、`LazyHashedPostState` 全在；`HashedPostState` 路径 11 处保留 |
| 依赖版本 pin | **未变**：revm 41.0.0 / alloy-evm 0.37 / reth-primitives-traits 0.5 / reth 2.4.1 |
| `Cargo.lock` 变化 | 仅新增 `parking_lot`、`libc` 两个传递依赖 |
| `reth-engine-tree` 编译 | 通过（2m43s） |
| n42-26 `cargo check --all-targets` | 通过 |
| n42-26 `clippy --all-targets -D warnings` | 零告警 |
| n42-26 `cargo test --workspace` | 46 套件零失败 |

验证在 `perf/h2-v4-batch-verify` 分支上做，因为它包含全部 39 个 interop 提交与
QMDB state-root 定制——只在 `hardening/gov5-cross-port` 上验会漏掉整条 interop 线。

事前担心的风险没有兑现。这批提交里 trie 相关改动密集（`prune sparse trie nodes by
epoch`、`rebase partial proof roots`、`remove sparse trie memory accounting`、
`parallelize pruning retention set calculation`），而 n42 的 state-root 定制正挂在
这条路径上；实际它们没有触及定制所依赖的 `HashedPostState` 接口，且这 33 个提交
**不是一次 deps upgrade**，只是功能与重构。

### 一次 flaky，非回归

首轮全量测试 `test_emit_retries_non_block_committed_output_when_channel_is_full`
失败一次。单独重跑通过，随后完整重跑 46 套件零失败。该测试验证 mpsc 满时的重试
行为，依赖 tokio 调度时序，与 reth 无关。记录在此以免下次再被同一条误导。

## 未做的验证

**没有跑 E2E**。上表全部是编译与单元/集成测试层面，没有 release 构建后起节点出块、
没有跨客户端对拍。切换基线前必须补这一段——reth 的 trie/engine 改动能编过、单测能过，
不等于实际出块与 state root 一致。

## 处置

基线**未切换**，`../reth` 已切回 `chore/reth-upstream-20260719` @ `c533db8ba`，
n42-26 工作区回到 `hardening/gov5-cross-port`。原因是 P4 正式窗口正在跑，其 pinned
二进制 `c0ce2778` 就构建自当前 reth；窗口计时期间变更构建基线会让"测的是哪个版本"
说不清。

评估成果保留在 `n42/chore/reth-upstream-20260726`，随时可切。

## 后续

reth 升级排在 P6 24 小时替换窗口完成、interop 合入 `main` 之后，作为独立任务：

1. 切 `chore/reth-upstream-20260726` 为基线；
2. 补 E2E 场景 1/3/4 与 5/8/12；
3. 更新 `CLAUDE.md` 的 reth fork 基线一节与 `.github/workflows/*.yml` 的 checkout ref；
4. 全量门禁后合入。

在此之前 **`CLAUDE.md` 记载的唯一正确基线仍是 `chore/reth-upstream-20260719`**，
不要因为本评估通过就提前切换。
