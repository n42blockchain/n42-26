# 七节点历史成绩与四节点复用计划

日期：2026-09-14。目标仍是 **4 节点、H2、二叉树 QMDB 状态读取、成功提交 1M TPS**。
本轮按用户补充要求，把主线从局部存储测量拉回已有整链路径。

后续进度：P1 的 fast-transfer factory 适配及 QMDB 连续区块对拍已落地，见
[devlog166](devlog-166-fast-transfer-adaptation-20260914.md)；下面保留立项时的判断与顺序。

## 以前到底跑到多少

| 项目与口径 | 已记录成绩 | 不能省略的条件 |
|---|---:|---|
| N42-26 round53，2026-09-01 | **170,546.56 TPS**；10,232,976 笔 / 60.001068 秒 | 七节点同机 Linux；163k tx/block；受控预签名交易，`N42_SKIP_TX_VERIFY=1`、deferred state-root 压测选项开启；是原报告的窗口内 committed 口径，未按现在的成功收据/QC/only-reader 门槛重新验收 |
| N42-26 round52 | 156,499.78 TPS；9,390,144 笔 / 60.001004 秒 | round53 的可比前纪录 |
| N42-26 batch fastlane | 平均约 **3.27M**，另一轮峰值 **13.33M TPS** | 共识/网络上界实验；省略 EVM/reth 执行、收据、状态根和持久化；不能算目标达成 |
| n42-rs 旧负载 round42 D5 | 首窗口 **423,775 TPS** | 245k tx/block；收款人生成器后来查出索引错误，旧轮次大幅重复收款人，不能与修复后的负载比较；后两窗 334,133 / 228,067，未采用大块方案 |
| n42-rs 旧负载 loop53 Q300a | 首窗口 **365,399 TPS** | 163k tx/block、secp256k1；同样早于收款人修复 |
| n42-rs 修复后负载 loop141 P350a | 首个 30 秒窗口 **333,416 TPS** | 163k tx/block，约 147k 个账户；Ed25519、parallel build、follower graft、deferred execution / seal-first；不是本仓测量 |
| n42-rs 修复后最佳三窗口 loop141 P350b | **22,518,476 笔**；三窗 TPS 328,018 / 220,073 / 202,273 | 三个约 30 秒窗口；按名义 90 秒折算约 250.2k TPS，不把这个折算冒充原始精确墙钟成绩 |
| Gov5 Windows 旧基线 | 首窗口 **22,487 TPS** | v5.7.955，98.4% occupancy，55/60 满块；硬件、块容量和 Linux 不同 |
| Gov5 最新 Linux 完整 B 组 | 均值 **90.6k TPS** | 35zzo / r74；整个 A/B 试验中 A2 受外部编译干扰丢失 |
| Gov5 Linux 35zzp | 首窗口 **125.9k TPS** | 暂停前单窗口；不是完整多窗口纪录；deferred fleet 35zzq 尚未跑 |

原始报告：[round53](devlog-141-one-minute-rerun-20260901.md)、
[round52](devlog-139-zstd-quic-direct-push-audit-20260824.md)、
[batch fastlane](devlog-83-batch-transfer-profile-optimize.md)、
[原生七节点](devlog-144-independent-chain94-fleet-20260904.md)。
round53 报告提交为 `ba402c477ac86810656af534e21da356579ec2ba`；这不是已核实的运行二进制 SHA。
报告列出的 `/data/n42-bench-artifacts-20260901/round53-main-baseline-60s` 在当前环境未取得，
因此这些数值是核对版本化报告后的历史成绩，未声称本轮重算了其逐块原始日志。

上游固定摘录及对象摘要见 [证据目录](benchmarks/20260914-fleet-reuse/)。
特别是 rs 的 423,775、365,399 和 333,416 都有出处；下降有负载修正这个实质原因，
不能简单说客户端退步，也不能只保留最高数字。

## 为什么看起来从头做

之前推进过度集中在 QMDB 局部基准、读取证据和资格工具，未及时把新增能力接回旧整链配置。
这些实现仍可复用，但不能替代执行/组块关键路径，优先级需要纠正。

还有两个真实的路径差异：

1. 旧 round53 是受控 Ethereum 转账压测。原生 chain-94 七节点已经实现 QMDB 根、
   H2 提交、完整启动及重启，但 9 月 4 日那轮是空交易验证，不能把 round53 的 TPS
   直接标到原生 only-reader 路径上。已有节点代码没有被推倒重写。
2. `payload.rs` 的 `N42_EXECUTION_LANES=8` 是发送方分组/排序准备线程，
   `try_build` 仍调 `default_ethereum_payload`，标为 `LIVE_SEQUENTIAL`。
   rs 的 `fast_transfer.rs` 和 `parallel_transfer.rs` 才是实际转账执行优化。
   原来的 parallel-EVM 路线图已指出这个缺口，现在应从这些已有实现适配。

## 代码复用清单

| 保留/接入项 | 本仓位置 | 上游依据与处理 |
|---|---|---|
| 发送方分组、nonce 顺序和确定性 merge | `crates/n42-node/src/payload.rs` | round53 的 8-lane 实现保留；round54/55 顺序扫描候选更慢，不重做 |
| binary-v1、zstd level 1、QUIC direct、共享 envelope | `crates/n42-network/src/`、`scripts/testnet.sh` | round38–53 已测；四节点 fanout 为 3；大块不得直接关压缩，保留身份验证与背压 |
| 原生 QMDB 执行与 H2-v4 接线 | `bin/n42-node/src/main.rs`、`crates/n42-consensus-service/src/orchestrator/execution_bridge.rs` | 复用 devlog144 的真实原生路径；不另造链实现 |
| exact-parent 读取、批量叶折叠、候选根与 WAL | `qmdb_state_reader.rs`、`qmdb_state_root.rs`、`qmdb_leaf_tree.rs` | devlog146–164 已完成的能力保留，后续看整块收益 |
| 第一项执行适配：plain-transfer fast path | `crates/n42-execution/src/evm_factory.rs`、`evm_config.rs` | rs `crates/n42/engine-types/src/fast_transfer.rs`；先适配现有 EVM factory，不绕开 provider、block executor、收据及系统调用 |
| 第二项执行适配：sender groups + bundle graft | `payload.rs` 与 `crates/n42-execution/` | rs `parallel_transfer.rs`；用专用有限线程池，保留候选顺序、余额增量与 revert 合并 |
| 减少重复组块与执行输出复制 | `crates/n42-node/src/exec_cache.rs`、`execution_bridge.rs` | rs `f9c6fcea4`/`28a94b94b`；复用必须绑定完整块身份及实际交易，禁止只凭 parent 或 header-only 命中 |
| 持久化与第二窗口退化 | QMDB/reth writer、缓存消费者水位 | Gov5 `f0603e82`、`15710ab9`；移出锁内准备，按交易数/字节限额，不删 fsync 或 latest-state 写入 |

## 执行计划与完成标准

**P0：恢复事实基线，已完成本轮代码定位。** 修正 README 过期记录，固定上游对象，
明确旧纪录的负载和绕过选项，保留现有 launcher、stress、原生 bootstrap 和审计器。
不再扩展 QMDB 索引容器 A/B 或 allocator 性能矩阵作为主线。

**P1：先接 rs 的 fast transfer。** 使用现有 `N42EvmFactory` 包装，保留本仓
`0x0302` randomness precompile 及 prevrandao 设置；上游默认 factory 不能整段覆盖。
上游只对 Prague/Osaka 有资格检查；Cancun 对照仍走原执行器，不能擅自扩大 fork 范围。
保留 inspector、合约、预编译、授权、access-list、费用/余额/nonce/溢出等回退。
对照同一批交易的执行结果、所有账户 original/status/reverts、收据、gas 和 QMDB 根；
确认 builder 和 follower 都真正使用该 factory。这个适配尚未落地，不能声称已有收益。

**P2：再接并行转账和组块复用。** 按发送方拆批，对共享收款方作确定性余额增量合并，
保留非转账回退；覆盖发送方同时是他笔收款方、beneficiary/reward 重叠、同账户二次触碰、
中断归还队列等情形。先迁移完整差分用例，避免 rs 已发生的重复 revert/`UnsortedInput`。
组块对照包括序列化、交易与收据承诺、bundle/graft，不能只计 EVM loop。

**P3：四节点运行配置接回现有工具。** 本仓 `testnet.sh --nodes 4` 已支持节点数变化，
但默认 dev genesis 不能冒充原生 QMDB-only 引导；`chain94-fleet.py` 使用七验证者历史 QC，
也不能截掉三人就改成四节点。应复用生成/启动流程，建立新的四人 genesis/roster（f=1，quorum=3）
和可信 QMDB genesis，引导后接同一 `n42-stress`、`qualify-1m-tps.sh`。
固定签名类型、发送方/收款方分布、余额、163k tx/block，记录真实账户触及量与进程参数。
先预热，再至少三个连续 60 秒窗口，之后长跑及单节点重启追赶；报告四节点共同 H2
提交的成功收据 TPS，检查 roots、reader 水位、WAL、nonce/费用与 timeout。

**P4：deferred execution / seal-first 单独推进协议适配。** 不永久排除 rs 已验证有效的路线；
它与 H2 并不天然冲突，但需要父执行结果承诺、分叉激活、执行失败撤销、按 hash 重试集合，
并调整验收的执行完成边界。旧 `N42_DEFER_STATE_ROOT` 是绕过校验的压测选项，
不能当成该协议。先闭合跨客户端向量和交接缺陷，再纳入四节点 A/B。

163k tx/block 要到 1M TPS，需要约 **163 ms 的有效连续提交周期**。
不能用 7/4 比例外推；rs 的 pacing 350ms 也不能原样当成达标配置。
旧 round53 的主要成本是 EVM 229ms、组块 assembly 162ms，重块提交间隔 943ms；
这决定优先做 P1/P2，而非继续优化约 20ms 的 sender drain。

## 本轮补读与限制

rs 固定 `466c1839791afadba7f1dd4d48c5518440dcafba`。Gov5 本地缓存 origin/main
已从此前 `a8e353ca` 前进到 `99f1039661f22362d91a9af2e49ecb98bc9c2d04`，
新增两个 DATC 周更新文档提交，已补读，TPS handover 未改变。此前一周索引 180/186
个提交仍保留在 devlog146；本轮 Gov5 补入 2 个，未声称刷新远端。

新增经验：批量 changeset 的旧尾段会原位重编码，不能 `rsync --append`；
暂存验收后完整 rename 替换段文件，最后更新索引，保留增量继续所需的当前态。
这是数据更新经验，本轮不启动 DATC 或改外部数据。

当前沙箱创建 TCP/UDP socket 返回 EPERM，真实四节点 TPS 尚不能在此测出。
继续本地实现和差分测试；不据此重写网络或把进程内基准包装成实机成绩。
本轮完成历史/源码映射与计划，尚无新的整链性能提升数字。
