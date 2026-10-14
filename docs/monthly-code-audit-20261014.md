# 三工程近一个月代码审计与修复（2026-10-14）

## 范围与结论口径

审计区间为 `2026-09-14T00:00:00Z` 至 `2026-10-15T00:00:00Z`，同时审查当前未发布工作区快照。
重点是自有代码的执行输入绑定、共享执行结果生命周期、帧背压、QMDB bin 树状态承诺、持久化恢复、H2 最终性及 Gov5 导出契约。
这不是形式化证明，也不表示逐行审查了全部导入上游、历史性能数据和文档副本。

| 工程/分支 | 冻结起点 | 区间前基线 | first-parent 提交数 | 净变更文件 |
| --- | --- | --- | ---: | ---: |
| n42-26/main | `18fe108881416a72bfc42754f8a5d97505ed64d5` | `e89eb5c614980cf15287802873a49320d57de28d` | 61 | 1307 |
| N42-gov5/qs/block-time-budget | `44592a0536d849c06392437e019bfcc9077c8207` | `4533358fc28db215a12b78577968037ee4fec33d` | 4 | 10 |
| n42-rs/main | `abe8891c6e76b77729c5e9522cf25503ad0c88e9` | `f9c6fcea47ac8acd9a9c55ffcb559e67933d7749` | 730 | 2631 |
| RS 链 94 兼容分支 | `d64964d87b92d33770348bc32c5cd0424f657567` | `e7d5c0cf7d78888f14e69c640e3f9e2bd6703293` | 36 | 223 |

n42-26 有 1091 个文档文件；RS 的变更中包含 1141 个 vendor 文件和 1010 个脚本文件。
Git 历史数量不能用作独立安全审查的覆盖率。完整基线与清单见同目录 JSON；本地原始证据在 `.artifacts/monthly-audit-20261014/`。
Gov5 起点存在其他会话的 76 个已跟踪文件改动和 76 个未跟踪代码文件，已保存 patch、源文件及 SHA 清单，在隔离副本验证；没有批量提交这些改动。

## 修复清单

| 编号 | 风险 | 原因及修复 | 回归证据 |
| --- | --- | --- | --- |
| R01 | 中：最终性生命周期 | 已 Finalized 的普通提交仍可被迟到的失败 FCU 回调改回可重试；终态检查覆盖失败回调和重试入口 | `late_failed_fcu_cannot_reopen_a_normally_finalized_commit` |
| R02 | 高：QMDB 状态承诺 | 恢复槽记录仅绑定父块和交易列表；不同时间/coinbase/blob 价格的投机构建可能覆盖彼此。执行端、构块端、验证端和 replay 工具统一绑定实际 block env、父块、beacon root、withdrawal 余额输入及有序交易 hash | 环境分离、双记录保留、真实 TIMESTAMP 合约投机构建、存储槽集成测试 |
| R03 | 高：脏存储写入兼容 | n42-26 仅观察交易净状态，漏掉同一交易内恢复原值的 SSTORE；借用 RS 的成功 SSTORE 观测，保留原指令及 gas，正式执行期间才开启记录 | 真实合约先写 7 后恢复 5，以及相同值 SSTORE 不产生额外重写 |
| R04 | 低：输入拒绝健壮性 | replay v2 的 `to-from+1` 对完整 u64 区间溢出；采用 checked 算术，解码和纯验证均返回 Bounds | `full_u64_range_is_rejected_without_arithmetic_overflow` |
| R05 | 高：执行确定性 | 随机数预编译的线程变量仅在构造 EVM 时设置；交错构造/执行、换线程或缺省随机值可读到另一块的值。在普通交易和 system-call 执行入口按实际 block env 重新绑定，None 使用零值 | 交错构造的真实预编译调用、None 不继承旧值 |
| S01 | 中：内存上界 | 淘汰的执行结果进入无界释放队列，逻辑缓存容量不能约束退休内存；容量改为 2，满/断连时释放所有权并在缓存锁外销毁 | 满队列/断连/精确释放测试 |
| S02 | 中：内存上界 | QMDB Released 批次同样使用无界队列；容量 2，满/断连时在 forest 锁外就地释放 | `forest_release_queue_returns_full_and_disconnected_batches` |
| S03 | 高：执行结果复用 | 同体交易和主要环境字段一致时，blob gas 输入改变仍可通过复用判定；补齐 excess_blob_gas、blob_gas_used 等值约束 | `a_reseal_cannot_change_blob_gas_execution_inputs` |
| S04 | 中：后台执行记录生命周期 | RS 兼容分支原先让预热/raw EVM 线程持续记录 SSTORE，无消费路径；引入只在块执行器中启用、错误和 unwind 自动清理的捕获作用域 | 捕获关闭/unwind 测试及原有 SSTORE/EOF 回归 |
| G01 | 中：可发布构建 | 已提交 anchor 工具依赖仍在未跟踪文件中的投票存储 marker 读取函数；发布最小真实读取函数，不伪造 marker 缺失 | marker 不存在/存在/损坏值测试；五个工具包构建测试 |
| G02 | 高：源库完整性 | 导出文件可指向源数据库内部或彼此覆盖；解析实际 symlink 父路径，拒绝源库内输出及重复目标。用随机 CreateTemp 代替固定 .partial | 重复路径、源库文件/目录、symlink、无效路径在打开源库前拒绝 |
| G03 | 中：失败导出 | 已发布循环先判断 key 非空，可能吞掉同时返回 nil key 的游标错误；先检查 error，再检查结束。同时清理失败导出的随机临时文件 | `TestAccountCursorFailureWithNoKeyIsNotEndOfExport` |
| G04 | 高：已存在但未发布的认证修复 | 审查并补发布 Gov5 当前快照中的 checked read、live-entry/leaf 认证、链 ID/高度一致性、逐行 plain state/bytecode 认证及完整覆盖检查；不批量带入其他工作区变更 | 冷读取错误、损坏 entry、错链 ID、缺失 plain account、失败不发布/不残留临时文件，正常和无 applied marker 的源库均可导出 |
| T01 | 测试竞态 | RS flood 测试只等交易排空，漏等后入队的帧描述，偶发 1599/1600；等待 frame inbox 也完成 | 全量重跑及 20 次重复测试 |

无界队列修复的代价：释放线程跟不上时，调用者会承担锁外销毁耗时；保证内存上界优先于持续积压。
容量 2 表示等待批次上限，另有一个工作线程当前处理的批次；不表示整个进程只有两个状态对象。
缓存上下文哈希仅是进程内元数据键，不改变交易、区块、QC 编码或 QMDB bin 树算法。

## 已核查但没有新增修复的关键路径

- ETH ECDSA 自定义链和 Ed25519 性能路线的配置与类型边界；Ed25519 已有弱公钥拒绝、canonical scalar 及单条/批量验证一致性测试。
- 直接转账的 nonce 上界、余额/奖励 checked 运算、precompile/合约/inspector 回退，以及并行路径与解释器的 QMDB 根/重启比较。
- H2 最终性回调、投票持久化、水位和 locked QC 恢复；没有降低 7 节点、f=2 的阈值。
- 帧分批排空的事务顺序、计数在锁内更新和背压镜像；本轮 flood 失败是测试完成条件，不据此宣称生产队列丢帧。
- replay v2 链身份、连续父哈希、块头/交易/回执承诺、长度上限、尾部校验及物化内存限制。
- QMDB 账户解码的 canonical 编码、存储读取、WAL/重启根比较；Gov5 当前快照已有链配置检查、持久化 live-entry 认证及游标错误修复；本轮审查、验证并发布相关最小子集，明确归属既有工作，不计作本轮原创。

## 验证与交付

| 最终验证对象 | 结果 | 本地证据 |
| --- | --- | --- |
| n42-26 最终全量 workspace | **1823 passed / 0 failed / 30 ignored** | job 020 |
| RS 主线全量 workspace | **3355 / 0 / 87**（含上游及 doc-test） | job 009 |
| RS 链 94 兼容全量 workspace | **1946 / 0 / 35** | job 017 |
| Gov5 当前快照 | 九包普通测试、三包 race 全部通过 | job 006 |
| Gov5 最终可发布分支 | 四包普通测试、两包 race 全部通过 | job 019 |
| RS flood 回归 | **连续 20 次通过** | job 015 |

Root code HEAD `7c5fcc6`，RS main `5fdb986e1`，RS 兼容分支 `fecb43bbd`，Gov5 发布分支 `11d82582`；文档提交可能继续推进分支，以上是本报告记录的代码交付点。
Root 257 个 Rust 源文件与实际测试适配 workspace 比较，没有函数代码差异；离线 QC verifier 只有两行文档注释差异，具体清单记录在 JSON。

Root 分段提交：`d339d1a`、`8ad1292`、`9744159`、`9cab8a6`、`7c5fcc6`。
RS/Gov5 的提交详情与全局入口分别见 [RS 审计记录](https://github.com/n42blockchain/N42-rs/blob/main/docs/monthly-audit-20261014.md) 和 [Gov5 发布分支审计记录](https://github.com/n42blockchain/N42-gov5/blob/fix/monthly-audit-20261014/docs/monthly-audit-20261014.md)。

复现命令：

```bash
# Root 使用已经适配并锁定 Reth v2.7.0 的工作区；准备流程见 Reth 稳定版适配文档。
cargo test --release --offline --locked --workspace \
  --manifest-path .artifacts/reth-stable-completion-20261012/workspace/Cargo.toml \
  -- --test-threads=8
# RS 在主线/兼容 worktree 分别运行。
cargo test --release --offline --locked --workspace -- --test-threads=4
# Gov5 发布 worktree，构建标签与项目 Make 流程一致。
go test -p 4 -buildvcs=false -tags nosqlite,noboltdb -short \
  ./cmd/n42-reth-state-dump ./cmd/n42-consensus-anchor ./lib/qmdb ./modules/state/commitment
go test -p 4 -buildvcs=false -tags nosqlite,noboltdb -race -short \
  ./lib/qmdb ./cmd/n42-reth-state-dump
```

构建任务使用 `CARGO_BUILD_JOBS=8`、`RAYON_NUM_THREADS=8` 或 `GOMAXPROCS=8`，共享压测机全程通过 claim 驱动串行执行，最终占用已释放。


最终测试、提交和推送记录由 [机器可读审计清单](monthly-code-audit-20261014.json) 给出。
临时编译失败、首次 flood 失败和后续成功均保留日志，不删除失败证据。
Root 使用锁定的 Reth v2.7.0 适配源构建；RS 主线及兼容分支分别使用自己的锁文件，兼容分支仅新增已有 revm/alloy-eips 的直接依赖边，没有升级其他依赖。

Gov5 修复发布到 `fix/monthly-audit-20261014`；导出路径和临时文件清理也应用到当前脏工作区，既有游标修复未重复覆盖。该工作区没有做强制 reset 或批量 merge。
RS 主线的小提交已逐次推送审计分支，再安全 fast-forward 并推送 main；链 94 兼容修复留在原兼容分支。n42-26 修复分段推送 main。

本轮不将历史七节点一小时/恢复结果改写为新二进制验收。历史冻结结果见 [链 94 恢复修复验收](chain94-recovery-repair-20261014.md)。
全量测试含忽略的长压测；通过单元和集成回归不代表新的 TPS 测量、长时运行或强制崩溃验收。
