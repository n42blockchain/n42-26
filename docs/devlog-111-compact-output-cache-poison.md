# devlog-111 — Compact execution-output 缓存投毒修复（v0.5.1）

## 背景

v0.5.0 合并后审计确认 HIGH-1：`BlockDataBroadcast.execution_output` 是未签名的独立字段。
攻击者可保留诚实 payload（和声明的 block hash）而替换 compact execution-output；reth 会以
state-root mismatch 拒绝该次 `new_payload`，但该拒绝描述的是被注入的字节，不能证明声明 hash
对应的区块本身无效。

原 S5 坏块缓存只过滤了 payload 重新计算出的 block-hash mismatch。它仍会把上述 state-root
verdict 写入缓存，导致诚实区块 hash 被 `should_skip` 丢弃，阻断后续直推与 sync 自愈。

## 修复

- follower eager import、`import_and_notify` 与 syncing retry 均记录本次是否实际注入 compact
  output。
- 若注入后 `new_payload` 非 `Valid`，先 evict 注入项，但**不**调用 `insert_if_invalid`；该 hash
  保持可重试。
- 未注入的路径保持原 S5 行为：reth 对确定性 state-root/EVM 无效块的 verdict 仍会进入坏块缓存。
- leader eager path 只消费本地构建的缓存输出而不注入 peer 字节，因此仍保留确定性无效缓存行为。

该变更不会放过无 compact-output 的真坏块。带 peer compact output 的真坏块最多多一次执行/重试，
代价有界且优先保证不会将诚实 hash 永久拉黑。

## LOW-1

`import_and_notify` 在 S5 `should_skip` 早退前调用
`discard_unvalidated_sidecar_diff(view, hash)`，使已暂存的 sidecar diff 与其它拒绝路径保持相同的
清理不变量。

## 回归覆盖

新增测试覆盖：诚实 payload + 伪造 compact output → 模拟 reth state-root mismatch → 注入缓存被
evict、声明 hash 不进入坏块缓存；随后同一 hash 的无 compact-output 诚实广播可 `Valid` 导入并推进
head。既有 non-inject state-root-invalid 测试继续验证第二次到达由 S5 跳过、不会重复提交给
`new_payload`。

## 后续

纵深防御应为 BlockDataBroadcast 的 payload 与 execution_output 加 leader 签名认证；本次 v0.5.1
先在接收端消除坏块缓存投毒。
