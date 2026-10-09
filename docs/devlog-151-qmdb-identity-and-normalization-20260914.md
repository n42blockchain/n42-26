# QMDB 缓存身份冲突修复与 Gov5 规范化路径减负

日期：2026-09-14，接续 [devlog-150](devlog-150-qmdb-runtime-qualification-20260914.md)。
**4 节点 1M TPS 尚未实测达标。** 本轮保留 H2、二叉树 QMDB 读取和承诺，
先修复执行结果复用的身份检查，再删除一处确定无消费者的序列化。

## 已复现的缓存冲突

`compute_and_commit` 原有缓存检查比较 parent、root 和 operations。存在同 hash
但不同操作时，它返回 `RootMismatch`，其中 `got` 是已经存储的旧根，而不是这次
操作实际算出的根。`Gov5QmdbStateRootJob::finish` 将所有 `RootMismatch.got`
交还 reth，让 reth 与区块头比较。若调用方仍携带旧的头部根，二者相等，冲突就被掩盖。

先添加真实 state-root job 回归：同一头部第一次执行空 bundle 成功，第二次换成
增加账户余额的 bundle，要求报错。修改前该断言失败，`finish` 实际返回成功；
日志保存在 [before.log](benchmarks/20260914-qmdb-identity/before.log)。
这是错误关联执行结果的本地回归，不是已证明的远程攻击或完整 Engine API 攻击演示。

现在缓存身份冲突返回独立 `ConflictingBlock`，通过 `ProviderError` 拒绝继续，
不再作为“新计算出的根”交还 reth。只有真正计算出的不同根仍走原有的
`BodyStateRootDiff` 比较路径。重新提交认证 base 的 hash 也被拒绝，避免把 base
当作新子块插入分支图。

身份比较检查完整 parent、root 和操作内容。操作枚举顺序不是身份：bundle 的
HashMap 顺序可能改变，而 QMDB 在应用前按 key 排序。因此相同 Vec 直接比较；
顺序不同但长度相同的重试排序借用的引用后比较完整内容，不复制账户/槽 value。
修改 key/value、删除标记、数量、重复 key 或 parent/root 的请求仍拒绝。
不改变已有 WAL 格式或操作记录顺序，旧的非排序记录在重启后仍可正确重试。

持久化回归同时确认：冲突和合法幂等重试都不追加 WAL、不更改树位置或替换固定
读视图；重新打开后，变更操作枚举顺序的同一块仍正确命中。

## 删除无消费者的执行结果编码

核对 `take_gov5_normalization` 到 `handle_built_payload` 的完整调用链后确认：

1. builder 的 broadcast cache 按原始完整 block hash 取出执行结果。
2. 适配器计算 QMDB 根和 Gov5 原生 receipt 根，并把完整执行结果 JSON 序列化、压缩。
3. 调用方仅使用两个根，把第三项绑定为 `_execution_output`，立即丢弃。
4. Gov5 H2 participant 的广播路径本来就不发送 Rust compact execution blob；
   normalized payload 仍经 `new_payload_for` 重新执行、验证，再参与后续流程。

现在返回值改为两个根，删掉第 2 步的 JSON/压缩及其临时缓冲。通用 compact-block
路径继续使用原来的序列化函数。没有把原始 builder 执行缓存挪到规范化后的 hash，
也没有省略 QMDB 候选计算、receipt 根计算、normalized payload 的重新执行或 WAL 同步。
这是对本周上游 sibling/错误缓存身份教训的具体应用：先保证身份和执行路径正确，
不能把同 parent 或旧执行输出直接当作新头部的认证结果。

新增非空账户状态与 receipt 的 adapter 回归，使用独立 QMDB 树验证返回根，
确认 sibling hash 不能消费结果、正确 hash 只消费一次，以及 candidate 定价不会
发布区块或改变父状态。通用 compact 编码/解码、交易根和按 hash 淘汰的回归继续通过。

## 验证与边界

- QMDB 节点测试：42 passed、5 ignored（显式测量）。包括四项新增身份/执行 job 回归。
- execution-cache 测试：2 passed，包括规范化与原有 compact round trip。
- 共识服务 Gov5 不广播 builder compact output 的既有回归：1 passed。
- workspace all-targets clippy `-D warnings` 通过；干净依赖环境沿用固定、补丁化的 reth。
- `git diff --check` 通过。日志与源文件哈希保存在
  [证据目录](benchmarks/20260914-qmdb-identity/)。

本轮没有测量移除编码带来的端到端收益，不提供未经测量的 TPS 或加速比。
候选 apply/undo 与后续重新执行、commit 之间仍有重复工作；消除它需要精确绑定
父状态、完整操作和候选生命周期，并覆盖被其他分支/历史读取打断、WAL 失败、重试。
当前不会用未经验证的推测状态复用换取数字。

真实四节点网络、多窗口持续吞吐和重启追赶仍待验证；沙箱 socket 限制未解除。
本轮证据支持上述局部修复，不构成 4 节点 1M TPS 达标证明。
