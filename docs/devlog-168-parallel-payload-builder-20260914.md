# 并行转账接入在线 payload builder

日期：2026-09-14，接续 [P2 执行适配](devlog-167-parallel-transfer-adaptation-20260914.md)。
本轮把 rs 的并行转账/graft 接入实际 `N42InnerPayloadBuilder::try_build`，
使用 `N42_PARALLEL_BUILD=1` 启用，默认关闭。没有新的 fleet TPS 成绩，四节点 1M 目标仍未达到。

## 在线路径

新增 `crates/n42-node/src/parallel_payload.rs`，复用固定 Reth payload builder 的
组块、串行执行、交易 gas/blob/RLP 检查、费用计算和执行缓存发布流程。
关闭时直接调用原 `default_ethereum_payload`。节点的实际 EVM 类型仍为 `N42EvmConfig`；
payload builder 的 trait 实现现在明确使用该类型，保留本仓 precompile 与 tracking executor。
没有更改签名恢复/验证、H2、原生头编码、QMDB 二叉树读取/根/WAL 协议。

Prague/Osaka 构建在原系统前置调用后，从原 best-transactions iterator 收集一个候选前缀：

- 只预选 legacy/2930/1559、无输入和 access list、不同 sender/recipient、未涉及 beneficiary
  的候选；实际账户 code、余额、nonce、费用、预编译和环境资格仍由 worker 的转账实现检查。
- 按候选声明 gas limit 预留容量，按原 RLP 规则累计 Osaka 长度，遵守交易数、取消和时间预算。
  合格转账实际消耗的多余预留容量可由之后的串行循环利用。
- 每个 worker 从相同 parent hash 打开独立 provider，并叠加系统前置调用后的 cache。
  仍经过既有 QMDB provider 入口，未新增从其他 parent/latest 或静默 fallback 读取的分支。
  打开/读取失败会使本次构建失败。
- 执行成功的转账按候选索引形成前缀，批量合并账户状态、合计 beneficiary 费用，
  通过原 receipt builder 与 gas 记账记录结果。`commit_transfer_batch` 与上一轮完整区块
  适配器共享，不通过外部提供的执行结果绕过验证。
- 被拒绝候选按原顺序放入 `ReplayTransactions`，随后继续原迭代器；串行验证判定无效时
  同时移除缓存中的 sender 后续 nonce。blob 排除也处理已缓存的依赖交易。

结构检查无法判断 recipient 是否为合约，因此被 worker 拒绝的空输入调用可能移到串行尾部。
最终交易顺序可以与原 serial builder 不同；要求是**对最终实际组出的顺序重新执行时，
完整状态、收据、gas 和 commitments 一致**。测试覆盖这种重排和后续入账资助串行重试；
不会让较晚入账提前资助一笔仍排在前面的交易。

## 状态根与发布边界

直接 graft 不会把完整 batch changes 逐笔发送给原增量 state-root hook。本版并行构建
丢弃该任务的 hook/receiver，使用 `BasicBlockBuilder::finish` 的完整同步状态根计算。
这是明确的首版吞吐代价，不能用缺少 batch 变更的增量根换取速度；后续再接完整批量通知。
原有显式 benchmark skip/defer 选项没有扩大，正式资格验证仍要求它们关闭。

finish 保留后置系统调用与提款。其后在 `db.take_bundle()` 与执行缓存发布之间补齐
回滚记录；最终 Osaka 块长检查和取消检查也位于发布之前。缓存包含完整的 bundle、
标准收据及原顺序 senders，不是在缓存发布后修改其中一份副本。

## 实际组块暴露的新问题

新增的“先给 CREATE 目标地址转账，再由串行尾部在该地址执行合约初始化并 SSTORE”
用例复现了原 `append_reverts` 的遗漏：保留最早账户 revert 时，会整条删除之后的
storage revert。余额与区块头仍能一致，但已发布 bundle 少了槽的回滚记录。

已修复为保留最早账户 original/status，同时合入后来首次修改的 storage revert，并保留
wipe-storage 要求。失败日志与修复后对照都保存下来。这是在适配代码中的实际复现，
未据此断言 rs 线上曾发生同样事故。

## 启动与观测

通用 `testnet.sh` 显式传递 `N42_PARALLEL_BUILD`，默认 0。
原七人 chain94 launcher 新增 `--parallel-build`，在清理继承 N42 环境之后设置开关，
并记录到 `run.json`；资格工具的配置摘要也记录该选项。没有实际启动/修改旧舰队。

`N42_PARALLEL_BUILD` 控制 builder 的并行转账前缀；`N42_FAST_TRANSFER` 独立控制普通
EVM factory 的逐笔快路径，部署时可同时开启，后者也作用于 follower。并行线程数继续
使用 `N42_PARALLEL_BUILD_THREADS`。四人委员会仍需正确的原生创世/roster，不能截短七人快照。

指标 `n42_parallel_payload_executed_total`、`n42_parallel_payload_batch_ms` 记录已执行
前缀和耗时，日志包含 accepted、skipped 与 batches；外层 path 标签为
`live_parallel_transfer_builder`。这些指标包括可能最终取消的构建，**不代表 H2 提交 TPS**。

## 验证与限制

- 调用在线 builder 实际使用的构建函数，分别启用和关闭并行；不是只测试独立执行 kernel。
  4,200 笔、两个 sender、共享收款方的前缀形成多个 batch。测试用局部 recorder 确认
  并行执行计数为正，防止全程回退后误判通过。
- 对返回的实际交易顺序使用关闭快路径的 `BasicBlockExecutor` 重新执行，比较发布缓存的
  完整 `BundleState` 和 execution result。对保持相同顺序的场景还比较完整 block，
  包括交易根、收据根、gas、请求与 sender 列表。
- 覆盖普通转账、合约读取合并后余额、同 sender 串行后续交易、先转账后 CREATE/SSTORE、
  42,000 gas 容量、nonce gap 及其后续交易过滤、空输入合约被拒绝后的重排、
  先跳过余额不足交易后在串行尾部重试，以及 Osaka 超长 RLP 交易及其 sender 后续过滤。
- 本组 builder fixture 使用真实 PoolTransaction/标准执行器和 Mock provider；签名与
  recovered sender 是受控输入，未测试真实签名入口。Mock state root 是哨兵，验证完整
  同步根调用确实发生，不能把这个哨兵说成计算出的二叉树根。实际 QMDB-only 连续三块和
  WAL 重启对照沿用上一轮测试并在本轮 release 复跑。
- `n42-execution --lib`、workspace all-targets Clippy `-D warnings`、启动器六项测试，
  以及 shell/diff 检查。Release builder 矩阵与 canonical QMDB 用例日志见证据目录。
- 使用同一固定补丁的干净 Reth 副本，`--locked --offline`；只增加已有锁定依赖的直接边，
  没有升级版本，也未修改相邻 dirty Reth。本轮未运行性能矩阵。

证据与本轮文件快照见 [验证目录](benchmarks/20260914-parallel-builder/)。
下一步验证在线 builder 经生产 QMDB hook 的完整路径、接入 follower 并行调度，再推进
原生四节点连续窗口验收。当前网络 sandbox 不能创建 TCP/UDP socket，因此本轮证据
没有真实网络、H2 finality 或四节点吞吐，不能宣称已经达到 1M TPS。

补充：检查 `n42-chainspec::n42_dev_chainspec` 确认默认开发链只激活到 Cancun。
当前并行前缀与转账快路径的资格仍是 Prague/Osaka，因此仅在默认开发链上设置开关
不会产生并行命中。下一步先对 Cancun 做完整执行语义对拍，验证后再扩展资格，
不以修改既有链的 fork 配置来掩盖这个问题。
