# 7 节点 QMDB 性能测试与提升计划

日期：2026-10-06。结论：本机高负载转账共同确认 **7,372 TPS**，存储合约共同确认 **3,238 TPS**。主要优化候选是导入交易的发送者签名恢复，以及导入长尾；当前小状态下 QMDB 二叉树计算不是第一瓶颈。这些是短时实测结果，不是长期容量认证。

## 测试版本和环境

- N42：主线 `007ffcf7b861b28c91d9583d6cdb5ea9de8e6a88`，加当前未提交的 `QmdbLeafTree` 批量折叠改动；分支 `perf/qmdb-catchup-20261006`。旧 H2 分支没有混入。
- Reth：仓库指定的 `23316e3ff8adca8c3bd5085ff0565fcae019202a`，应用仓库的 `reth-payload-transactions-root.patch`。通过独立 worktree 构建，未修改共享 `../reth`。
- 构建：`cargo build --offline --locked --profile profiling --bin n42-node --bin n42-stress -j 8` 成功。优化构建保留符号；不是此前遗留的二进制。
- 节点 SHA-256：`2aab2eebe48ea072103b926a164e7f575dbbca32d0766646affe04fc9e81bc4a`。
- 压测器 SHA-256：`2aa40d9604309946fad31deb2858ddd27e715d06862b3da979118b45a4975017`。
- 参考实现 `../n42-rs`：`09a1284e4`。本轮没有运行它的同环境 7 节点对照，因此没有给出两客户端的吞吐量差距。
- Apple M1 Max、64 GiB，7 个节点、压测器和 profiler 共用一台 macOS 主机；回环网络。每节点 Tokio/Rayon 等工作池限制为 2，详见启动脚本。不是 7 台服务器的网络和 CPU 隔离结果。
- 新链 4242，5,000 个压测账户，加主账户和存储合约，初始 5,002 个状态条目。pre-Shanghai QMDB/H2 配置；没有用 chain94 历史大状态，也没有采用其 post-Cancun native producer 配置。
- 配置 1,000 ms 出块间隔、480M gas、22,857 tx/块；压缩开启，移动端数据包关闭。真实状态路径为 `Gov5QmdbStateRootStore -> QmdbLeafTree`，日志明确记录 `strategy="gov5-qmdb"`。旧 Twig/JMT 旁路关闭，未使用 trusted-root 替代计算。
- Node 0 由 samply 启动采样，199 Hz；其余 6 个节点直接运行。所有节点轮流产块。没有无 profiler 对照，因此尚未量化 profiler 开销。

## 吞吐量和测试有效性

共同确认 TPS = 测量起止共同高度之间的交易数 / 本机实测墙钟时间。每次采样先检查 7 节点提交 QC，再读取它们相同的共同高度，比较区块 hash、stateRoot、receiptsRoot、transactionsRoot。它不等于发送速率，也不是压测器按区块 timestamp 推导的 TPS。

| 用例 | 配置目标 TPS | 测量秒数 | 共同确认交易数 | 共同确认 TPS | 压测器状态 |
| --- | ---: | ---: | ---: | ---: | --- |
| 连续分档：转账 1 | 1,000 | 62.706 | 119,280 | 1,902 | 最后统计 RPC/HTTP 错误 0；到窗口末主动停止 |
| 连续分档：转账 2 | 5,000 | 64.331 | 345,723 | 5,374 | 出现大量 RPC 拒绝和 nonce 重同步 |
| 连续分档：转账 3 | 10,000 | 63.946 | 373,811 | 5,846 | 出现大量 RPC 拒绝和 nonce 重同步 |
| **新链重跑：转账** | **10,000** | **64.420** | **474,882** | **7,372** | 完整结束：643,500 提交成功、0 RPC 拒绝、500 HTTP 错误 |
| **新链重跑：100% 存储合约** | **2,000** | **64.743** | **209,632** | **3,238** | 60 秒末统计：206,500 提交成功、0 RPC/HTTP 错误、0 nonce 重同步；窗口末主动停止 |

![吞吐量与导入长尾](../.artifacts/performance-20261006-141644/throughput-and-tail.svg)

必须保留以下边界：

1. 压测器限速把 `target_tps / actual_batch_size` 向上取整，第一档实际约 1,955 TPS，存储重跑约 3,442 TPS。目标值不代表实际流量。60 秒是签名生产时长，缓冲批次和在途请求还会延长发送。
2. 连续分档中的 RPC 拒绝原因没有被压测器逐条保存；不能将它们全部解释为节点容量不足或明确归因于 nonce。新链独立重跑没有 RPC 拒绝，结果更适合作为后续 A/B 起点。
3. 转账重跑压测器报告约 10,330 提交 TPS，而共同确认只有 7,372 TPS。正常负载期间实际平均块间隔约 2.3 秒，并多次触及 22,857 tx / 480M gas 上限。停止发送后，最终观察到 638,500 笔共同确认；仍有一个节点排队 5,000 笔。HTTP 错误与 nonce 缺口需要逐交易核对，不能宣称已全部排空或可持续 10k TPS。
4. 第一轮存储阶段结束汇总碰到了 QC 已更新但区块尚不可读的窗口，脚本报 `NoneType`。原始日志、采样和 profile 保留；修复起止采样的重试后，存储用例在新链完整重跑。失败轮的存储数据没有作为上述正式存储结果。
5. 第一轮存储抽查 20 笔收据均成功，节点记录了实际 storage slot 写入。合约按调用者更新计数和时间槽，不能把该负载称为每笔写入全新槽。
6. 所有有效采样点的 7 节点承诺一致。该检查不是崩溃恢复、历史大状态或跨客户端一致性认证。没有主线未优化版本的 7 节点 A/B，所以不能用这些数据宣称本轮树优化带来了某个端到端提升倍数。

## 瓶颈证据

以下 p95 来自所有 7 个节点对应测量阶段的日志事件。排除了空块和带 `timing.commit_ms` 的重复 Slow block 最终化日志。导入、execution、pipeline commit 存在包含和重叠，不能相加。`execution_ms` 也不能直接当成纯 EVM 指令耗时。

| 日志范围 | 新链高负载转账 p50 / p95 | 新链存储 p50 / p95 |
| --- | ---: | ---: |
| follower import | 840 / 1,187 ms | 191 / 357 ms |
| import execution | 774 / 1,107 ms | 145 / 316 ms |
| pipeline commit（包含等待） | 1,038 / 1,810 ms | 366 / 617 ms |
| 共识状态 persist | 55 / 110 ms | 61 / 115 ms |
| import state_hash | 16 / 25 ms | 14 / 27 ms |

**发送者恢复是首要 CPU 候选。** 转账独立重跑中，Node 0 高负载阶段累计记录 45.870 秒 CPU delta；`recover_signer` 调用链占 39.29%，N42 payload 转换调用链占 31.61%。它们是包含关系，不能相加。测量时，`tx_iterator_for_payload` 对每笔交易直接 `try_recover()`，构造 EVM 时没有接入 `ctx.sender_recovery_cache()`。后续已共享此缓存，保留 cache miss 的正常验签和 restored-slot 包装；98 项执行层测试通过。优化后七节点 TPS 尚待 A/B 测量，不能把上述火焰图占比直接当作 TPS 增益。

**导入长尾使配置的 1 秒间隔无法维持。** 高负载转账导入 p95 已超过 1 秒，leader packing p95 只有 32 ms；重点应放在 follower 导入的恢复、解码、执行和排队分段。7 节点 `ps` CPU 合计均值约 556%（100% 为一个逻辑 CPU），峰值约 713%，RSS 峰值合计约 6.76 GiB。这不是系统总 CPU 或压测器 CPU，也不足以断言整机 CPU 已完全用尽。进程快照是 macOS `ps` 估计值。

**持久化是次要固定延迟。** `save_consensus_state()` 在共识循环中构造快照、JSON 序列化、同步刷盘、rename、同步父目录；每块都会执行。高负载转账 persist 均值 61.9 ms、p95 110 ms。不能为降低此值直接删除 fsync。QMDB WAL 刷盘在独立指标中，阶段快照差分按 7 节点计数加权：转账平均 9.91 ms，存储平均 11.00 ms。两者不是同一项耗时。

**当前小状态的 QMDB 计算较轻。** 同样按阶段内指标快照差分加权，commit 计算平均转账 0.579 ms、存储 1.276 ms；candidate 计算平均 0.646 / 1.522 ms。转账 commit 指标样本 114 次、candidate 17 次；存储分别 343 / 49 次。快照覆盖阶段内部子区间，不是所有块的逐块计时。`state_hash` 还包含树计算之外的工作，不能用这两个指标相减得出确定开销。大状态下的树更新、WAL 和 checkpoint 仍需专测。

## 火焰图和原始数据

SVG 宽度使用 samply `threadCPUDelta`（µs），不是睡眠线程的墙钟采样次数。用 samply 的预解析符号侧文件还原函数；高负载转账约 2.48%、存储约 3.97% 的 CPU 权重所在栈仍包含未解析帧。`cvwait`、`kevent`、yield 等栈仍可获得前一个采样间隔的 CPU delta，不能把这些等待栈的宽度解释为实际消耗同等 CPU 的等待操作。采样只覆盖 Node 0，不代表每个节点的精确 CPU 分布。

- [高负载转账完整 CPU 火焰图](../.artifacts/performance-20261006-141644/transfer-repeat/profiles/high-transfers.svg)
- [发送者恢复火焰图](../.artifacts/performance-20261006-141644/transfer-repeat/profiles/high-recovery.svg)
- [QMDB 调用链火焰图](../.artifacts/performance-20261006-141644/transfer-repeat/profiles/high-qmdb.svg)
- [存储合约 CPU 火焰图](../.artifacts/performance-20261006-141644/storage-repeat/profiles/storage-2000.svg)
- [区块容量与积压图](../.artifacts/performance-20261006-141644/transfer-backlog.svg)
- [转账分析 JSON](../.artifacts/performance-20261006-141644/transfer-repeat/analysis.json)、[存储分析 JSON](../.artifacts/performance-20261006-141644/storage-repeat/analysis.json)

全部原始证据在 `.artifacts/performance-20261006-141644/`：`provenance.json`、`source.patch`、启动和分析脚本、7 节点日志、Prometheus 快照、共同确认采样、逐块 CSV、进程快照。`transfer-repeat/` 和 `storage-repeat/` 是独立新链重跑，使用同一二进制。每轮 `profiles/node0.json.gz` 与 `node0.json.syms.json` 可由 `samply load` 打开，聚合栈另存为 `*.folded.gz`。

## 提升计划

以下是待实施和待验证的方案；百分比是验收目标，不是已实现收益。

| 顺序 | 最小改动和依据 | 验收条件 |
| --- | --- | --- |
| P0：先修测量 | 压测器用准确 batch/TPS 时间间隔；最后不足整组的账户单独计算 batch；保存 RPC 错误分类和 HTTP 失败交易 hash；失败重试相同已签名 bytes；停止发送后等待在途请求。避免重同步修改仍有预签批次的 nonce。 | 实际发送速率距目标 ≤5%；0 HTTP/RPC 错误；发送成功交易逐笔核对入块；最终各节点 pending/queued 为 0。采样短暂不可读须重试，承诺不一致立即失败。 |
| P1：共享发送者恢复缓存 | 将 builder context 缓存传入 `N42EvmConfig`，在自定义 payload 转换中使用缓存；保留 restored-slot executor 包装。入口只缓存已恢复验证的 sender，cache miss 正常恢复。先记录 hit/miss，再评估 `../n42-rs` 的容量调整，避免同时改多个变量。 | 签名错误仍拒绝；缓存碰撞、淘汰和跨 payload 测试通过；7 节点承诺一致。相同块大小下 follower import p95 至少下降 25%，同负载共同确认 TPS 至少提升 15%，重复 3 次确认。 |
| P2：拆开导入耗时 | 对 decode、sender recovery、tx-env 构造、EVM、state-root、等待 worker 分段计时。缓存 A/B 后，再单独试恢复工作池 2→4；只在剩余 EVM 成为热点时评估并行执行。 | 给出每段 p50/p95 和 CPU profile；2/4 worker 分别在同主机及隔离服务器验证，不能用总 CPU 增大充当收益。导入 p95 目标 <1 秒，同时没有 backlog 持续增长。 |
| P3：共识持久化和 WAL | 测量 JSON、file fsync、rename、directory fsync 各段。评估专用持久化 worker、合并重复快照；只有明确提交的持久化边界完成后，才能推进对应生命周期。WAL 另做 batch/group commit 设计。 | 先做断电/中断点、QC 与执行状态恢复、半写尾恢复测试；安全条件通过后，再以 persist p95 下降 ≥30% 为性能目标。不得关闭 fsync 冒充优化。 |
| P4：历史大状态 QMDB | 导入真实受信快照，覆盖 1M、6M 级状态、随机更新、删除、undo/reorg、跨 twig seal、WAL 增长与 checkpoint。对现有树改动做主线基线 A/B。 | 根和 proof 等价、undo 可恢复；报告 QMDB compute/lock/WAL/checkpoint p95、RSS、磁盘写放大，不能把本次 5,002 初始条目结果外推到历史状态。 |

统一回归矩阵：修复压测器后，转账 1k/5k/8k/10k、存储 1k/2k/4k，每档至少 5 分钟，3 次独立运行；同一 genesis、账户、nonce 起点、gas、worker 配置。做一次不带 profiler 的吞吐量对照，另一次采样。结果必须用共同确认墙钟 TPS、积压趋势和导入长尾判断。最后在 7 台隔离 Linux 主机、真实网络时延及 chain94 当前状态上复测，再决定生产参数。

所有本轮节点已正常退出，3 轮节点进程返回码均为 0；原数据和 build worktree 保留。没有提交或推送本轮改动。
