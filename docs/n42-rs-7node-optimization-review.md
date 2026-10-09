# n42-rs 七节点性能优化对照

日期：2026-09-23。对照源为相邻仓库 `../n42-rs` 的 `feat/native-fleet7`，HEAD
`e8791e37e`；主要证据在 `docs/FLEET7_PLAN_V4.md`、`docs/FLEET7_PATH_AUDIT.md`
和 `scripts/fleet7-runs/`。该分支最新计划同时包含四节点结果，本文按实验节点数分别标注，
不把四节点数据当作七节点收益。

## 结论

没有找到可以安全地直接复制进当前 Gov5 H2 热路径、且已有七节点端到端性能证据的新增代码。
当前仓库已在执行层适配了 n42-rs 的普通转账快路径和 sender-group 并行执行；在线 payload
builder 也有并行交易构建路径。其余最有希望的点要么当前 H2 还没有相同的安全前置条件，
要么仍是未验证的微基准/四节点实验。本轮因此吸收其性能分析方法与实验结论，不改投票规则、
共识轮换或交易根格式。

## n42-rs 的实测与适用性

| 优化/实验 | n42-rs 证据 | 对本仓的判断 |
|---|---|---|
| 父执行结果完成时即发布给子块检查（`N42_CHECK_ON_PARENT_OUTPUT`） | 七节点 loop183、275ms pacing：V1a 的 R1 为 106ms（对照腿 V0a 为 153ms），窗口一 388.8k 对 339.8k；V1b 窗口一 401.7k、窗口总量 27.51M。两腿均无 invalid block，但 V0b 的 R1 读数为 7ms，说明不能把所有腿都解释成同等幅度的 R1 改善。来源：`FLEET7_PLAN_V4.md` §2c。 | 算法思路值得保留，不能照搬到 Gov5 H2 参与者。当前 `execution_bridge.rs` 明确要求该参与者等当前 payload 被 reth 返回 `Valid` 后才释放 H2 投票；n42-rs 的提前父结果检查处在不同的投票阶段。须先证明检查覆盖本仓 QC 投票所依赖的全部状态条件，再设计独立 A/B。 |
| follower 在父块执行输出上执行子块（`N42_FOLLOWER_EXEC_ON_PARENT_OUTPUT`） | 七节点 loop183 两腿未显示稳定收益；loop183 V2a 出现 29 个 invalid 与 3 次 TC，但当腿同时内存降到 2.7GB，报告认为该故障与新开关无关。后续七节点尝试中，投票与执行路径几乎没有可重叠时间。来源：§2c、§2h。 | 不移植。当前只有在精确 parent、canonical/read-view 和 QMDB 版本关系闭合后才可执行；先量化可重叠时段，避免引入跨块状态叠加和分叉清理复杂度。 |
| 更长 leader tenure | `F7_LEADER_TENURE=256` 的 663k/663k、窗口二 625k/647k，来自 loop215 **四节点**，不是七节点对照。七节点早期使用 tenure 16。来源：§5.1 与 §0。 | 不套用到 H2。改变 leader 轮换会改变共识协议时序/公平性与故障切换边界，不等于普通性能开关；只有协议层明确支持同等 tenure 且多节点参数一致时再评估。 |
| 按交易 hash 描述区块（Block By Description / compact body） | 后续四节点的紧凑 body 将 R1 缩短约 20–65ms、组装从 112–116ms 降到 65–75ms，但 follower execution 增加约 36ms，窗口二/三下降 15–20%，因此该配置被关闭。新的 v5 描述式区块在 loop220 前只有 22–31ms 的局部 root/查找测量，整链收益尚无结果。来源：§2r、§5。 | 先不移植。当前 `BlockDataBroadcast` 有完整 payload、执行输出及已审过的身份/哈希边界；改成 hash 引用会新增缺项回退、队列驻留与来源验证协议。须先有本机命中率和七节点整链对照。 |
| 增大块、增加线程、把签名验证移出 follower ingest | 260k 块的周期变为 163k 的 1.7–1.9 倍；八线程配置被 loop181 否决；关闭 follower 验签把约 132–135ms 签名成本移到约 217ms 的 vote road。来源：§1、§5.1、§5.3–5.4。 | 吸收为明确的负面实验结论：不以更大块/更多线程/延迟验签作为默认优化。当前验签语义保持不变。 |

## 本仓已吸收的实现

- `crates/n42-execution/src/fast_transfer.rs` 与 `evm_config.rs`：按 n42-rs 的普通转账
  EVM 快路径改造为本仓 factory 适配；保留本仓预编译、fork 条件及解释器回退。
- `crates/n42-execution/src/parallel_transfer.rs`、`parallel_block.rs`：移植 sender 分组、
  候选顺序、bundle graft 和 revert 合并，并覆盖本仓的账户交叉依赖与 QMDB 约束。
- `crates/n42-node/src/parallel_payload.rs`：并行 builder 已挂到现有 payload 生命周期，
  通过 `N42_PARALLEL_BUILD=1` 显式启用；follower 的并行 import 由 `N42_PARALLEL_IMPORT=1`
  配置。它们应继续用同配置差分与原生 fleet 资格运行验证，局部正确性和吞吐不能互相代替。

实现适配记录见 `docs/devlog-166-fast-transfer-adaptation-20260914.md`、
`docs/devlog-167-parallel-transfer-adaptation-20260914.md`、
`docs/devlog-168-parallel-payload-builder-20260914.md` 和
`docs/devlog-170-parallel-engine-import-20260914.md`。这些文档明确指出：部分完整区块执行
适配器尚未进入 follower 在线路径，且这些适配记录没有本仓 fleet TPS 成绩。不得把上游成绩
或小规模差分测试写成本仓性能提升。

## 建议的下一轮吸收顺序

1. 在当前原生七节点配置上补齐与上游一致的阶段计时：proposal 到 quorum 的投票路径、父结果
   检查、执行输出发布、reth `new_payload` 与共识释放投票时间；固定块大小、交易和账户分布、
   pacing、节点资源以及窗口二/三指标。
2. 对现有 fast transfer、parallel build/import 做单一开关的同配置 A/B；只有窗口一和持续窗口
   同时改善、所有节点 roots/QC/成功收据一致、且没有 invalid/TC 增长，才记为已吸收的性能收益。
3. 若阶段计时确认父执行结果检查仍处于 H2 投票临界路径，再基于本仓 `Valid` 投票边界设计
   证明义务和 fallback；若发送方查找/反序列化占主要成本，再单独评估描述式 body，先测 queue
   命中与回退，再跑完整七节点 legs。

上游所有数字均为各自分支、配置和硬件上的结果，不能直接预测本仓收益。此对照没有启动
`../n42-rs` 或本仓的 fleet，也没有修改其源码或外部运行环境。
