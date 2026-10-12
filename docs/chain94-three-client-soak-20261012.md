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
