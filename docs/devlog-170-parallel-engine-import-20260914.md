# 2026-09-14：接通 follower 的 Engine 批量执行

本轮把 n42-rs 的转账批量执行复用到真正的 Reth Engine 导入循环。生产 QMDB-only
读取、4096 笔真实签名交易、标准收据验证和独立二叉树根的组合已经通过 debug 集成测试。
不是四节点成绩：测试使用 Ethereum 头部和 EthBeaconConsensus，不包含原生 H2 投票/QC，
没有推进 canonical forkchoice；QMDB 根策略实际计算并提交了已验证候选状态。

## 执行入口与约束

- 新增 `patches/reth-import-batch.patch`，固定 Reth baseline 上的四文件小补丁。
  ConfigureEvm 提供默认关闭的 batch import 扩展；Engine 自己的手动事务循环显式调用，
  BasicBlockExecutor 的普通/带 state hook 入口也调用同一扩展。
- 节点组件由 `N42_PARALLEL_IMPORT=1` 开启。config 持有精确父区块哈希的 provider 工厂，
  每个 worker 独立打开父状态并叠加 pre-execution cache；继续经过生产 QMDB adapter。
  单笔 `N42_FAST_TRANSFER` 和 leader 的 `N42_PARALLEL_BUILD` 独立控制。
- 仅 Cancun、Prague、Osaka 完整合格转账块。原签名恢复 iterator 仍先完成；不跳过任何
  sealed transaction，不重排，检查累计容量所要求的 declared gas limit。
  worker 拒绝某笔时不合并候选，按原顺序串行；数据库失败或合并错误使候选失败。
- 系统前置调用只执行一次。批量收据逐索引发送给原 receipt-root task，更新 prewarm
  进度；finish 后执行系统后置调用/withdrawals，merge 后补齐本块回滚记录。
  BAL/Amsterdam 保留原入口，多块 executor 已持有旧 bundle 时保守走串行。

## 修复状态根更新遗漏

直接 graft 以前绕过 State::commit，安装的 state hook 收不到直接插入账户。
现在有 hook 才分配观察集合，在批量合并后发送每个 changed EOA 的最终值；共享收款人的
多个余额增量已经合并，缓存慢路径不再重复通知，手续费和后续系统调用仍正常 commit。
成功和错误返回都恢复 hook，避免中途 drop 错误关闭根更新流。根消费者可以接收一个完整
状态批次，不要求每笔交易一个更新。这里不关闭或绕过 follower 的状态根验证。

保留 `keep_cache=true`，后置系统调用可能读取转账后的账户。leader 仍保留当前同步根计算，
本轮未据此重启其异步根任务。

## 验证

`parallel_qmdb_builder.rs` 扩展已有生产 hook 集成测试，独立进程注册 QMDB-only adapter：

1. 真实签名/恢复、真实交易池和在线 builder 产生 4096 笔；pool admission 仍为
   MockTransactionValidator，不能声称已测试完整生产入池验签。
2. 普通 BasicBlockExecutor 导入同块，断言批量命中 4096，完整 bundle、reverts、gas、
   receipts 与关闭单笔快路径的串行执行相同；state hook 覆盖全部变化账户。
3. 用独立余额/nonce/费用 oracle 算出 QMDB 根并替换临时 builder 根，形成不同哈希，排除
   leader execution cache 命中。调用真正 BasicEngineValidator，批量命中恰好 4096，
   返回的完整执行输出与串行相同，QMDB store 发布匹配的根。
4. 错误根区块被拒绝且不发布；复制首笔签名交易到尾部并重新计算 tx root，重复 nonce
   触发串行回退后拒绝，不能略掉交易接受残余批次。canonical head 始终为原父块。

执行层 126 项通过，含真实前置合约供资、后置合约读取 grafted 账户、withdrawals，
以及拒绝批次后 executor cache/receipts/bundle 未变的断言。多批次共享收款人、预缓存账户、
新 EOA 和奖励的串行/并行对照同时核验 state-hook 最终账户值。
Fleet 启动器新增 `--parallel-import`，写入 run.json 和每个子进程显式环境；四种选项组合
验证不会继承旧实验开关。六项脚本测试通过。

构建/检查进度与来源快照以 [证据 manifest](benchmarks/20260914-parallel-import/manifest.json)
为准；本轮没有新 TPS 测量。`--locked --offline`，不升级依赖；只增加已锁定 storage-api
的直接依赖。相邻工作目录的 dirty Reth 没有修改。

## 下一步

现在 leader 与 follower 的执行调度都已接通，下一项回到原生四节点 H2 启动与带交易验证。
旧 chain94 的七人签名 roster 不能直接截成四人，需生成匹配的四节点原生 genesis/keys，
沿用既有持久化 QMDB 和验收工具。最终仍以四节点成功收据、H2 committed 区块和 QMDB-only
读取证据衡量 1M TPS；此次单进程正确性测试不能代替该目标。
