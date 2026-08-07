# devlog-124 — Gov5 H2 Engine header profile

日期：2026-07-21

分支：`feat/gov5-n42-live-interop`

基线：`main @ f904813`、Reth `2.4.1 @ c533db8`

## 目标与安全边界

让已认证的 replay-v2 finalized range 能无损重建 gov5 历史块的 Engine payload 身份，
但仍不提交 `newPayload`、不绕过执行后 state/receipt root 校验，也不开放验证者投票。
现有七节点 datadir、QMDB checkpoint 和 range 文件全部只读。

## 实现

- 增加显式 `Gov5H2` header profile，覆盖真实历史中的三项非标准哈希语义：空 ommers
  承诺为全零、当前 difficulty 0（兼容既有 replay-v2 历史的 1），以及超过 Ethereum 32-byte 限制的 `N42H` H2
  extra-data。
- `N42H` 只接受 `magic + little-endian view` 的最小结构，最大 4096 bytes；错误 magic、
  过短、超长、非零 ommers 或非 1 difficulty 均 fail closed。QC/commit seal 的密码学验证
  仍由已完成的 H2-v4 finality verifier 负责。
- Reth 标准 payload/header 校验前只在临时副本中规范化为 Ethereum 表示，之后恢复 gov5
  字段并重算、核对原始 block hash。默认 Ethereum profile 不变。
- profile 由 `N42_GOV5_HEADER_PROFILE` 显式启用，要求绑定 gov5 genesis，且启动门禁继续限制
  为 observer mode；validator 模式会直接拒绝启动。

## 真实数据验收

- runtime-01 原七节点 block `771-898`：成功生成 128 个 Engine payload 计划，head 保持
  `663a60f7...80aa`。
- runtime-02 自定义七节点 block `1-49`：成功生成 49 个 Engine payload 计划，覆盖 247
  笔交易，genesis parent 保持 `b71c2810...92ec`，head 保持 `7348ff64...9b00`。

这证明 header/body/transaction/receipt 输入已经可以无损跨过 Engine 编码边界，不代表
Reth 已成为 execution follower。

## 剩余阻塞

Reth 执行仍计算 Ethereum MPT state root 与 receipt trie root，而 gov5 replay-v2 使用 QMDB
state commitment 和 native compact receipt root。下一步必须在不跳过确定性执行验证的前提
下接入 QMDB state commitment/ancestor state，并对拍连续执行结果；在至少 1000 块通过前
继续保持 observer-only。
