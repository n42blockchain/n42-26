# Gov5 replay-v2 跨客户端执行导入闭环

日期：2026-07-22

## 目标

让 Rust `n42-26` 以 observer 身份从 Gov5 自定义链的认证数据重建 block 0 执行基线，逐块执行 replay-v2 的 1–49，并同时验证 Ethereum 执行结果、Gov5 header 身份和位置敏感的 QMDB root。现有七节点、QMDB 历史和高性能数据均保持只读；所有验证使用新建的 `/tmp` 数据目录。

## 认证边界

- 链 ID：1143。
- Gov5 block 0 hash：`b71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec`。
- header 所承诺的原始 genesis QMDB root：`91a450c13f9deab2c9edf5832c96008862e7cc1169599f68461c3ec947099941`。
- replay-v2 在 block 1 执行前追加硬分配和四个系统合约；执行基线 QMDB root：`3c217eb08349ce62ac5ef49bae73c0580353c598721507cb7898abcae00cb508`。
- checkpoint、block 0 finalized-range、配置 genesis 和本地重建的 QMDB 前缀分别验证后才建立执行 store。任一 hash、root、slot、key 或 value 不一致均启动失败。
- finalized-range 必须与 QMDB checkpoint 的结束块、块哈希和 state root 一致；导入时每块必须 `new_payload=Valid`，随后 `head=safe=finalized` 的 forkchoice 也必须 `Valid`。

## 跨客户端语义修复

1. Gov5 H2 replay header 先验证原始外层 hash/parent/number，再转换为 Reth 可执行 header。父子执行验证使用同一归一化身份，外层链路仍由认证的 Gov5 hash 约束。
2. Gov5 H2 是 PoS 执行链。由于源 genesis 没有标准 Paris 字段，Rust 在不改变认证 block 0 header 的前提下显式从 block 0 激活 Paris/TTD=0，避免 Reth 错加每块 2 ETH PoW coinbase reward。
3. replay-v2 的系统合约 overlay 使用 Gov5 实际 consolidation bytecode；尾部字节差异会改变 code hash 和 QMDB 叶，不能用语义近似代码替代。
4. Gov5 会把 journal dirty 账户写入 QMDB，即使该账户最终 `AccountInfo` 与原值相同。Reth 侧改用 `BundleAccount.status` 区分纯读取和 dirty 状态：纯 `Loaded*` 不写，其他状态均写账户操作。block 9 的合约 `610178da211fef7d417bc0e6fed39f05609ad788` 正是此类 no-op dirty write；QMDB 位置敏感，漏掉它会移动后续 slot 并改变 root。

## 真机结果

使用当前 Gov5 重新导出的配对产物，在全新 Rust 数据目录中顺序导入 49 块、247 笔交易：

- block 1–49 全部 `new_payload=Valid` 且 finalized forkchoice 成功；
- RPC head：49（`0x31`）；
- head hash：`765e220ce23ff55715d75bb5d51d5e193d189c6bf88e6f4d29e7b00063cd93f8`；
- head QMDB state root：`6fb33357c8db5eb206f506af271cf5fff885fc11bbd82b405b74a42943c98314`；
- Rust RPC 返回值与 Gov5 checkpoint/finalized-range 完全一致。

旧测试产物中 block 1 的 parent 为零，不能代表连续 canonical chain；本轮使用 Gov5 当前代码重新生成、互相锚定的 QMDB 与 finalized-range，未修改旧文件。

## 接入等级

当前能力达到“认证历史执行 + observer head 恢复”，可以使用全新数据目录接入现有七节点做只读跟随。还不开放 validator 投票：参与共识前至少要完成跨进程实时追块、重连/catch-up、连续 1000+ 块最终性一致和故障恢复验收，并确认不会对七节点已有数据目录产生写入。
