# 性能主线：同机共享执行、原生帧与 QMDB bin 树

日期：2026-10-07。目标是先消除重复工作、复制和串行等待，再按实测瓶颈继续优化。
每批保持小提交、逐次推送，代码、正确性和容量结论分别记录。

## 当前事实与对照来源

- 本仓库使用 `Gov5QmdbStateRootStore → QmdbLeafTree` 的 QMDB 二叉承诺。
  账户读的持久化索引是派生读取视图，不是另一个状态承诺。保持现有编码、证明、undo 和 WAL 格式。
- `fast_transfer.rs` 已直接检查并计算合格转账的余额、nonce、gas 和结果，跳过解释器；
  `parallel_transfer.rs` 按 sender 依赖分组。不能再把“增加转账快速路径”作为尚未实施的新方案。
  普通合约、特殊交易和不符合资格的转账保留解释器执行。
- 现有 `ImportGate` 仅在一个适配器中串行化同哈希请求，每个请求仍提交完整 payload。
  它既不是跨进程执行共享，也不是完成结果缓存。上轮 E7 A/B/A 未证明吞吐收益。
- 现有领导者构块输出命中缓存；跟随者在已测 E7 路径仍独立执行。
  同机七验证者共享一 EL（E1）应成为第一条容量主线，E7 单独作为独立执行对照。
- 上轮 E7 合格数据：A1 37,952.29、B 36,904.73、A2 36,731.55 canonical TPS。
  构块 819–825 ms、打包 205–209 ms、规范化验证 963–971 ms、跟随导入 1,362.5–1,417 ms。
  QMDB prepare 平均 17–33 ms。这些阶段可能重叠，不能相加或外推 E1 容量。
  来源：[完整审计](benchmarks/20261007-pressure-feedback.md)。
- 只读参考 `../n42-rs` HEAD `bce4921b8`：
  `bin/n42/src/import_once.rs`、`crates/n42/tx-queue/src/frames.rs`、
  `crates/n42/engine-types/src/output_shards.rs`、`docs/SHARED_EXECUTION_SCOPE.md`。
  不修改参考仓库，不把旧 scope 文档中的“未实现”误当作当前代码状态。
- 最新参考 `loop346a-stopped2.out` 的 A2 记录 3,126,667 / 3,060,000 / 2,993,333 TPS，
  构块重复执行 0/1,426，抽样 Ed25519/回执读取通过，但最终 settlement 因 FCU 错误停止。
  因此这是一份机制与瓶颈证据，不是新的完整通过容量结果，也不是本仓库实测。
  参考使用延后两块填写字段的配置；本仓库不能直接照搬该协议时序。

## 目标结构

同机验证者保留各自密钥、投票与 QC 验证，共享一个执行服务与状态存储。
入队后的交易/帧为不可变对象。每个认证块由一个 owner 执行或接管已有构块输出，
其余请求者等待并读取相同结果；QMDB root、下一块读取和最终持久化消费相同的输出。
同机结果优先使用共享对象/句柄，网络才编码交易体或状态结果。

先采用一个 EL 进程服务多验证者的架构；若进程隔离要求仍需多个执行进程，才比较
Unix socket、共享内存不可变页和 RPC 服务的额外成本。共享内存需要版本、生命周期、
崩溃恢复和认证绑定，不能用裸指针替代。
E1 明确共享执行故障域；E7 保留独立执行、校验与持久化口径。

## 分批实施与验收

| 批次 | 借用思路及本仓库落点 | 测试与实际效果验收 |
| --- | --- | --- |
| P0：建立归因 | 每块关联 admitted/build/execute/root/seal/QC/commit/durable 时间；采集队列笔数与字节、CPU、RSS、major faults、编码和复制字节 | 缺失阶段记 unavailable；同一个块的时间线确认真正关键路径；修复前轮 converter 编译错误并补回归 |
| P1：同机执行一次 | 在共享 EL 内统一导入入口，按链身份、认证父状态及块身份复用 owner/result；接管构块输出，覆盖所有导入道路 | 同时七请求只发生一次执行、一次 root 计算及持久化；own_executed_again=0；换主、owner 取消、失败、重试、分叉、过期与边界淘汰测试 |
| P2：认证帧与传输 | 迁移 Ed25519 原生帧认证边界与不可变 frame index；frame plan 持 Arc 引用，同机传句柄，异机传 frame id 与缺失补取 | 错签、重放、截断、nonce 缺口、缺失帧、乱序和部分帧均有用例；测编码/解码次数、bytes/tx、补取率及端到端队列延迟 |
| P3：有界入队 | admission 同时限制 transaction、byte、in-flight frame；信用在真实释放后归还，batch 不能绕过限制 | 慢消费者与断连持续压力下 RSS/backlog 有界；记录 offered/admitted/canonical 和背压时间；16,384 frame index 保留上限不能替代信用 |
| P4：直接转账进一步瘦身 | 测量现有 fast path 对解释器；尝试 sender lane 的批量 nonce/余额结算和紧凑账户 delta，减少逐笔 Account/Result/HashMap 分配 | EVM 作为差分 oracle；验证 self-transfer、recipient 也是 sender、共享收款人、overflow、费用与失败次序、fork/system calls；不改变合法性和回执结果 |
| P5：分片输出直读 | 输出保留 batch maps，加 address→batch index，只合并冲突；QMDB 操作与下一块 provider 直接读冻结视图 | 与连续 BundleState 逐账户/slot、receipt、bin root 和 undo 比较；量化 freeze/index/conflict/merge、缓存 miss 和下一块等待 |
| P6：封块与共识流水 | 输出可读就允许依赖任务推进，把不依赖投票的连续状态合并与持久化准备移出关键路径；QC 与 durable 完成各自计时 | 最终字段仍遵守当前 H2/Gov5 协议；不能把参考延后字段直接移植。验证换主、超时、分叉、延迟输出、读等待、重启与恢复；衡量周期和完成延迟 |
| P7：存储深入优化 | 根据大状态测试比较 WAL group commit、批量哈希、受影响祖先折叠、快照与后台压缩；固定 fsync/恢复语义 | apply/undo/proof 和重启后根一致；报告 durable TPS、写放大、IO/tx、同步耗时和持续 compaction backlog |

P1 的结果复用必须以认证内容/状态为边界。仅凭请求声明的 blockHash 返回 VALID 会绕过
同哈希不同 body 的校验；可信的内部结果句柄与任意外部 payload 应走不同入口。
owner 中断后可接管，SYNCING/ACCEPTED 不永久缓存；活动项不能因容量淘汰而产生第二个 owner。
forkchoice 的 canonical 更新与执行去重分别处理，不能吞掉换主后的合法更新。

P4 不预设当前逐笔 EVM 包装必须保留，也不预设共享内存必须更快。候选可以是紧凑列式
账户数组、批量费用归集、sender lane 执行、按地址索引的不可变 delta。每次只替换一个边界，
用相同输入和状态差分确认，再比较时间和内存。发生 sender 交叉依赖时保持原交易次序。

## 测试和测量方案

1. **正确性门槛**：先模块与集成测试。状态、gas、回执顺序、原生头、QC、QMDB bin root
   和恢复后状态一致。所有失败运行保留日志；候选测试失败时不进入容量比较。
2. **执行微基准**：新增 `transfer_execution_probe`，同一 release 二进制比较解释器 A1、
   直接转账 B、解释器 A2，10 次/段，预热单列。每段最终三方账户及 gas 完全相同，
   B 强制命中 direct path。热缓存单 sender 基准只回答执行内核成本，不能换算成链 TPS；
   后续扩展多 sender、随机账户、真实 pinned QMDB parent、大状态与混合合约数据。
3. **E1 主线**：七验证者、一 EL，同一 genesis、账户、数据、线程预算与持久化设置。
   先短正确性轮，再固定二进制 warmup/A1/B/A2；至少重复三组，负载先 60 秒再 5–10 分钟。
   offered rate 逐级提升，直到 canonical 不再上升或 backlog 不再稳定；发送速率不是吞吐。
4. **E7 对照**：同一认证帧工作负载、相同总 CPU 预算；分别报告每 EL 执行/root 次数及资源。
   不用 E1 的单执行结果证明七份独立执行容量。
5. **验收数据**：common canonical tx/window、QC 与所有相关根、durable tx/window、
   p50/p90/p99 阶段与队列延迟、execution/root 次数、换主次数、bytes/tx、CPU秒/百万笔、
   RSS、major faults、WAL IO/tx。收益需超过 A 两端漂移且重复可见；同时检查尾延迟与 backlog。
6. **同机独占**：遵守 `/data/blockchain/wr-logs/BOX-CLAIM-PROTOCOL.md`；
   不与参考任务抢机器，不在 A/B/A 之间编译，重编译后第一轮排除。

## 本轮执行记录

- 前轮 frozen converter 测试实际编译失败（E0382）；三个测试/基准入口都未得到通过结果。
  已保存原日志与原源码，修复为移动交易体前保存 `current_hash`，保持错误输出身份不变。
- 增加直接转账/解释器对照 executable，最终账户与 gas 比较，限定纯执行微基准口径。
- 测试与两个微基准通过同一个独占驱动排队；共享机器仍有参考仓库测试占用。
  真实执行结果填入本节及独立报告后，再按阶段成本选择 P1/P2/P4/P5 的下一段实现。
  当前没有新的 TPS、加速倍数或运行正确性通过结论。
