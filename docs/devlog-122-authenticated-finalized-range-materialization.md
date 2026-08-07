# devlog-122 — finalized-range 认证后物化

日期：2026-07-21

分支：`feat/gov5-n42-live-interop`

## 目标

为 replay-v2 execution/archive importer 建立无副作用的第一阶段：只有完整 v1 frame 的
Blake3 摘要和所有逐块检查通过后，调用方才能取得 canonical header、transactions 与
receipts。认证完成前不暴露 entry，不调用 Engine API，也不写 follower head。

## 实现

- `decode_finalized_range_stream` 返回受类型约束的 `VerifiedFinalizedRange`；旧的低内存
  `verify_finalized_range_stream` 继续逐块验证并丢弃 payload。
- 物化路径对所有保留的 header/block/compact-receipt 原始字节施加 256 MiB 总上限，独立于
  16 MiB 单 blob 和 128 block wire 上限。
- 完整解码 gov5 的扩展 Ethereum header，拒绝非法字段或尾随 header bytes。
- 从 gov5 block RLP 的第二个字段解码每笔 EIP-2718 transaction，逐块重算 Ethereum ordered
  transaction trie root；body 交易被替换但 header 未变时明确拒绝。
- 认证结果保留已解码 `TxEnvelope` 与 `EthereumReceipt`，后续 importer 不需再次解释未验证
  body bytes。

gov5 replay-v2 的 `stateScheme=qmdb` 会设置 `UseEthereumTxRoot`，所以该 transaction root
正是 `DeriveShaErigon(EthTransactions)`，不是 legacy N42 protobuf-concat root。旧 native
profile 若使用 protobuf transaction root 将 fail closed，不能混入本 importer。

## 真实数据验收

- 原七节点 `771-898`：128 entries 在整帧通过后全部物化，head/state root 保持
  `663a60...80aa` / `76ae42...4a4c`。
- 自定义交易链 `1-49`：49 entries、247 transactions 全部解码并逐块通过 Ethereum tx trie
  root 与 gov5 native receipt root；head/state root 保持 `7348ff...9b00` /
  `be55d1...9f2d`。

## 安全边界与下一步

该提交只建立认证后消费边界，不把 QMDB root 冒充 Reth MPT root，也不启用
`N42_SKIP_STATE_ROOT` / `N42_DEFER_STATE_ROOT`。下一步是把已认证 entries 转换为明确的
execution/archive import plan，并在一次性自定义链 datadir 上验证；标准 Reth 无法验证 QMDB
state root 时必须显式停止，直到 N42-QMDB execution commitment 接入，不能以绕过 root 的方式
制造“跟随成功”。
