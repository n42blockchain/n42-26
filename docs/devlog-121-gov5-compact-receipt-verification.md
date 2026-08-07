# devlog-121 — gov5 compact receipt 跨客户端验证

日期：2026-07-21

分支：`feat/gov5-n42-live-interop`

## 目标

把 finalized-range 从“携带但不读取 receipt bytes”推进到逐块解码、交易数绑定和
receipt root 确定性验证，为后续 execution import 排除一条未验证输入通道。

## 实现与审计约束

- Rust 实现 gov5 compact receipt decoder，覆盖 legacy、EIP-2930、EIP-1559、EIP-4844
  与 EIP-7702 类型、success、累计 gas、address/topics/data logs。
- 解码器限制每块最多 250,000 receipts、1,000,000 logs、每 log 最多 4 topics；拒绝
  reserved flag、超长 gas、截断和非规范 varint，输入仍受 finalized-range 16 MiB 单
  blob 上限保护。
- verifier 从 canonical block RLP 计算交易数量，要求与解码 receipt 数完全一致，并对
  每块重算 header 声明的 receipt root。
- root 算法明确采用 gov5 replay-v2 native 语义：`keccak256` 连接每个
  `RLP([status,cumulativeGasUsed,logs])`；空列表为 `c5d246...a470`。它不是 Ethereum
  receipt MPT 的 `56e81f...b421`，不得静默互换。
- 没有增加“边验证边导入”回调：v1 frame 只有整帧末尾摘要，在摘要通过前触发有副作用
  的 callback 会形成 TOCTOU/未认证导入。execution importer 必须先完成整帧验证，或在
  v2 使用逐 entry digest/Merkle commitment 后再开放流式消费。

## 真实数据验收

- runtime-01 原七节点 block `771-898`：128 个空块逐块通过 native empty receipt root，
  QMDB/head 锚保持 `663a60...80aa` / `76ae42...4a4c`。
- runtime-02 自定义七节点 block `1-49`：Rust 从 gov5 bundle 解码并绑定 **247 笔交易及
  receipts**，全部中间块 receipt root 通过；首块 parent 正确指向 genesis
  `b71c28...92ec`，head 为 `7348ff...9b00`。

## 下一步

当前数据已经具备 header/body/receipt 的跨语言确定性验证，但仍未推进本地 Reth head。
下一步设计安全的两阶段 execution import（先完整验证，再逐块 newPayload/FCU），同时为
网络 finalized-range request/response 增加逐段认证；完成 1,000 块连续执行前保持
observer-only。
