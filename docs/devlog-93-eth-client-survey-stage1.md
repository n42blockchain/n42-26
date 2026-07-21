# devlog-93 以太坊客户端近三月更新调研 + 借鉴评估 + 阶段一执行

> 日期：2026-06-22
> 范围：调研 reth / erigon / geth 最近三个月（2026-03 ~ 2026-06）的更新，评估对 n42-26 的借鉴价值、效果与工作量，制定计划并执行「阶段一」。

## 1. 背景与目标

定期跟踪上游执行客户端，把可借鉴的工程成果有选择地引入自研组件（JMT 状态树、Block-STM 并行 EVM、HotStuff-2 共识、手机无状态验证）。本轮聚焦：哪些更新值得借鉴、效果/工作量如何、先做什么。

**关键前提**：我们的 reth fork 基线是 `chore/merge-upstream-paradigmxyz-latest`（reth 2.3 @ `0655e7a9a`，2026-06-10 merge）。因此 **reth 2.0→2.3 的成果我们已通过 fork 获得**，但它们只作用于 reth 自身的执行/MPT/存储路径；自研组件不会自动受益。真正的借鉴工作 = 把思路移植到自研组件，或启用 fork 已带、默认关闭的开关。

## 2. 三客户端近三月更新摘要

| 客户端 | 版本/时间 | 与我们相关的要点 |
|---|---|---|
| reth | 2.0(4/8)→2.1(4/20)→2.2(4/30)→2.3(6/10) | Sparse Trie Cache（状态根 1–2ms）、ArenaParallelSparseTrie、Storage V2（磁盘 -30~50%）、engine backpressure、EIP-7928 BAL 并行执行+prewarming（默认关）、吞吐 1.4→1.5 Ggas/s |
| erigon | 3.4 "Splashing Saga" | Chaindata 缩 4x(~20GB)、Caplin 历史下载持久化、historical `eth_getProof` 转正、EIP-8024 新 opcode(SWAPN/DUPN/EXCHANGE)、并行执行修 BAL workload |
| geth | 1.17.x | CVE-2026-26313/26314/26315（devp2p DoS）、`eth_sendRawTransactionSync`、`debug_executionWitness` 数据损坏修复、`prune-history` |
| 生态 | Ress / EIP-7928 | Ress=无状态 reth（<4K 行，14GB，witnessed EVM）；BAL 实测区块处理 -42%、存储读 -69%，平均 BAL ~70KiB；revm 产 witness 与 geth 不兼容（OP system caller） |

## 3. 借鉴清单与评估

| # | 借鉴点 | 对应组件 | 效果 | 工作量 | 优先级 |
|---|---|---|---|---|---|
| 1 | geth CVE + witness 损坏教训 | n42-execution / n42-mobile | 正确性/安全 | 小 | P0 |
| 2 | Storage V2 迁移（fork 已带） | reth 存储 | 磁盘 -30~50% | 小 | P0 |
| 3 | EIP-7928 BAL → 并行 EVM 预热 + 手机并行验证调度 | n42-parallel-evm / n42-mobile | 并行度、主网对齐 | 中 | P1 |
| 4 | EIP-8024 新 opcode | revm/reth | EVM 兼容性 | 小（随 fork bump） | P1 |
| 5 | engine backpressure（fork 已带） | n42-consensus-service | 缓解 reth 落后 | 小–中 | P2 |
| 6 | ArenaParallelSparseTrie arena 技巧 | n42-jmt | 微优化 | 中 | P2 |
| 7 | Erigon Chaindata 压缩 / Caplin 历史持久化 | 存储/共识 | IDC 存储/恢复 | 中 | P3 观望 |
| 8 | `eth_sendRawTransactionSync` | n42-node RPC | 手机确认 UX | 小 | P3 选做 |

**诚实提醒（避免重复踩坑）**：
- #5 背压：[[devlog-87-el-backpressure-scheduler]] 已证 leader 单边背压 no-gain 且治标更差，默认 OFF。reth 落后只在合成超高 TPS 出现，远超 8s-slot 生产需求（见 high-tps-investigation）。**非数据驱动不再投入**。
- #6 JMT：`ShardedJmt` 已实现跨块内存保留 + snapshot + 16 分片，reth Sparse Trie Cache 的核心思路我们已具备，arena 仅微调。

## 4. 阶段一执行（本轮完成）

### 4.1 CVE / witness 安全核对 —— 结论：全部不适用

| 项 | 性质 | 对 n42-26 |
|---|---|---|
| CVE-2026-26313 / 26315 | geth devp2p 消息致高内存（CWE-770） | 不适用——执行层是 reth(Rust)无 geth 代码；网络层 libp2p+QUIC 不用 devp2p |
| CVE-2026-26314 | geth 特制消息致崩溃（CWE-20） | 同上 |
| revm/geth witness 不兼容（OP system caller） | Optimism 特有 + 跨实现 | 不适用——生成端 `ReadLogDatabase` 与验证端 `StreamReplayDB` 是同一 revm 闭环，且非 OP 链 |

DoS 根因（p2p 消息无大小/资源上限）对应防护我们已有：`read_log` 有 `MAX_ENTRY_COUNT=500_000`、wire 解码全程边界检查、zstd 解压 16MiB 上限。

### 4.2 witness 对抗性回归测试 —— 已落地并通过

现有 V2 正向往返（`crates/n42-node/tests/stream_v2_pipeline.rs`）已覆盖执行→生成→wire→zstd→验证。本轮补**对抗面**（固化 geth witness 损坏教训：损坏/篡改的 witness 必须被手机检测，绝不静默通过）：

- `test_adversarial_truncated_read_log_detected` —— 截断 read log → 重放数据耗尽
- `test_adversarial_injected_read_log_entry_detected` —— 头部插入额外 entry → 严格顺序重放错位
- `test_adversarial_corrupted_code_hash_detected` —— 篡改合约 code_hash → `code_by_hash` 找不到字节码

断言：篡改后手机重放要么报错、要么算出与诚实 `receipts_root` 不同的根。`cargo test -p n42-node --test stream_v2_pipeline`：**9 passed（6 原有 + 3 新），clippy 无新警告**。

**踩坑与修正（重要洞察）**：初版还写了「篡改 account balance」「篡改 storage 值」两个测试，均失败：
- balance 篡改后 `receipts_root` **不变** —— 不影响执行结果的状态值本就不改变 receipts。
- storage 篡改报「无非零 storage 读」—— 最小 ERC-20 测试合约的槽算法与重放读到的值不匹配（SLOAD 读到 0）。

由此明确 **V2 验证边界**：手机重放只锚定 `receipts_root`（gas/logs/success）+ 严格顺序重放的结构完整性。**不改变执行的纯状态值由 state_root 锚定，不在 receipts_root 路径**。测试注释已据此修正，避免误导后人断言一个不成立的安全属性。

### 4.3 Storage V2 迁移可达性 —— 已核对

- `bin/n42-node/src/main.rs:33,313` 用 `reth_ethereum_cli::Cli::parse().run(...)`，**节点二进制透传 reth 标准 CLI**，自动继承 reth 2.3 全部 `db` 子命令，无需写代码。
- reth fork 提供 `reth db migrate-v2`（`crates/cli/commands/src/db/migrate_v2.rs`、`db/mod.rs:82-83`）。
- `--storage.v2` flag **默认 true**（`crates/cli/commands/src/common.rs:79`）→ **新建 datadir 的节点已默认 Storage V2**。`testnet.sh --clean` 起的节点很可能已在用 V2。
- 已有 V1 数据：`<n42-node 二进制> db migrate-v2` 原地迁移，无需 resync。

**待办（需 WSL2 环境）**：实跑量化磁盘节省需要一个 V1 老数据节点对比，留待 WSL 测试网执行。

## 5. 阶段完成状态

- [x] 调研 reth/erigon/geth 近三月更新
- [x] 借鉴清单 + 效果/工作量评估 + 优先级
- [x] 阶段一-CVE/witness 安全核对（结论：不适用）
- [x] 阶段一-witness 对抗回归测试（3 个，全绿）
- [x] 阶段一-Storage V2 可达性核对（透传 reth CLI，新节点已默认 V2）
- [ ] 阶段一-Storage V2 实跑磁盘节省量化（需 WSL2）

## 6. 后续计划

- **阶段二（P1 主线）**：EIP-7928 BAL 集成 —— leader 执行产出访问集打包进 block，follower/手机据此并行预读预热，复用 reth fork 已有 BAL store/wire/payload 抽象；先基准（并行验收率、手机验证延迟）再决定默认开关。跟随下次 fork bump 拿 EIP-8024 opcodes 并补兼容性测试。
- **阶段三（P2/P3 数据驱动）**：仅当压测复现出生产相关的 reth 落后尾延迟，才评估 engine backpressure（用 reth 基于内存水位的模型，而非单边减速）；其余按需选做。

## 7. 阶段二结论：EIP-7928 BAL 集成 = no-go（2026-06-22）

深入调研 reth fork 的 BAL 实现（`alloy-eip7928` 全套类型 + revm `State::with_bal_builder()` 生成 + engine-tree 内部并行执行/prewarming）与 n42-26 自身流水线后，结论是 **不集成 BAL**。三个落点全部归零：

1. **手机侧——倒退**。n42-26 手机验证（`read_log` + `StreamReplayDB`）已超越 BAL：把"输入世界状态变成有序数据流"，利用"同块同交易→同 DB 调用顺序"的确定性，**不传 key、不传 post_value**，1 字节 header 变长去零去重，零分配单 pass 重放。BAL 的额外结构（key/post_value/tx 索引/account 分组）全为**并行执行**服务；手机低能耗、不并行（见架构原则），这些是纯开销（更大流量+更多解析=更耗电）。引入 BAL 是用更重的并行格式替换更优的低能耗格式。
2. **follower/IDC 侧——无需自集成**。follower 走 compact-block 缓存命中跳过执行；真要并行重执行，reth fork **自带 BAL 并行执行路径**（默认配置），启用即可。
3. **格式对齐——无需求**。自定义链 + 自研手机协议，与主网 BAL 无对齐需求。

**架构原则（本轮确立，已存记忆）**：手机 = 低能耗 + 横向抽样扩展，**不在共识关键路径**，**绝不纵向并行执行**（并行=多核满载=耗电发热，违背手机参与者根本利益）；扩展靠数量横向分摊，不靠单机性能。任何"手机侧优化"方向必须是降低单机能耗/流量，而非提速。

**元教训**：与阶段一 backpressure 被数据否决同类——深入自身架构后，上游"先进特性"在我们的设计目标（手机低能耗）下反而是退步。**借鉴前先问"现有方案在自己的目标函数上是否已更优"**。本轮三客户端调研的净产出 = 阶段一安全核对+对抗测试已交付、Storage V2 确认开箱可用、BAL 与 backpressure 经评估否决（负结论同样是价值）。
