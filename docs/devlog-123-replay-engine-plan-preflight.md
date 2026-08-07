# devlog-123 — replay Engine import plan 预检

日期：2026-07-21

分支：`feat/gov5-n42-live-interop`

## 结果

新增 `build_replay_execution_plan`，只接受整帧认证后生成、外部不可伪造字段的
`VerifiedFinalizedRange`。它把 entry 转为 typed Engine API `ExecutionData`，检查 block hash、
parent 与 number，但不调用 `new_payload`，因此不改变 Reth head 或状态。

finalized-range v1 没有携带 ommers、withdrawals、execution requests 和完整 BAL bytes；header
要求其中任一数据时预检 fail closed，不以空值代替。

## 真实数据发现

对两套保留数据运行预检：

- 原七节点 `771-898`；
- 自定义交易链 `1-49`（247 transactions）。

两者 header 的 `ommers_hash` 都是全零，而标准 Ethereum/Reth payload 重建使用
`EMPTY_OMMER_ROOT_HASH=1dcc4d...9347`。因此两套输入都在第一个 entry 明确返回
`UnsupportedOmmersHash`，未进入 Engine API。即使先解决此项，QMDB state root 与 Reth MPT
state root 的 commitment 差异仍会使标准执行路径拒绝。

这说明“接上 H2 网络”与“执行跟随”之间仍缺 N42 execution profile，而不是传输或 JSON
转换小问题。下一实现必须让 Engine block 重建采用 N42 zero-ommers 语义，并把执行后的状态
commitment 接到 gov5-compatible QMDB；不能修改既有七节点 block hash，也不能打开 state-root
bypass。

## 验收

- 当前 gov5 v1 payload profile（无 withdrawals/requests/BAL）被完整解析；
- 任意遗漏的 fork 数据被逐类拒绝；
- gov5 zero-ommers header 有独立回归测试；
- 两套真实 bundle 均在无副作用预检中复现同一兼容性边界。
