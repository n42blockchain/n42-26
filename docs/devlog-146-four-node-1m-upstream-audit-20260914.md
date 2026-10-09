# 4 节点 1M TPS：本周更新审查与第一批实现

日期：2026-09-14。目标持续进行，**尚未达到或实测 4 节点 1M TPS**。
架构约束：HotStuff-2 共识、QMDB 二叉树状态承诺和状态读取。

## 审查范围与版本

按 2026-09-07 00:00 EDT 起的提交建立完整索引，重点阅读执行/存储/共识
关键变更、对应回归及实验复盘。索引不等于逐行审计全部提交。

| 来源 | 本次固定版本 | 窗口内提交数 | 边界 |
| --- | --- | ---: | --- |
| n42-rs | `466c1839791afadba7f1dd4d48c5518440dcafba` | 180 | 本地 HEAD，2026-09-14 04:37 EDT |
| N42-gov5 | `a8e353cac7a00c7f6ca2816cc8e4634c45a4f337` | 186 | 本地缓存 origin/main，2026-09-14 04:37 EDT |
| 当前 n42-26 | `dbbcb54` 加本次修改 | — | 保持既有 reth 依赖和锁文件 |

完整 SHA、时间、主题、tree hash 见
[`upstream-inventory.json`](benchmarks/20260914-qmdb-leaf/upstream-inventory.json)。
SSH 默认配置首先报权限错误；指定独立 SSH 配置后 DNS 仍被沙箱阻止；网页读取亦失败。
因此不声称已刷新远端。gov5 工作区实际停在 9 月 4 日的 `4533358f`，并有大量旧的
未提交改动；本周审查使用 `git show origin/main:...`，未切换或修改其工作区。
两边历史有重写，引用以上固定对象，不按旧提交 SHA 盲目 cherry-pick。

## 已读出的有效经验与风险

| 主题 | 上游证据 | 对当前项目的处理 |
| --- | --- | --- |
| QMDB 每个块批量折叠叶路径 | rs `09642e0c3`；gov5 `9ecfa3be`，root computer 改调 ApplyOps | 移植到当前 `QmdbLeafTree`，保留排序、叶编码、二叉树根、撤销语义；见下方实测 |
| 上层二叉树增量缓存 | rs `255a2ec52` | 当前 leaf tree 已有，不能算新增收益；仍有共享祖先重复计算，可后续单独测 |
| 内存跟随实际负载 | rs `7439b0b2c`/`a8140b164`：小 undo、retention 16；gov5 `15710ab9`：tx lookup 尾部 64 块可达 10.4M hashes、约 1.1 GB/节点 | 不把区块数当稳定内存边界；用交易数/字节及消费者进度共同约束 |
| 持久化排队 | rs `c7e40e23b`/`28a94b94b`：fsync 移出森林锁，同时补齐 delta/checkpoint 耐久；gov5 `f0603e82`：history fold 的准备工作移出 MDBX 写事务 | 优先减少锁内计算；不能删 fsync 冒充吞吐提升。当前 QMDB WAL 已在独立锁同步，无需重复移植 |
| 真正的 QMDB 状态读取 | rs `8a0a6ece5`：版本化 read view，先 verify 再 on；gov5 `6445f1bf` 审查确认 import 账户/槽读取已走 QMDB | 当前 `Gov5QmdbStateRootStore` 是根计算/归档存储；还没有等价的 EVM latest-state 读取接入，必须继续实现 |
| 不能提前停写 hashed state | rs `56feadae3` 后 `e9bf24024`/`eee453638` 复盘：storage v2 的 HashedAccounts/HashedStorages 就是持久化 latest state | QMDB 读取没有覆盖 latest、历史回退、重启、unwind 前不删写入；禁止把这些表误判成无用 MPT 派生索引 |
| reader 与 retention 的耦合 | rs `cdb074fa2`：DB 落后 17–18 块时，retention 16 先删了 reader 所需记录；新增 keep_from，仍有 64 块上限 | 固定 DB 事务的实际状态边界，显式 reader 水位；不可直接读可能位于待构建块/兄弟分叉的可变树 tip |
| 读取失败不等于不存在 | gov5 origin/main 的 reader 仍使用 `Get`，verify 跳过部分出错比较；旧工作区另有 `GetChecked` 和错误传播改动，未在所审 main 中 | 读取接口需要区分“值不存在”“该版本不可用”“I/O/解码错误”；不能以 0 mismatches 掩盖失败或零次比较 |
| 推测构建必须读到未写入父状态 | gov5 `2d826e5a`/`521dd53f`/`3c0b917d`：顺序/并行 fill 需要 parent layers；`d36f6269`：空账户按删除语义处理 | sibling hash、父版本、EIP-161 删除和 SELFDESTRUCT 槽枚举都应有回归，不能只测普通余额转账 |
| deferred execution 不只是提前投票 | rs `a322a0642`/`12f7ae1d8`；gov5 `93e31b89`/`b15cd2b7`/`6445f1bf`：父执行根、gas、fork timestamp、签名恢复、费用边界、拒绝后的证据撤销 | 这是 fork 协议改动。暂不搬入当前项目，也不打开 vote-before-import 来报目标数字 |
| 提交先到、区块后到 | rs `61af2de22`/`f888c8257`；gov5 `6445f1bf`：retry 由单槽改为集合 | 按 block hash 保留所有未完成提交；SYNCING/尚未导入属于重试条件，不能吞掉，也不能一概标为 Invalid |
| builder 输出复用必须绑定身份 | rs `17f1328f8`：相同 parent 的 sibling 不是自己的构建；`466c18397`：过期 executed insert 被丢后，header-only payload 执行空交易 | 不凭同 parent 或执行缓存命中认定执行正确。当前 iterator 解码实际 payload，validator 未发现相同 remembered-full-block 路径，不机械移植补丁 |
| 测量口径和实验配置 | rs `9a40a002a`：recipient 索引修复使触及账户从约 13k 变 147k；gov5 `14f29f6e`：packet-window 参数未传到节点 | 必须记录交易类型、发送方/收款方分布、实际进程参数、第二窗口退化；不能比较不同状态形状的峰值 |

上游结果也不是本项目的结果：rs 记录的 333,416 是 7 节点第一窗口，
并有交接错误尚未闭环；gov5 最新完整对照的 B 均值 90.6k，125.9k 是停止前单窗口，
deferred fleet 仍未跑。来源分别为固定版本的 `FLEET7_STATUS.md`、
`FLEET7_PATH_AUDIT.md`、`QS_HANDOVER_20260912.md`。不得互相替代或外推成 4 节点 1M。

## 第一项实现：块内叶哈希折叠

修改 `crates/n42-twig-core/src/qmdb_leaf_tree.rs`：

- `apply_sorted_ops` 先完成重复 key 检查，连续 append 只填叶数组。
- twig 封存前以及整块结束时，自底向上仅计算所改叶范围的祖先，每个祖先一次。
- 直接调用 `set` 仍立即维护树；公共调用结束时没有待折叠叶。
- 空块、删除、更新、跨 twig、回滚、快照、证明仍沿用原格式。
- 同一块内一棵满 twig 的内部节点从逐叶路径约 22,528 次哈希降至 2,047 次；
  没有修改叶哈希或引入新的分叉规则。

release 微基准：先填 200,000 个 key，每次更新 147,000 个 key，包含排序、
value 更新、undo 记录与状态根。输入生成/复制与树 clone 在计时外；每轮撤销并验证
原根，两实现结果根相同。6 组交替次序；固定 3 字节测试值，**不是真实账户编码**。

| 统计 | 原逐叶路径 | 块内批量路径 |
| --- | ---: | ---: |
| 中位耗时 | 220.331 ms | 83.222 ms |
| 最小–最大 | 219.116–227.707 ms | 82.323–84.670 ms |
| 中位耗时变化 | — | -62.2%，约 2.65x |

原始日志：[`release.log`](benchmarks/20260914-qmdb-leaf/release.log)。
这是存储阶段 CPU/内存微基准，未包含真实节点网络、交易签名、EVM、fsync 或共识。
当前节点并非所有状态路径都使用 `QmdbLeafTree`，不能将此比率套到整链。

## 第二项实现：4 节点 H2 提交吞吐口径

原 `qualify-1m-tps.sh` 默认 7 节点，只采节点 0 的 `eth_blockNumber`，
随后把 canonical inclusion 标成 `strict_committed_tps`，无 1M 阈值失败。

现在默认 4 节点，新增 `scripts/h2-tps-audit.py`：

1. 开始/结束均采集所有节点 `n42_consensusStatus` 的已提交 hash/view，
   绑定执行层同 hash 的区块，要求 chain id 和 validator count 一致。
2. 起点取所有开始提交高度的最大值，终点取所有结束提交高度的最小值。
   分母包含开始采样前到结束采样后的全部时间，使用同主机同 boot 的 monotonic clock。
3. 复查所采提交未变，核对每个区块的 parent，以及所有节点共同首尾区块的
   hash、state/receipts/transactions roots 和交易数；缺数据、回退、断链即失败。
4. 保存 `h2-start.json`、`h2-end.json`、`h2-audit.json`，低于默认 1,000,000
   已提交 inclusion TPS 时写 `below_target` 并返回非零；资源摘要照常保留。

H2 RPC 报告及链一致性检查不等于独立 BLS 证书验证，也不证明交易都执行成功、
磁盘已同步、四个 URL 对应四个独立进程，或实际 EVM 状态读取后端已经是 QMDB。
配置摘要明确来自 launcher 环境，不能当节点实际配置证明。完整目标验收仍需后续
部署身份、状态读取计数、交易/状态正确性及耐久检查。

## 验证与环境限制

- QMDB 核心单元测试：62 passed，2 个显式微基准/外部快照测试 ignored；
  包含已有跨客户端向量及新边界/重复 key 测试。
- 节点 QMDB store 集成单元测试：17 passed，2 个测量 ignored；包含兄弟分叉、
  精确父状态、WAL 失败回滚、重启恢复及 fsync 不持有状态锁。
- release 对照基准：1 passed，6 组根与回滚一致。
- `cargo clippy -p n42-twig-core --all-targets -- -D warnings` 通过。
- `python3 scripts/test-h2-tps-audit.py`：11 passed，覆盖真实 CLI 阈值退出码、
  节点掉队、QC hash 绑定、分叉、根不一致、缺区块及时间边界。
- shell 语法、内嵌 Python 解析、`git diff --check` 通过。

工作区 `../reth` 的未提交额外依赖使直接 `cargo ... --locked` 要求更新锁文件。
依照 devlog-145，使用已经固定并补丁化的干净 reth 副本
`/tmp/n42-ci-repro-j5wu4xcf/` 验证；只同步本次修改的 twig 源文件，保持根锁文件不变。
示例复现命令（干净依赖环境准备后也可从仓库根直接运行）：

```bash
cargo test --locked --offline -p n42-twig-core --lib
cargo test --release --locked --offline -p n42-twig-core --lib \
  bench_batched_leaf_paths -- --ignored --nocapture --test-threads=1
python3 scripts/test-h2-tps-audit.py
N42_BENCH_NODES=4 N42_BENCH_MIN_COMMITTED_TPS=1000000 \
  bash scripts/qualify-1m-tps.sh
```

本沙箱调用 `socket.socket()` 就返回 `EPERM`，并非只缺 RPC 节点；不能启动
真实本地 4 节点、进行网络压测或证明 1M。没有修改别的仓库工作区、启动已有 fleet、
清空其数据或改变系统内存设置。

## 继续推进顺序

1. **接通真正的 QMDB 读取**：精确 block identity 的不可变读视图；先给账户/槽
   做 verify 与错误计数，再接入执行 provider。读取不可用和真实缺 key 必须分开。
   优先覆盖创建/删除/恢复为原值、SELFDESTRUCT、重启、兄弟分叉、DB 落后 retention。
2. 基于真实账户大小和当前生产路径测根计算、账户/槽冷暖读取、持久化锁与内存增长。
   后续优化先消除块操作 clone、重复编码/哈希和缓存容量失控，再考虑并行。
3. 4 独立验证节点使用固定 genesis、相同交易形状和实际配置，按 60 秒窗口至少 3 窗
   做对照；记录共同提交 TPS、成功交易数、H2 view/timeout、每节点执行/QMDB 水位、
   CPU/RSS/I/O、同步与重启恢复结果。不能用投递速率、单区块峰值或 sum(各节点 TPS) 达标。
4. 当前网络限制不影响上述实现及离线差分测试，目标保持进行中。
