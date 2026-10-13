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

Gov5 一小时资源汇总已保存于 `gov5/soak-resource-summary.json`。
各节点 FD 最大 22、线程最大 17–18；RSS 从约 3.29–3.32 GiB 升至 4.33–4.40 GiB，
最后十分钟仍有约 0.06–0.12 GiB 范围变化，不能据此认定长期内存有界。
七份日志的 panic/fatal/bad-block/state-root-mismatch/database-corrupt 模式计数均为零；
该模式扫描与链/回执/QC 验证互为补充，不能代替故障恢复验收。


## 升级舰队与混合互通的后续队列

Reth v2.7 兼容分支的新二进制升级短测已通过；数据来自旧兼容舰队的独立副本，
90 秒认证窗口新增 27 块、167 笔成功交易、0 笔失败，七个 leader 均出块。
详细编译／回归门槛及运行证据见 [Reth 稳定版适配补全](reth-stable-completion-20261012.md)。

当前 n42-26 一小时续跑尚在进行。一次进度采样中七节点同高 13,652,918，
已验证 454 个新增块、2,702 笔成功交易、0 笔失败；该采样不能记为一小时 PASS。
机器占用声明下的串行队列为：

1. n42-26 一小时续跑完成并优雅退出。
2. n42-rs 主线完整 workspace 回归；兼容升级七节点一小时续跑。
3. RS、Gov5、n42-26 分别演练单节点与双节点掉线／追块、四在线无 quorum 停顿、整队重启。
4. 停止的 Go 七节点分别离线导出实际提交锚点，验证 head/root 与 QC；
   在新 `mixed27-fleet` 目录导出同一完整状态与 QMDB leaf，分别初始化两个 Rust 节点。
5. Go 0–4 / n42-26 5 / n42-rs 6 固定七成员委员会先做 90 秒短测；通过后续跑一小时。

恢复驱动增加停止时的 R1/R2、view、locked QC 与 committed QC 采集，并在重启运行后
再次停止对比，要求水位不倒退、同 view 的 QC 哈希不改变，两类 QC 均按固定七密钥离线验证。
该驱动补充目前仅做 Python 语法检查，运行结果仍待队列执行。
测试采用优雅进程停止，不能据此声明断电耐久性或 Byzantine 故障通过。

混合初始化沿用既有 secp256k1 传输身份、BLS 委员会、ETH ECDSA 交易、chain94
原 genesis/header 及 QMDB bin 状态根；保留各节点自己的真实签票水位。
不会把三个独立续跑后的分叉数据库直接接成同一委员会。
混合驱动已排队，结果将写入 `mixed27-fleet/{smoke,soak}-audit/summary.json`；
准备、短测或恢复门槛失败时后续阶段拒绝启动，不自动降低 quorum 或绕过状态校验。


## 再次核查：一小时结果与队列失败原因

本工程严格 QMDB-only 七节点一小时续跑已完成：认证窗口
13,652,464 → 13,653,662，新增 1,198 块、7,154 笔成功交易、0 笔失败，
七个 proposer 均出块，区块／状态根／收据／commit QC 一致性通过。
工件：`n42-26-soak-audit/summary.json`。优雅停止后七节点均为 view 1,311，
R1/R2、locked/committed view 均为 1,310，提交哈希
`7281e2f03755ef6c2f4bba0c269f8289fc25459dc26d9be539352386460900db`。

Gov5 一小时结果仍为新增 1,801 块、4,481 笔成功交易、0 失败。
RS 升级短测通过，但其一小时与后续恢复／混合互通尚未执行：
上一队列在性能主线完整 workspace 回归的 sled 重开用例失败，后续门槛全部拒绝启动。
保留 `build-service/007..012.result.json` 原始失败结果，不改写为 PASS。

该 sled 用例原先依赖退出作用域就立即释放后台持有的文件锁；改为独立子进程写入并
flush，进程成功退出后再重开读取 1,024 条记录，测试持久化行为与后台线程生命周期匹配。
修复后的定向用例已通过，完整 workspace 回归重新执行中。
恢复审计另发现 locked QC 应按 prepare 域验证，不能调用 commit-only 验证器；
正在补充显式阶段校验及跨域拒绝回归。此次核查尚不能签署三工程最终验收。


## 三工程独立七节点一小时窗口均通过

升级兼容 n42-rs 的一小时续跑完成，认证窗口 13,652,491 → 13,653,631：
新增 1,140 块、7,194 笔成功交易、0 笔失败，七个 proposer 均出块。
七节点区块／QMDB 根／收据／固定七密钥 commit QC 的连续一致性检查通过。
工件：`rs27-fleet/soak-audit/summary.json`，后台任务 `build-service/020.result.json` 返回 0。
这补齐了 Gov5、本工程与升级兼容 RS 各自一小时窗口；尚未替代正在运行的恢复演练和混合互通。


恢复驱动第一轮在停掉 RS 节点 6 后因临时目录的 `queue.py` 遮蔽 Python 标准库而阻塞，
其余六个节点持续出块。优雅停止自有 supervisor 后，任务 021 以 130 结束，
后续 022–025 门槛拒绝启动；现场完整移至 `rs27-fleet/faults-attempt1-queue-shadow/`。
临时排队脚本改名 `wait-for-validation-claim.py`，同目录标准库解析及两线程 smoke 已通过。
第二轮恢复／混合队列从任务 026 开始；第一轮不能记录为恢复 PASS。


## 恢复审计发现并修复两处 Rust 缺口

第三轮 RS 恢复演练已证明单节点重新追齐：七节点同高 13,653,785，
同根 `92890576c47a3b7a9838a8cbcb697902f83835730b71a03156150e50badc5462`。
随后停掉节点 5、6 时，在连续两任 leader 缺席的 view 上无法推进，故该轮仍为 FAIL。
工件保留于 `rs27-fleet/faults-attempt3-two-offline-leaders/`。

修复分两组逐段推送：

- RS 兼容升级分支启动时读实际 canonical execution head，首次握手通报实际高度；
  若 checkpoint 指向本地尚缺的块，执行驱动从已有执行头继续，保留原 finalized 水位。
  启动读取不制造提交证书或 finality。缺失／未知执行头拒绝启动。
- 本工程、RS 兼容升级及 RS 主线复用 Gov5 现有 TC 推进逻辑：任意持有完整验证超时 quorum
  的节点可本地推进 view；只有下一任 leader 可签名并广播 NewView。固定 n−f quorum、
  超时签名、内嵌 QC、未知 epoch 拒绝及 NewView leader 校验保持原规则。
  新回归在 native / gov5legacy / h2v4 三个域分别验证五签名可越过两任离线 leader、
  四签名不能推进、非 leader 不能伪造 NewView。

任务 038 的回归／构建门槛通过：RS 兼容共识 242 项，RS 主线共识 287 项；
本工程共识单测 245 项，integration 67 项、committee evidence 12 项和其他明确 target 均通过。
两个 Rust 运行二进制已重新冻结，摘要在 `recovery-binaries.sha256`。
第四轮恢复与混合队列从任务 039 开始；前三套独立一小时窗口发生在此次 timeout 修复前，
不能把旧窗口的二进制身份替换成新二进制。


第四轮 RS 演练进一步通过双节点停机继续出块、四在线无 quorum 停顿、恢复追齐，
但整队重启后出现 view 1,565 / 1,566 分裂，未能恢复持续出块；该轮仍为 FAIL，
保留在 `faults-attempt4-full-restart-split-views/`。

补齐 bounded future-report 同步：同一已知未来 view 上收到 f+1 个不同验证者的有效超时报告时，
只跳到该 view 并发布自己的超时，不据此安装 QC、TC 或提交区块；TC 仍须 n−f 个签名。
未知 epoch、窗口外报告、无效签名、重复 sender 和 observer 的限制保持原有拒绝路径。
缓冲证书若已推进到更高 view，不因旧报告对新 view 提前超时。
七节点回归明确验证两份报告不能同步、三份报告只同步 view、四份签名不能形成 TC，
五份才形成 TC，committed/locked QC 不被弱同步替换。

该修复及重建在任务 046 全部通过：RS 兼容共识 243、本工程共识 246 + integration 67、
RS 主线共识 288 项；固定 signer/quorum 和 NewView leader 回归继续通过。
另修复一个单测依赖线程在 1.5ms 内调度的竞态，改为明确的消费完成握手验证输出顺序；
未消费时的有限重试测试保留。修复分开提交，未改生产队列的重试预算。
第五轮恢复／混合队列从任务 047 开始，当前仍待运行验收闭环。


## 2026-10-13：RS 恢复全套 PASS

任务 052 返回 0，`rs27-fleet/faults-drills/summary.json` 为 PASS。
单节点停机继续出块／追齐、双节点停机继续出块、四在线无 quorum 停顿、
恢复 quorum 后追齐以及整队优雅重启后持续出块均完成；各阶段固定七密钥、五签名 QC、
canonical hash、QMDB 根及收据一致性检查通过，停止前后 R1/R2、view 与两类 QC 不倒退。

第五轮在恢复五在线后的早期 60 秒门槛未通过，原失败现场保留于
`faults-attempt5-recovery-budget/`。将“恢复首个新提交”与“恢复后的持续出块”分别验收后：
前者明确限时 180 秒，与一小时 auditor 的 stall 上限相同，后者仍要求 60 秒至少 7 块。
没有改变 quorum、签名或状态根条件，也未把原 60 秒失败改写为 PASS。

实测恢复第五个节点后首个认证新提交耗时 75.403 秒，随后 60 秒新增 8 块；
整队重启在所有节点启动且追齐后约 5.116 秒产生认证新提交。
后一个耗时不包含七个 EL 串行初始化时间，不能当作端到端重启耗时。
详细阶段事件、停止水位及 QC 在 `faults-drills/events.jsonl`。
Gov5、本工程恢复演练及混合委员会运行门槛尚在后续队列，最终综合验收未签署。


## Gov5 恢复验收 PASS；本工程恢复驱动继续修正

Gov5 的第二轮恢复全套 PASS，工件 `gov5/faults-drills/summary.json`。
第一次五在线 60 秒仅新增 5 块的失败现场保留于 `gov5/faults-attempt1-five-node-window/`。
链原配置 period=3s、maxTimeout=30s，两个离线 leader 的轮换可能跨越一个 60 秒窗口；
五在线改为 180 秒至少 7 个认证块，六/七在线的健康窗口保持原门槛，协议参数未更改。
恢复第五个节点后首个新提交耗时约 20.172 秒；整队启动且追齐后约 10.155 秒恢复新提交。
前后 R1/R2、view、锁定与提交 QC 对比通过，没有改变已验证的 Gov5 二进制。

本工程第一轮恢复完成单/双节点停机出块、无 quorum 停顿、恢复追齐，但全队启动时
驱动先等 node0 的 committed RPC 状态才启动 node1..6，因尚无 quorum 而超时。
`n42-26-faults-attempt1-startup-barrier/` 保留原 FAIL。启动屏障改为先检查本地执行 RPC
及已有 snapshot 高度，等七节点全部启动后，仍严格执行 QC/追齐、新提交及持久水位校验。
这项修正只涉及隔离测试驱动，未更改本工程生产恢复逻辑；从任务 061 重试。
