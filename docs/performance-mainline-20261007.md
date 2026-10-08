# N42 原生实时链性能主线

更新：2026-10-07。按用户纠正，以 N42 链的实时交易、出块、执行和共识为对象。
本计划替换此前围绕通用 Engine 适配器、独立执行层对照和 witness 的安排。
既有回放/兼容路径的数据保留为历史记录，不用来验收这条原生链路。

## 核对的参考代码

只读核对 `../n42-rs` HEAD `f6ab43cea` 及当前源码：

| 环节 | 参考源码 | 实际机制 |
| --- | --- | --- |
| 原生交易 | `crates/n42/tx-types/src/{alt_sig,envelope,primitives}.rs` | `0x50` Ed25519 交易；sender 由算法和公钥派生；`N42TxEnvelope`、N42 Block/Receipt 贯穿链路 |
| 认证与入队 | `tx-types/src/frame.rs`、`tx-queue/src/{lib,frames}.rs` | 入队计算 frame root；认证覆盖有序交易哈希及签名字节；交易持不可变引用，frame index 随 canonical prune 回收 |
| 构块与传输 | `engine-types/src/{frame_blocks,payload}.rs` | frame plan 按帧取引用，最后一帧可取前缀；frame description 表达有序帧与前缀，缺失内容补取 |
| 直接执行 | `engine-types/src/{fast_transfer,parallel_transfer}.rs` | sender 依赖分组，直接计算转账状态；账户 read set/批处理减少共享读取和逐笔开销 |
| 输出与下一块 | `engine-types/src/output_shards.rs`、`payload.rs` | 保留 batch maps，以地址索引关联输出，只合并冲突；FrozenShards 供 root 与下一块读取，连续状态组装在封块后完成 |
| 构块结果复用 | `engine-types/src/{built_executions,chain_alias}.rs` | 保留构块执行结果；共识封头改变身份后绑定原结果，避免自己的块再次执行 |
| 同机共享执行 | `bin/n42/src/{import_once,payload_serve}.rs` | 同一执行实例内，多验证者的同块请求由一个 owner 处理，其余共享 checked/final 结果；覆盖 own/compact/foreign/payload 各入口 |
| 封块与共识 | `engine-types/src/{payload,post_seal,fields_at_seal}.rs`、`docs/PHASE_D_DEFERRED_EXECUTION.md` | 区分执行结束、输出可读、封块、投票/QC、字段发布与持久化；拆解 seal 后到下一块启动的等待 |
| 状态承诺 | `qmdb-state/src/forest.rs`、`qmdb-reth/src/{read_view,state_reader}.rs` | 精确父版本账户读取、QMDB bin 树、undo/分叉切换与持久化 |

Reth/revm 可作为内部实现组件；产品和测试对象是上述 N42 原生链路。
frame tree 是交易承诺规则，QMDB bin 树是状态承诺，两者分别验证。
参考 frameBlocks 受链配置控制；当前实现也保留非帧体规则，不能把任意 body 的 root
无条件替换为 frame tree。认证帧不足配置门槛时仍须验证交易，不能信任声明 sender。

## 本仓库必须先跨过的边界

本仓库已有 `Gov5QmdbStateRootStore → QmdbLeafTree`，以及直接转账和 sender 分组执行。
这些是已有基础，不重复把它们包装为新优化。

当前 `crates/n42-node/src/node.rs`、`parallel_payload.rs` 等仍沿用 Ethereum primitives，
交易类型主要是 `TransactionSigned`；`ingest.rs` 的受信任预恢复 TCP 批次并不等同于
参考的 N42 原生认证帧队列。第一步应打通原生类型、认证帧和实际出块路径，
不能通过打开开关或提高批次上限宣称已移植。

现有 `ImportGate` 仅在适配器中排队，不能代表同机共享执行。
要共享的是原生构块/执行产物、父状态视图、root 和最终结果，而不是缓存外部请求声明。

## 正确路径与实施顺序

实时 N42 交易 → Ed25519/帧认证 → 有界帧队列 → frame plan → 直接转账执行
→ 冻结分片输出 → QMDB bin root/下一块读取 → N42 封头、投票与 QC → 提交与持久化。
同机验证者各自签名投票，对同一块共享一次执行产物。

| 批次 | 实施内容 | 验收 |
| --- | --- | --- |
| 1：原生类型与帧贯通 | 借用 N42TxEnvelope/Block/Receipt、0x50 Ed25519 编解码与认证；让交易类型贯穿入队、队列、构块、直接执行、封头和读取 | 官方仓库内测试向量、跨实现 hash/sender/signature、畸形输入；实时发送原生交易得到可读取的成功块与回执 |
| 2：帧引用与背压 | 入队一次计算 root；帧内引用贯穿 frame plan 和传输；计数、字节和 in-flight frame 信用独立有界 | nonce 缺口、帧前缀、重复/乱序、缺帧补取、断连释放；队列/RSS 有界；记录复制字节与查找次数 |
| 3：一次执行与共享结果 | 移植 built_executions、封头身份绑定与 import_once 的统一 owner；共享结果跨所有验证者入口，换主继续使用已发布输出 | 同块并发请求执行一次，own_executed_again=0；取消接管、失败重试、分叉和淘汰不产生重复 owner；各验证者 QC 与 canonical 身份一致 |
| 4：账户读取与直接批处理 | 精确父 QMDB 视图；比较原读取、按块 read set 和紧凑账户数组；直接计算余额/nonce/费用，减少逐笔 map/result 分配 | 相同原生交易与父状态的状态/gas/回执等价；共享收款人、sender 交叉依赖、自转账、溢出与失败次序测试 |
| 5：输出分片直读 | 保留 batch output，按地址建 index，只合并冲突；root 和下一块 provider 直接读 FrozenShards | bin root、账户/slot、回执、undo 等价；测 freeze/index/conflict、下一块等待和内存；不把分片重新合成大 map 放回关键路径 |
| 6：封块和共识流水 | 实测执行结束→输出可读→封块→QC→下一块启动；按参考拆开等待和后台字段计算 | 同步/延后字段语义与 N42 链协议一致；换主、超时、延迟输出、失败恢复测试；提交 TPS 与尾延迟同时改善 |
| 7：存储深入优化 | 批量 QMDB 操作/哈希、受影响祖先折叠、WAL 批次与后台压缩；以大状态和持续写入选方案 | durable TPS、写放大、IO/tx、fsync、compaction backlog；重启、undo 与证明根一致 |

不预设参考所有开关都更快。帧大小、线程数、read set、dense id、索引构建和后台任务池
分别做对照；按真实关键路径选择下一段。逻辑和物理 CPU、NUMA、内存局部性也纳入测量。
封头/字段规则变化先在具有明确 N42 协议配置的测试链验证，再考虑同步到目标链。

## 测试和数据闭环

- 首个端到端基线：同一计算机，七个 N42 验证者，共享一个原生执行实例；实时发送
  N42 Ed25519 交易，使用足够覆盖测量窗口的输入流。报告 offered、admitted、
  common canonical 和 durable 交易数，不把入队率当吞吐。
- 每批先做模块/集成正确性，再冻结二进制、配置、输入和父状态，运行 warmup/A1/B/A2；
  重编译后的首轮不计收益。先 60 秒，之后 5–10 分钟稳定性轮；重复多组，检查两端漂移。
- 每块采集：入队、frame select、账户读取、直接执行、输出冻结、QMDB root、封块、QC、
  下一块启动和 durable 时间；同时采集执行次数、构块复用次数、bytes/tx、CPU、RSS、
  major faults、队列字节与笔数。缺失阶段明确标记，不按零处理。
- 以各验证者共同提交、N42 原生头/回执与 QMDB bin root 一致作为正确性门槛；
  共享执行的收益必须同时有“每块一次执行”计数与 canonical/durable 完成数据。
- 转账正确性可使用现有解释器做局部差分，性能主基准直接走 N42 原生执行。
  通用 Engine payload 转换和单 sender 热缓存测量不代表这一主线容量。
- 重型任务遵守共享机器的 BOX-CLAIM 协议。已完成的旧任务不重新排队；下一批队列
  只安排本计划中的原生路径测试。

## 前轮结果归档

前轮排队任务已完成并释放机器：converter 库筛选测试两版各 5 项通过，隔离测试
30 项通过，执行库 129 项通过。原 E0382 编译错误修复已得到该冻结快照验证。

旧微基准：100k 热缓存转账解释器 A1/A2 中位约 65.16/65.17 ms，直接路径 34.33 ms；
220k legacy payload 转换 A1/A2 约 185.90/185.67 ms，候选 109.92 ms。
这些结果来自 `.artifacts/gov5-validator-20261007/`，仅归档各自局部范围，
不作为 N42 原生实时链的容量或架构迁移完成依据。

此次按用户纠正撤销前轮新增 witness 重执行计时，保留此前已有代码和其他未提交改动。
