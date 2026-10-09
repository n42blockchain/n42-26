# 默认 Cancun 链快路径与生产 QMDB provider 验证

日期：2026-09-14，接续 [在线 builder 接入](devlog-168-parallel-payload-builder-20260914.md)。
默认 `n42_dev_chainspec` 只激活到 Cancun。本轮补齐同 fork 的执行资格与结果语义，
没有改变默认或既有链的 fork 配置。`N42_FAST_TRANSFER=1` 与 `N42_PARALLEL_BUILD=1`
现在可用于默认开发链；Shanghai 及更早 fork、Amsterdam 继续走原解释器。

## Cancun 的执行结果差异

仅把资格从 Prague 放宽到 Cancun，会使完整 `ExecutionResult` 对拍失败：
两者普通转账均使用 21,000 gas，但 Cancun 的 `floor_gas` 是 0，Prague/Osaka 的无输入
EIP-7623 floor 是 21,000。失败日志保留在证据目录。

`fast_transfer.rs` 现在按实际 spec 构造这一字段，其余 nonce、余额、费用、gas 上限、
code/预编译、环境和 inspector 资格规则保持。在线 builder 的前缀门槛同步扩到 Cancun，
并保留 Amsterdam 回退；不能只扩大 EVM 内部门槛而让 builder 仍完全串行。

扩展原有 legacy/2930/1559 × 新/旧收款方 × 转账额的完整结果及 BundleState 对照，
覆盖 Cancun、Prague、Osaka。`n42-execution --lib` 125 项通过；不合格环境测试改用
Shanghai 和 Amsterdam 验证回退。没有把小样本耗时当作性能结论。

## 在线 builder 经生产 provider hook

新增独立进程测试 `crates/n42-node/tests/parallel_qmdb_builder.rs`，使用真实
`BlockchainProvider`、MDBX 测试数据库、已有 QMDB-only 注册 hook 和实际
`N42InnerPayloadBuilder::try_build`，而非独立执行 kernel。

- Reth 账户表没有发送方资金；QMDB 中存两个发送方、共享收款方、beneficiary 和一个
  不参与转账的账户。先断言注册前 Reth provider 无法读到发送方，再注册精确 parent 视图。
- 为两个发送方各生成 2,048 笔真实 secp256k1 签名交易，每笔恢复并核对 sender；
  进入真实内存 pool。测试 admission validator 提供固定余额/nonce 元数据，
  因此这不是完整 txpool 验证/网络入口测试；执行余额仍只能来自 QMDB。
- builder 使用显式 `with_parallel_transfers(true)`，EVM factory 的逐笔快路径关闭。
  局部指标断言并行前缀实际执行全部 4,096 笔，避免全程串行回退后误判通过。
- 发布的完整 bundle、收据、gas 与同一 provider 上关闭快路径的串行整块重执行一致；
  读取计数增加，read/provider/mismatch/unavailable 错误计数均为零。
- 通过原 `RethExecutionOutputCache::take_gov5_normalization` 消费 builder 的 broadcast
  输出，取得实际二叉树 QMDB 候选根与原生收据根。候选根与独立余额/nonce/费用算术 oracle
  一致；oracle 保留未触碰账户，并按协议的键顺序应用操作。QMDB 冻结叶位置属于根，
  不能把任意顺序的账户写入列表或重新排序的全量状态当作同一根。
- 明确断言父根未改变、新候选 hash 尚未 committed；测试不会把 prepare 当成 H2 提交。

为复用实际 builder，增加显式构造与并行选择方法；节点装配仍读取原环境开关，
已有 EOF guard、交易池、系统调用、串行尾部和缓存发布逻辑不变。

此测试验证生产读取与原生候选根计算边界，没有发出 H2 投票、验证 QC 或推进 canonical head。
临时 Ethereum builder 的 MPT 根不是最终原生 QMDB 根；本测试通过现有 normalization
桥验证后者，不能把两个根混为一谈。

## 连续提交与检查

新增默认 Cancun 8,200 账户、每块 4,096 笔、连续三块：串行、逐笔快路径、并行区块
三组各执行 12,288 笔。逐笔快路径命中计数至少 12,288；三组根、成功收据、费用与独立
算术 oracle 一致，各自关闭重开 WAL 后验证全部相关账户。已有 Cancun/Prague 用例保留。

- Release canonical QMDB：4 passed，显式性能实验 1 ignored。
- Release 在线 builder 矩阵加入默认 Cancun 混合尾部，连同原 Prague/Osaka 场景通过。
- 独立生产 hook / builder 测试通过，完整对照及错误计数见日志。
- Workspace all-targets Clippy `-D warnings`、格式和 diff 检查；构建使用上一轮同一干净
  固定补丁 Reth 副本与 `--locked --offline`。没有升级依赖或修改相邻 dirty Reth。

[证据目录](benchmarks/20260914-cancun-builder/) 保存失败复现、通过日志和本轮输入摘要。
上一轮生产二进制编译已完成，日志/哈希归入 devlog168；本轮重新构建含 Cancun 支持的版本。
临时构建副本的内嵌 Git SHA 是其构建元数据，不是本轮修改的提交号；源码与可执行文件身份
以输入快照和 SHA-256 为准，未创建提交。

下一项接入 follower 并行调度，并复用原生四节点启动与验证路径。在线 leader 的并行前缀
已接通，但 follower 目前仍通过原逐笔执行器；H2 和二叉树 QMDB 的验收要求保持不变。
没有真实四节点 TPS 数据，不能声称达到 1M。

后续接入点已核对：Reth `crates/engine/tree/src/tree/payload_validator.rs` 的
`execute_transactions` 自己调用 `executor.execute_transaction`，然后在外层 finish/merge；
仅覆盖 `ConfigureEvm::executor` 不会覆盖这条在线路径。它还逐笔发送 indexed receipts、
更新 prewarm 的 executed-tx counter，并向根任务发送 state hook 更新。
`StateRootUpdateHook` 在 drop 时关闭更新流，不按交易条数关闭；REVM State 暴露其
`state_hook`。后续 batch graft 可发送完整的合并账户更新，但必须验证这些更新覆盖了
所有 changed keys，不能仅停用通知而继续接受原根任务的结果。未在本轮修改该路径。
