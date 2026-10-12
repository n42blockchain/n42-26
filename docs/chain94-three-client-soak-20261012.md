# Gov5 replay v2 基线：三工程七节点持续运行验收

日期：2026-10-12。状态：进行中，尚未验收通过。
路线归属：[自定义链兼容主线](04-Product-Lines-and-Performance.md)，交易为 ETH ECDSA（secp256k1），共识 H2，状态承诺 QMDB bin。

## 范围与基线

检查 n42-26、N42-gov5、n42-rs 的共同协议与启动路径，基于 Gov5 replay v2 输出继续实时出块。
历史数据只读导出，持续运行使用独立数据目录和 loopback 端口。
同一链的隔离分叉不连接到原舰队；不重置或复用原进程的可写数据目录。

初步观察：

- n42-26：`f6eeb0d`，本任务开始时工作区干净。
- Gov5：`4533358f`，工作区包含他人未提交改动，源码差异保存于任务 manifest。
- n42-rs：`ef44403e4`，实际源码身份和差异保存于任务 manifest。
- replay v2：`/data/blockchain/qs-replay-linux`；源高度 13,612,974，输出高度 13,652,362，
  checkpoint hash `0x7b3f3a8f7d3cc5c953fa1375c6ebd5db26ffb1a0d23fe07ae64ec34a518f7924`。
- 增量 replay_stats：297,548 笔重执行，skip/failed 为零，receiptMismatch 为 3。
  该字段是历史源与重执行比较；不能直接归为实时舰队失败，也不能忽略其成因。
- 原 2026-09-04 Rust 七节点快照高度 13,560,375，早于本次 replay 输出，不能直接充当本次验收。

材料目录：`.artifacts/chain94-soak-20261012/`；已保存三个仓库身份、dirty 状态及独占运行记录。

## 执行阶段

1. 只读导出同一 replay 输出的 state JSONL、完整 header、QMDB leaf-form；校验 checkpoint/hash/root/链 ID/genesis 和工件摘要。
2. 审计三个工程的交易、header/QC 签名域、委员会、费用/奖励/回执、QMDB 编码、父状态读取和恢复逻辑。
3. 固定源码/依赖及二进制身份，准备隔离七节点运行目录。分别验证三个客户端，再做混合客户端联合验收；拓扑可按用户补充调整。
4. 无负载启动与全 leader 轮换后，加入 ECDSA 转账及失败交易验证；持续运行至少 1 小时且覆盖至少 1,000 个新增提交块。
5. 验证单节点退出/重入；七节点 f=2 下两节点退出仍推进；超过可用 quorum 时停止提交；恢复后追齐。
6. 完整优雅停止并重启，核对持久化 head、QC/vote watermark、QMDB 根、交易回执与历史查询；失败时保留现场并修复回归。
7. 重复出现问题的阶段，并补做完整回归；记录 PASS/FAIL/未执行，报告性能仅作为诊断数据。

## 验收门槛

- 所有节点共同高度的 hash、stateRoot、receiptsRoot、transactionsRoot 一致；QC 经过共同委员会签名校验。
- 七个 leader 实际出块，提交持续推进，无无法解释的停顿、重复执行副作用、拒绝有效块或根漂移。
- 转账余额/nonce/费用与 receipt 状态一致，失败交易不被算为成功；事务顺序和失败边界一致。
- catch-up、掉线重入、整队重启后仍满足一致性与持久化；不丢已提交交易，不回退安全投票状态。
- 无 panic、数据库损坏、QMDB 错误；RSS、FD、线程、磁盘/WAL 和积压有界。
- 每项结果附源码、二进制、配置、起止高度、实际时长、日志和审计输出。

本页将随实测更新。已有历史互通或空块短测记录不替代上述持续运行验收。

## 进展与已发现问题

- QMDB leaf-form 导出通过：输出高度 13,652,362，root
  `0x24fc459b2bdc8da63c9d98c825f047061688e3db8aaeddbb8db3af4d72c00cda`，
  63,541,729 slots / 6,204,334 live entries，2,353,106,566 bytes；重建根与 header 一致。
- 首次账户导出失败：旧 replay v2 数据缺少 QMDBApplied 标记，工具只接受带标记的数据。
  Gov5 修复 `92feb67e` 增加 canonical head fallback，并在同一只读事务重建 QMDB 根后才发布工件。
  新增缺 head、非 canonical head、正常 fallback/marker 和根不一致回归；该包测试通过。
  当前工作分支推送遇远端 non-fast-forward，修复已推送至 `fix/replay-v2-state-export-20261012`。
- 修复后完整账户导出成功：6,191,590 accounts / 12,744 storage slots，与 live entries 总数一致；
  JSONL SHA256 `d0892d4639f6b7f9128a6aa03f50a58933a76c4cae1a0c87ebf0a620cda51d53`。
- 本工程冻结源码 + 适配 Reth 2.7 的 release 节点、bootstrap、init-snapshot 构建通过；Gov5 release 节点构建通过。
- n42-rs 验证者原先无条件启用 H2-v4，而 chain94 配置未启用 interopV4。
  已准备按链配置选择 Gov5Legacy/PoP 签名域的候选，并保持 v4 配置原行为；测试及实机验收待完成。
- 七节点管理脚本离线回归 13 项通过；持续审计工具离线回归 5 项通过。
  `scripts/audit-chain94-soak.py` 当前支持 Gov5/n42-26 RPC，核验新增块、回执根、QC 签名及资源样本；
  n42-rs RPC/验证者证据适配待完成。工具的 PASS 仅覆盖其记录的链/回执/QC 窗口，持久化仍须重启验证。
- Gov5 独立七节点 smoke 已按机器独占协议排队；后续一小时持续运行及故障恢复尚未完成。

### 首轮 Gov5 smoke 现场与审计修正

隔离七节点已从 replay 输出继续出块；正式审计起点为 13,652,378。
审计首次 FAIL：`consensus evidence roster size differs`。所有本轮进程随后优雅退出，数据库与日志保留。
源码核对确认 Gov5 `n42_getConsensusEvidence` 在本配置下返回确定性移动委员会证据，
不能当作七个 IDC 验证者的 H2 CommitQC；`AssembleCE` 的 IDC/移动组合结构未在该实时写入入口使用。

审计改为解析各节点头中嵌入的 `LastCommittedQC`，按链的 legacy 签名域验证聚合签名，
再核对 QC 目标为本地 canonical ancestor。共同比较上限取所有节点 QC 已认证高度的最小值，
逐块检查连续 parent 链与根/回执；QC 作为窗口边界的祖先链认证证据记录，不把移动委员会字段伪装为 IDC QC。
新增 canonical QC 解码、位序、长度、bitmap padding、尾随字节回归；离线 5 项通过。

### 快照恢复与 RPC 后续修复

本工程初始化成功：replay 输出头 13,652,362，6,191,590 accounts / 12,744 slots；
n42-rs 共识回归 286 项通过，验证者 release 构建通过，legacy 配置修复 `429e2cb1a` 已推送。

本工程 bootstrap 原只支持 Gov5 v1 快照；已增加 `N42HSSv2` 解析并保留 R1/R2 持久化投票水位。
v1 无投票水位时关闭已恢复的 current view，避免假设该 view 未投票。长度和尾随数据严格校验。
对应 release 回归 3 项通过，bootstrap release 重建通过。

Gov5 第二轮 smoke 的 IDC CommitQC 验证已通过，随后按 hash 查询回执报不存在。
定位为 `jsonrpc.BlockNumberOrHash` 解码把 Hash 数组按值传给 helper，结果只写入副本、目标仍为零 hash。
已改为指针写入并补字符串、对象参数一致性及非法输入回归；该修复的 Go 测试与第三轮 smoke 待完成。

### 按既有兼容分支复用（用户补充后）

历史 n42-rs `perf/erigon-borrow-20260828` @ `e7d5c0cf7` 记录了真正的七人混合委员会：
Go 0–4、n42-26 为 5、n42-rs 为 6；151 个样本块中 25 个由 n42-rs 生产，多个 Go-led QC 包含 Rust 投票。
原工作树 `/home/n42/src/n42/n42-rs-wt-erigon` 保留完整 chain94 header 扩展、Prague/EOF 规则、
状态 bootstrap 与跨实现签名配置。当前 n42-rs 性能主线缺少其中部分兼容扩展；本次自定义链验收优先复用该现存兼容分支，
不把性能路线的 Ed25519/帧改动引入 ECDSA 兼容链，不重写已有协议。原兼容源码保持只读，使用独立 target 构建。

Gov5 第三轮 smoke：60.000651 秒，共同 QC 认证高度 13,652,387 → 13,652,422（+35），
七个 leader 全覆盖，98 成功 / 2 失败交易回执及累计 gas/回执根跨七节点一致，QC 聚合签名验证通过。
失败交易原因仍须单独核对；本轮 PASS 不代替一小时持续运行、故障与重启验收。

优雅停止后的七节点均持久化到 13,652,426，head/root/QC 一致：
`0x26fd2d7692b4d91a54f2e6982420c04b20241ff1f530ae2e08b2a41740615257`，
root `0x23c4e35e5cb2c898a86a6ddaee660b213ebbf7f96b744c1d6ad71f5d25fc20bc`，
view 73，locked/committed/R1/R2 水位均为 72。该公开 CommitQC 已用离线 verifier 核验。
后续 Rust 和混合运行可从此 replay 派生、已认证的短程继续点开始，保持共同数据和委员会；不伪造历史 QC。

旧 n42-rs 兼容分支 release 节点、init-snapshot、验证者构建通过；H2 共识/网络/执行与 QMDB 相关回归共 328 项通过。
已从 Gov5 停止后的 13,652,426 导出新的配对工件；n42-26 和旧 n42-rs 均初始化成功，
导入 6,191,631 accounts / 12,969 slots，QMDB leaf-form 重建根通过。
本工程沿用 `scripts/chain94-fleet.py` 和原 `consensus.json`（slot 3000 ms、epoch 200、f=2）准备七节点，
bootstrap 使用真实 v2 状态和验证过的 CommitQC；各节点可写数据目录独立。

持续审计新增读取 n42-rs 原有公开 checkpoint 的适配，不改该工程协议或检查点布局；
正确区分 Rust checkpoint 的 Msb0 bitmap 与 Gov5 wire 的 Lsb0 bitmap。工具离线回归现为 6 项通过。

### 三工程独立短测与长测启动

- n42-26：90.000803 秒，共同认证高度 13,652,433 → 13,652,462（+29），
  210 成功 / 0 失败，七个 leader 全覆盖，七份独立状态使用 QMDB `only` 读取，根/回执/QC 审计通过。
  本轮发送器实际为短时突发，不能当作持续负载验收。证据：`n42-26-smoke-audit/summary.json`。
- n42-rs 既有兼容分支：90.000671 秒，13,652,433 → 13,652,460（+27），
  164 成功 / 0 失败，七个 leader 全覆盖；沿用七份独立执行实例，25/27 个新增块包含负载。
  采用修正后的 2 TPS 持续发送，根/回执/QC 审计通过。证据：`rs-compat-fleet/smoke2-audit/summary.json`。
- Gov5 `44592a05` 修复低速发送器每 10 ms 至少放行一笔导致低速设置实际变成 100 TPS 的问题；
  保留分数 tick 和 batch credit，低速/高速及 batch 回归通过，已单独提交推送。
- Gov5 已从保存的七份状态恢复，启动 3,600 秒、至少 1,000 个新增认证块的长测。
  结果持续写入 `gov5/soak-audit/samples.jsonl`；运行中数据不作为最终 PASS。

Gov5 短测两笔失败已定位为块 13,652,396 中对同一 ERC20 合约的 `transfer` 调用。
`debug_traceTransaction` 均返回 `execution reverted`，使用 gas 分别为 23,744 和 23,756；
不是用尽 gas。七节点失败状态和回执根一致。具体余额/合约条件仍需核对，尚不宣称原因完全闭环。
交易、回执和 trace 已保存于 `gov5/failed-smoke-transactions.json`，不包含私钥。

一小时运行、节点退出/恢复、整队重启及混合委员会互通仍待完成。

### Gov5 短测失败归因闭环

在后续持续运行期间，对块 13,652,396 两笔失败调用补充真实交易执行前的
`prestateTracer`。测试 ERC20 的余额 slot 为发送者地址，
两发送者余额均为零，转出数量分别为 811 和 307；调用 trace 均为 `execution reverted`。
与该合约的余额不足分支一致。负载生成器把 seed 提交完成标记为已 seeded，
普通账号可能在自己的 token seed 实际执行前发送 token 转账。
这是有效交易的合约回退；不属于执行根或节点一致性错误。
公开 prestate／交易／receipt／call trace 已配对保存。

正在进行的 Reth 稳定适配见 [两工程补全记录](reth-stable-completion-20261012.md)。

### Gov5 一小时持续运行结果

3600.000613 秒长测 PASS：共同 QC 认证窗口 13,652,424 → 13,654,225（+1,801），
4,481 笔成功 / 0 失败，七个 leader 全覆盖，块身份、parent、状态／交易／回执根、
回执身份与累计 gas 跨七节点核对，嵌入 QC 的聚合签名通过。
该窗口起点来自头中携带的祖先 QC，略早于恢复时数据库 head；
这是认证窗口计数，不把它当作新负载独立吞吐成绩。
证据：`gov5/soak-audit/summary.json`、`samples.jsonl`。

七节点均优雅停止。只读导出的停止状态全部一致：

- head：13,654,229；hash `0x6c410f224600189f6c9c6c71770f2812b7c4ee96fffaa50a092855b61d33a767`。
- QMDB root：`0xe3a2548fc9eadd81a5f7ad2c30dacaaa41e99ff69feff530401b4136041ffc01`。
- view：1,877；R1 / R2 持久化水位：1,876。
- 各节点 CommitQC 与各自 head 匹配，7 份 QC 均经既定委员会和 legacy 签名域独立验证。

停止状态保存在 `gov5/node*/soak-anchor.json` 与 `gov5/soak-stopped-state.json`；
保留早先 13,652,426 的 `anchor.json` 和配对导出工件，未覆盖原继续点。
这轮通过一小时健康运行与停止后状态检查，完整重启、掉线恢复和混合互通仍未验收。
