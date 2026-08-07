# devlog-126 — Branch-safe QMDB state-root strategy

日期：2026-07-22

分支：`feat/gov5-n42-live-interop`

基线：`main @ f904813`、Reth `2.4.1 @ c533db8`

## 目标与安全边界

把已经验证的 Gov5 replay-v2 QMDB mutation 转换接到 Reth 2.4.1 官方
`StateRootStrategy`，让 Engine Tree 的真实 EVM 输出可以按 Gov5 QMDB commitment 验证。
默认 Ethereum profile、现有七节点 datadir、QMDB 历史和高性能路径均不改变。本阶段仍只允许
observer；不开放 Rust validator 投票。

## Branch-safe candidate store

- 启动时从 portable checkpoint 重建完整 QMDB tree，并再次核对 chain/genesis/block/hash/root；
  root 不一致时不创建 store。
- 每个候选块只保存相对父块的排序 operation。计算候选 root 时从认证 base snapshot 沿该块的
  **精确父分支**重放；兄弟分支互不覆盖，也没有全局 canonical tree 的提前突变。
- 只有算出的 root 等于 header commitment 时才登记候选 delta。root mismatch、缺父块、存储分支
  发散、非法 operation 或超过 4096 层的 ancestry 均 fail closed。
- Reth 仍负责执行、gas、receipt、bloom、requests/BAL 和最终 header commitment 校验；策略只替换
  state-root 计算。当前返回空 Ethereum trie updates，因此这条 bounded 模式不提供
  `eth_getProof`，也不冒充 archive importer。

## 启动门禁与资源上限

只有同时设置 `N42_GOV5_QMDB_EXECUTION=1`、observer mode 和 Gov5 header profile 才会安装该
策略。portable checkpoint 先走流式验证，再在 1,000,000 slots / 512 MiB 默认上限内物化；硬上限
分别为 4,000,000 slots / 2 GiB。87,786,434-slot full archive 仍只走流式验证，不会被整份读入
内存。

节点启动后还会要求本地 Reth best block 的 number/hash 与 QMDB execution base 完全相同。这个
检查刻意拒绝 fresh Reth genesis + block-49 QMDB checkpoint 的错误组合：QMDB root 有了并不代表
Reth PlainState、canonical header 和 execution ancestor 已经水合。

## 验证与当前结论

单测覆盖 root mismatch 不发布、正确重试、兄弟分支隔离、精确父分支 child、缺父块、深度上限和
bounded checkpoint 物化。`cargo check --all-targets`、`cargo clippy --all-targets -- -D warnings`
与 `cargo test --workspace` 是提交门禁。

这一步建立了真实 Engine 执行到 QMDB root 的安全接线，但尚不能直接跟随现有七节点。下一步先从
runtime-02 的 genesis/QMDB base 构造与 Gov5 完全相同的 block 0，并在一次性 datadir 中连续执行
1–49；之后才做 checkpoint-to-PlainState hydration、原七节点 catch-up 和 1,000-block 门禁。
