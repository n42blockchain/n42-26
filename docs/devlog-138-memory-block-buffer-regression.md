# devlog-138：reth 2.4.1 内存块缓冲默认值导致出块吞吐 -32%（已修）

## 现象

devlog-137 的 E2E 发现：合并 upstream 33 个提交后，功能全部正常（多节点块哈希一致、
ERC-20 状态守恒、手机验证通过），但场景 1（400 秒、4 秒 slot、单节点空块）从 101 块
掉到 69 块。劣化是渐进的——前 27 块正常，之后单调恶化到 8~24 秒一块。

## 根因

`crates/engine/primitives/src/config.rs`（upstream `197481f0c`，
"perf(engine): keep 5 in-memory blocks by default (#26462)"）：

```rust
-pub const DEFAULT_MEMORY_BLOCK_BUFFER_TARGET: u64 = 0;
+pub const DEFAULT_MEMORY_BLOCK_BUFFER_TARGET: u64 = 5;
```

引擎因此把 5 个块留在内存里等待持久化。对标准 reth 这是优化；对 N42 不是——N42 的
state root 从 `HashedPostState` overlay 推导，而需要遍历的 overlay 深度跟着这个缓冲
一起长。这也解释了为什么劣化是渐进而非从第 6 块整齐开始：持久化是异步的，缓冲填满
之后 gap 才逐步累积。

## 定位与验证

三点对照，除 reth 与该参数外全部变量固定（同一份 n42-26 代码、同一台机器、同一条
命令、同一个场景）：

| 配置 | 出块 / 400s | 平均间隔 | 最大 | >6s 的间隔 |
|---|---|---|---|---|
| 旧 reth `c533db8ba`（默认 0） | 101 | 3.97s | 5s | 0 |
| 新 reth `018a27282`（默认 5） | 69 | 5.87s | 24s | 大量 |
| 新 reth + 显式 `--engine.memory-block-buffer-target=0` | **101** | **3.97s** | **5s** | **0** |

第三行把参数单独拎出来复现了旧行为，因此归因唯一。最初怀疑是 n42 自己的 twig 上层树
随累积 twig 数变慢（那条路径的特征恰好是"每块时间地板随 version 单调上升"），被第一
行否掉：旧 reth 在同一棵树上跑到 101 块且零慢间隔。

## 修法

`bin/n42-node/src/main.rs` 在 CLI 解析前把默认值固定回 0：

```rust
let engine_defaults = DefaultEngineValues::default()
    .with_state_root_fallback(true)
    .with_memory_block_buffer_target(0);
```

沿用 n42-node 已有的模式——它本来就因为"并行 state root 在这个 workload 上反复回退"
而覆盖了 `state_root_fallback`。`DefaultEngineValues::try_init` 是 reth 给下游设默认值
的正规入口，设置发生在 clap 解析之前，所以 `--engine.memory-block-buffer-target`
仍然可以覆盖它，运维想抬高不受影响。

**没有改 reth。** 这不是 upstream 的 bug，是它的默认值与 N42 的 state-root 定制不
相容；改默认值而非改 reth，升级路径才不会每次都要带补丁。

验证：不传任何环境变量、靠 n42 自己的默认值跑场景 1 —— 101 块 / 3.97s / 最大 5s /
零慢间隔，与旧基线逐项一致。

## 为什么必须修而不是接受

8 秒 slot 下这个回归看不出来：场景 4 实测 7.7s，缓冲带来的额外开销被 slot 余量吸收。
但那是在吃余量而不是没有问题——4 秒 slot 立刻掉到 5.87s。生产目标是 8 秒 slot，把
余量留给真实负载（交易执行、BLS 验签、手机分发），不该先被内存缓冲占掉。

## 顺带

`tests/e2e/src/node_manager.rs` 增加 `E2E_ENGINE_MEMORY_BLOCK_BUFFER_TARGET`，
不设则不传该参数（即用节点自己的默认值）。这是上面第三行对照的手段，也留给以后
需要就该参数做 A/B 时用。

## 状态

分支 `verify/e2e-reth-20260726`。修复后完整六场景重跑通过。reth 升级本身的其余结论
见 devlog-137——该文档的"升级被吞吐回归挡住"一节至此解除。
