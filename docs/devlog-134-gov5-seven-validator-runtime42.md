# devlog-134 — Gov5 最新上游的完整七验证者运行

日期：2026-08-07

分支：`feat/gov5-n42-live-interop`

## 目的

为 Gov5 5.7.905 私网验证 Rust/Reth 与五个 Gov5 节点的完整轮值互操作。完整集合不是
“五 Gov5 + 一个 Rust”：初始验证者集合有七个槽位，Rust 分别占槽位 0 与 6；缺少任一个
Rust 节点都会留下空领导者槽位，不能作为长期共识证据。

## 运行固定项

- Gov5 上游：`8d7f57db2539b323cc863e5a1274bc1b451439e1`；守护进程每十分钟检查，若
  `main` 改变则该窗口无效。
- 创世块：`0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec`；
  七个 HTTP RPC 端点均已直接查询确认。
- 端点：Gov5 `28501..28505`，Rust 槽位 0 为 `29545`，Rust 槽位 6 为 `29546`。
- Gov5 二进制 SHA-256：
  `771f8a41f56483e8f388b25cfaa0b1623e231d9f19d16af4723645fd0c9acf80`。
- N42/Reth 二进制 SHA-256：
  `cfcd41790e04349da825979f3532627ffa9875a0a1e3dc37647daa57ef4707b1`。

## 数据处理规则

第二台 Rust 节点从停止状态下的第一台 Rust 节点复制 **Reth execution 数据与 QMDB consensus
数据的匹配对**，而不是单独复制 execution 数据。这样保留认证的 finalized-range/QMDB
lineage；部分复制会由启动时的 lineage 校验明确拒绝。节点身份仍只由槽位 6 的 BLS 与 P2P
密钥确定，因此数据复制不改变验证者身份。

## 已完成的短期检查点

- 七端点连续同高同 hash，零交易根，10 分钟审计最大滞后为零；
- 两台 Rust 均报告 `validatorCount=7`、已提交 QC 且零 equivocation；
- 每个 Rust 节点均在 71 块、stride 7 的范围按预期贡献 11 个领导者区块，所有七端点及
  parent chain 一致；
- 两台 Rust 资源审计均为单进程，线程数 162、FD 数 93，逻辑计数单调；
- 七个日志均无结构化 `ERROR`、panic、fatal 或 equivocation。启动时
  `error=Duplicate` 是 gossip 去重，不能用全局 error 匹配误判。

## 仍在进行

24 小时头部、资源和 Gov5 上游守护窗口仍在运行。`scripts/gov5-seven-validator-final-verifier.sh`
要求该窗口全部完成后，联合检查二进制、上游、创世、七端点、双 Rust leader 审计、资源审计和
关键日志；在完成文件产生前不得宣称长期验证完成。
