# 按完整操作身份保留 QMDB 候选计算

日期：2026-09-14，接续 [devlog-161](devlog-161-qmdb-dirty-twigs-20260914.md)。
**4 节点 1M 成功提交 TPS 仍未实测达标。** 本轮减少本地 builder 和 import 之间
重复的 QMDB 应用/撤销工作，保持 H2、二叉树 QMDB 读取、正常重执行及 WAL 同步。

## 为什么不能只凭 parent 复用执行

再次检查固定 n42-rs 提交 `17f1328f8f68296d84277ea1429f53ccf01a324b`：上游曾把
相同 parent/高度、相同交易的 sibling 当成本地构建块，把本地执行根记到对方 hash
下，导致后续块被拒。受益人、timestamp、randao、base fee 等同样影响执行身份。

本轮**不复用 EVM 执行结果**。import 仍从实际 payload 重执行，构造自己的完整
QMDB 操作，再比较精确 parent hash 和全部 key/value/delete 内容。二叉树根是
该父状态和确定性排序操作的函数，因此可以复用树计算；这不授权跳过 header、
receipt、gas 或交易验证，也不把 builder 的原始 hash 当作最终 normalized hash。
相同父块但操作不同会失配；相同操作但父块不同也会失配。仅比较 root 不足以命中。

## 一份有界的未发布计算

`prepare_candidate(parent, owned_operations)` 供 builder normalization 使用。
原 `compute_candidate` 保留立即撤销的语义，仍可用于普通候选检查。

- 准备路径应用一次、得到 root 和 undo，保留原有操作 Vec 的所有权，避免额外
  clone 一份缓存 delta。准备阶段不插入 block graph、不发布 read view、不写 WAL。
- state 中明确保存 `prepared`；`tree_at` 指其父块。匹配提交取出原 undo 和已应用
  的树，然后沿原路径比较 header root、派生 read view、编码、写入和 fsync WAL。
- 不匹配、普通候选计算、历史 snapshot/proof、读取版本重建、tree stats，以及
  显式/后台 base 写盘前，都先撤销 prepared。已有不可变父 read view 可直接使用，
  既不泄漏候选值，也不必为了读父状态抛弃候选。
- 缓存至多一份，保留计费上限 **64 MiB**。计费包括操作 Vec/value capacity、undo
  的 Vec/value capacity，并保守计入新触及叶堆。超过上限仍正确计算根，立即撤销，
  提交时走普通应用。它不是整个 tree、allocator、临时计算或进程 RSS 的硬上限。
- 编码失败、根不匹配和 WAL 失败都沿原路径撤销；持久化前不发布读取版本。允许
  在父 WAL 写入期间准备子候选，但父失败必须撤销它；父成功后才可正常提交子块。
- 持久化格式和原有状态表写入不变。prepared 从不作为一个已提交块保存，重启从
  已校验持久化记录恢复，重复提交仍逐内容检查且不追加第二份 WAL。

新增指标：`n42_qmdb_prepared_candidate_bytes`，以及
`n42_qmdb_prepared_candidates_total{outcome=retained|oversized|discarded|adopted|miss}`。
`adopted` 指选用了匹配计算，之后仍可能 root/WAL 失败，**不能当作提交计数**。
准备路径复用现有 lock-wait/hold 和 candidate-compute 直方图，operation 为 `prepare`。

## 包含持久化和读取版本的 release 对照

同一可执行程序选择原 `compute_candidate` 或新 `prepare_candidate`，随后均调用
当前 `compute_and_commit`。使用真实 Gov5 账户编码的合成负载：初始 200k/2M
账户，每块更新 147,000 个不同账户，开启不可变 QMDB read views。

计时包含两端原地排序、候选树计算、需要时的撤销、完整操作匹配、提交应用、
读取版本构建、WAL 编码、实际 ext4 写入和 fsync。原路径释放 builder delta 的
成本也在计时内。EVM、delta 生成/初始复制、初始树、oracle 和重启检查在计时外。

每个状态规模 6 组交替执行顺序，每进程 3 块，每格 18 个样本：

| 初始账户 | 原候选撤销 + 提交 | 保留候选 + 提交 | 总耗时变化 |
| --- | ---: | ---: | ---: |
| 200k | 280.7690 ms | 147.6775 ms | -47.40% |
| 2M | 427.1995 ms | 229.2700 ms | -46.33% |

各阶段中位数（它们不能相加代替总耗时中位数）：
候选列包含两份输入的排序，不直接等于节点内的 candidate-compute 指标。

| 初始账户 | 原候选 / 原提交 | 新候选 / 新提交 |
| --- | ---: | ---: |
| 200k | 153.204 / 123.721 ms | 86.5215 / 59.5460 ms |
| 2M | 198.683 / 226.892 ms | 94.0120 / 135.8635 ms |

72 个样本的对应状态根与 WAL 长度一致，24 个进程均在计时外重开持久化 store，
核对最终根及 QMDB 账户 nonce。每个新路径样本都确认实际保留了候选；没有把
超预算回退混成命中样本。未绑定 CPU，也没有独占宿主机；计时期间本轮编译和
测试均已结束。完整样本、阶段耗时及极值保留。

这是**本地 builder 命中的存储路径**，不是所有验证者或所有块都会命中：历史读取
和其他候选会撤销缓存，EVM 重执行可能产生不同 delta，节点轮流担任 proposer。
没有真实集群命中率证据，不能将 46–47% 套到整链 TPS。

## 验证与证据

- node QMDB：58 passed、7 ignored；另有实际 provider 精确版本读取测试 1 passed，
  builder normalization/缓存身份测试 2 passed。
- 新增七项回归覆盖：完整 delta/重排匹配、错误 header root、sibling parent 和
  value/delete/empty 差异、重复 key、容量按 Vec capacity 计费、历史读取与持久化
  不泄漏候选、WAL 编码/写入失败与重试、pending 父失败/成功时子候选的行为。
- 成功父 WAL 场景还核对重复提交不增长 WAL、父读取版本没有子值、重启后子状态。
- 独立 release harness 中六项缓存回归和四项读取版本测试通过，两个测量默认 ignored。
- workspace all-targets clippy `-D warnings`、`git diff --check` 通过。
- [源文件、提取脚本、计时程序、全部样本和验证日志](benchmarks/20260914-qmdb-prepared-candidate/)。

节点验证仍使用固定补丁 reth `23316e3ff8adca8c3bd5085ff0565fcae019202a`。
未修改相邻开发 reth，未刷新或声称刷新远端引用。网络限制仍使真实四节点压测
不可运行；下一步应分解剩余 read-view 更新、WAL 编码/同步成本，并在可运行环境
核验实际命中率、连续成功 TPS、重启追赶和业务状态，目标继续进行。
