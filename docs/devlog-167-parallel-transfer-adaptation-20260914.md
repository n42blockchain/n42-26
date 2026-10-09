# rs 并行转账适配：完整状态合并与系统调用顺序

日期：2026-09-14，接续 [P1 快路径](devlog-166-fast-transfer-adaptation-20260914.md)
和 [四节点复用计划](devlog-165-fleet-records-and-reuse-plan-20260914.md) 的 P2。

本轮复用了 rs 的 sender grouping、worker batch、按候选位置收集结果和 bundle graft，
补出完整区块执行适配器，并用 QMDB-only 连续提交与重启验证。**还未接入在线
payload builder / Engine follower；P2 尚未完成，没有新的 fleet TPS 成绩。**
H2 与二叉树 QMDB 读取、根计算和 WAL 规则没有改变。

## 复用范围

固定源是 n42-rs `466c1839791afadba7f1dd4d48c5518440dcafba` 的
`crates/n42/engine-types/src/parallel_transfer.rs`，保存在本轮证据目录。
新增 `n42-execution/src/parallel_transfer.rs`；使用本仓已适配的 N42 factory，
不搬入 rs 的自定义 envelope、Ed25519 交易、seal-first 或 follower import 协议。

worker pool 默认 16 线程，`N42_PARALLEL_BUILD_THREADS` 可调，限制在 1–32
及系统可用并行度之内。每个 batch 使用自己的状态视图；候选结果写入独立槽位，
按输入索引取回，避免收集后再全量排序。被拒绝的发送方后续候选保持为 skipped，
不继续消耗其 nonce。数据库打开/读取失败向上报错。

`parallel_block::try_execute_transfer_block` 返回完整 `BlockExecutionOutput`：

1. 用原来的 tracking Ethereum block executor 执行系统前置调用。
2. 将前置调用后的 cache 作为每个 worker 的只读基线叠加在同一个不可变 parent DB 上。
   worker 的输出 originals 因而是执行交易前的状态，不混入主状态的 revert 历史。
3. 并行执行合格转账，按照区块顺序核对累计容量和每笔声明 gas limit。
4. 将 delta/graft 合并回主状态、合计 beneficiary 费用；保持标准收据构造与 gas 累计。
   tracking executor 记录每个已执行交易的 hash，QMDB restored-slot key 保持相同顺序。
5. 由原执行器执行后置系统调用及提款，随后 merge、补齐唯一且有序的回滚记录。

不合格交易或区块 gas 容量不足返回 `None`，要求调用者在新状态上串行执行整个区块。
发生 graft 错误时返回错误并丢弃局部候选，不能在部分修改后的状态上重试。
调用者必须保证主 DB 与 worker DB 指向同一个精确 parent；本 API 不验证签名、区块头
或 H2 QC，也不会发布执行缓存或提交状态。当前只有测试调用，没有运行开关能把它接入在线路径。

## 在移植算法中复现并修复的问题

以下是固定源码算法在本仓适配测试中的复现，不能直接断言是上游线上事故。

- **未来入账错误资助早先支出**：`A→B 1; B→C 100; A→B 99`，B 初始余额为零。
  只按 sender 分组会先执行 A 的两笔，再接受本应失败的中间一笔。
  现在把 recipient 同时也是 sender 的组连接起来，组件内保留候选顺序；纯收款方共享
  不会把独立发送方全部串行化。该用例执行索引为 `[0, 2]`、跳过 `[1]`。
- **缓存收款方覆盖入账**：C 初始 10，已在主状态 cache；两批分别增加 5、7。
  旧 slow path 各自从 10 计算后覆盖，得到 17。现在先读取待提交的累计增量，得到 22。
- **普通 graft 丢弃 contracts 元数据**：创建 EOA 时 REVM bundle 可能包含空 bytecode。
  原普通 graft 没有保留该 contracts map；现在和最大 bundle 交换分支一样保留。
  测试比较完整 bundle，而非只比余额和 nonce。
- **回滚大小漏算 storage 项**：原 `append_reverts` 把 `reverts_size` 设为地址条数。
  系统调用带有 storage revert 时不符合 REVM 的 `AccountRevert::size_hint()`。
  现在按真实 hint 统计，包含四个系统合约的完整输出与串行执行一致。

另外将 delta 的余额/nonce 运算从饱和运算改为 checked arithmetic，溢出即报错。
新 EOA 的 slow/fold 提交保留 loaded-not-existing 标志；不错误标成合约创建。
保留 rs 针对同一账户重复触碰、提款、reward 的 earliest revert 去重方式，避免重复
changeset 地址重现其历史记录中的 `UnsortedInput` 问题。

## 验证

使用上一轮固定补丁的干净 reth 副本，`--locked --offline`；只增加已在 lock 中的 rayon
作为 n42-execution 直接依赖，没有升级依赖版本。没有修改相邻 dirty reth 工作区。

- `n42-execution --lib`：125 passed，新增 7 个分组/graft 用例和 3 个完整区块用例。
- 5,000 笔、两个 sender、共同收款方，实际分成多个 worker batch；逐笔结果和完整
  BundleState 与关闭快路径的解释器一致。小规模矩阵涵盖缓存/未缓存、重复收款、
  新账户和合并后 reward。回滚仅规范化地址顺序，其他字段完整比较。
- 部署实际 EIP-4788 / 2935 / 7002 / 7251 bytecode 的 Prague fixture，比较标准收据、
  gas、requests、完整 bundle/code/storage/reverts，以及 C 和新账户的提款入账。
- 独立的受控 fixture 用前置系统调用给初始空余额 A 入账，用后置系统调用读取 C 余额
  并写入 storage，验证 pre→parallel→post 顺序。该 fixture 有意替换两处测试合约代码。
  执行测试使用给定 recovered senders，不声称覆盖签名验证。
- QMDB-only：8,200 账户、每块 4,096 笔、连续三块；串行与并行各执行 12,288 笔。
  所有成功收据、累计 gas、余额、nonce、手续费及三块 durable roots 与独立算术 oracle
  一致；各自关闭并重开 WAL 后验证账户。fallback provider 为空，没有静默读回 Reth。
- Release `n42-node --lib canonical_bench`：原 Cancun、Prague fast-transfer 和新增
  parallel-transfer 三个正确性测试；显式性能实验保持 ignored。
- Workspace all-targets Clippy `-D warnings`；日志见本轮证据目录。

[证据目录](benchmarks/20260914-parallel-transfer/) 包含原始失败复现、通过日志、
固定上游文件及改动文件快照。debug/QMDB 日志的时间只是正确性运行的附带输出，
未控制共享机器负载，也没有真实网络/H2 finality，不能用于 TPS 或性能提升结论。

## 下一项：接入真实出块循环

仍复用 `N42InnerPayloadBuilder`、现有 pool iterator 和 Reth 组块/缓存发布语义。
需要在主状态 pre-execution 之后插入 batch 阶段，保留 skipped candidates 的 sender
依赖和串行后续执行；补齐交易数、声明 gas limit、Osaka RLP 上限、取消及 build budget。
`TrackingExecutor::commit_preexecuted_transfer` 已提供本 crate 内部的已执行转账记账入口，
不能作为信任外部执行输出的接口。

Reth `BasicBlockBuilder::finish` 本身不发布 payload execution cache；发布发生在
`default_ethereum_payload` 的 finish 之后。接入点应在该处取走 bundle 前补齐 graft
reverts，不能等到缓存发布后再修改。现有增量 state-root hook 只观察逐笔 commit，
直接 graft 必须向其提供完整变更或转为完整同步根计算，不能沿用缺失 batch 变更的根。
N42 的 `with_jit_support()` 当前沿用返回 Self 的默认实现，无需凭猜测更换 factory。

目前的完整区块适配器已验证系统顺序和 QMDB 读取约束，可复用到后续 follower 接入；
不能把独立 API 的通过测试描述为已在线运行。随后继续四人原生创世、现有 fleet launcher
和连续 60 秒窗口验收。原七人快照的签名 roster 不能通过截短变成四人委员会。
