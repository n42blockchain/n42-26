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

## E2E：功能全过，但出块吞吐回归 32%

补跑了 CI 覆盖的六个场景（`e2e.yml` 的 1/3/4 与 5/8/12），验证分支
`verify/e2e-reth-20260726` = 本评估分支 + T7 批量验签 + Windows 原生支持
（cherry-pick `7c5f6d6`）。

| 场景 | 结果 |
|---|---|
| 1 单节点连续出块 | **FAILED** — 69 块，低于 70 下限 |
| 3 ERC-20 | PASSED — 300 笔转账全成功，余额与总供应守恒 |
| 4 多节点共识 | PASSED — 1/3/5 节点全部 5 项验证通过 |
| 5 手机验证 | PASSED — 245 次 attestation 零错误 |
| 8 手机 EVM | PASSED — QUIC 验证 2 块（含带交易块） |
| 12 Blockscout RPC | PASSED — 17 项零警告 |

**功能正确性没有回归**，这是本次评估最想确认的一点：多节点在 5 个采样高度上块哈希
完全一致、ERC-20 状态守恒、手机侧 Merkle 验证通过。新 reth 的 trie 重构没有破坏
n42 的 state-root 定制。

**但出块吞吐明显回归。** 场景 1 是 400 秒、4 秒 slot 的单节点空块生产：

| | 出块数 | 平均间隔 | 最大间隔 | >6s 的间隔 |
|---|---|---|---|---|
| 旧 reth `c533db8ba` | **101** | 3.97s | 5s | **0 个** |
| 新 reth `018a27282` | **69** | 5.87s | 24s | 从第 28 块起大量 |

对照实验控制了除 reth 以外的全部变量：同一份 n42-26 代码、同一台机器、同一个场景、
同一条命令，只重建了 `n42-node`。旧 reth 平均 3.97s 精确贴合 4s 配置且最大仅 5s；
新 reth 平均 5.87s、最大 24s，且劣化是**渐进的**——前 27 块正常，之后单调恶化。

一开始怀疑是 n42 自身的 twig 上层树随累积 twig 数变慢（那条路径的特征正是"每块时间
的地板随 version 单调上升"），对照结果排除了这个猜测：旧 reth 同样跑到 101 块且零
劣化，问题在 reth 侧。

upstream 这批里值得先看的嫌疑：

- `197481f0c perf(engine): keep 5 in-memory blocks by default (#26462)`——内存中保留
  的块数变少，state root 计算可能更多回落到磁盘；
- `84f4c989c feat(trie): prune sparse trie nodes by epoch (#26485)`；
- `f708b8be1 perf(trie): parallelize pruning retention set calculation, remove LFU (#26439)`；
- `a04b780d6 perf(trie): remove sparse trie memory accounting (#26458)`。

劣化从第 28 块才开始，不像"每块固定开销变大"，更像某个按累积量触发的机制（epoch
剪枝、内存块窗口滚出）到达阈值后才生效。

### 附带发现：5 节点启动超时不是功能问题

首轮场景 4 的 5 节点子测试报 "node did not become ready within 60 seconds"。节点日志
显示它并没有崩溃：reth 每次启动都要 heal static files 并校验存储一致性，这段 IO 在
5 节点并发时耗了 27 秒，总启动 61 秒——刚好压过硬编码的 60 秒预算。把超时放宽后
5 节点完全通过（`E2E_NODE_READY_TIMEOUT_SECS`，默认仍是 60 秒，CI 行为不变）。

### 这次 E2E 的效力边界

跑在 Windows 原生环境，而所需的两处支持（32 MiB 栈、跨平台 e2e 路径）至今没进
`main`，也就是说这条路径平时没人走。结论对"功能是否回归"有效；吞吐数字的绝对值
不能直接套到 Linux CI 上——但**同机对照的相对差异**（101 → 69）是可信的。

## 处置

基线**未切换**，`../reth` 已切回 `chore/reth-upstream-20260719` @ `c533db8ba`，
n42-26 工作区回到 `hardening/gov5-cross-port`。原因是 P4 正式窗口正在跑，其 pinned
二进制 `c0ce2778` 就构建自当前 reth；窗口计时期间变更构建基线会让"测的是哪个版本"
说不清。

评估成果保留在 `n42/chore/reth-upstream-20260726`，随时可切。

## 后续

**升级被这个吞吐回归挡住了，不是时机问题。** 原本的结论是"编译与测试全绿，等 P6
完成就可以切"；E2E 之后要改成"先定位 -32% 的出块回归，再谈切换"。8 秒 slot 下
（场景 4 实测 7.7s）还看不出问题，4 秒 slot 就掉到 5.87s——生产目标是 8 秒 slot，
当前余量掩盖了它，但这是在把余量吃掉。

顺序：

1. 定位回归提交：在 `chore/reth-upstream-20260726` 上对嫌疑提交做二分，每次跑场景 1
   取出块数（101 vs 69 的区分度足够，单次即可判定）；
2. 判断是配置可调（如 in-memory block 窗口）还是需要适配 n42 的 state-root 定制；
3. 修复或规避后重跑六场景，确认场景 1 回到 100 块量级；
4. 再走原计划：切基线 → 更新 `CLAUDE.md` 基线一节与 `.github/workflows/*.yml` 的
   checkout ref → 全量门禁 → 合入。

在此之前 **`CLAUDE.md` 记载的唯一正确基线仍是 `chore/reth-upstream-20260719`**。
`../reth` 已切回该分支，`n42-node` 也已在其上重建（对照实验的产物），当前工作区
就是正确基线。
