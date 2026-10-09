# QMDB undo 字节预算与认证 base 恢复

日期：2026-09-14，接续 [devlog-151](devlog-151-qmdb-identity-and-normalization-20260914.md)。
**4 节点 1M TPS 尚未实测达标。** 本轮处理持续出块的内存与回退问题，保持 H2、
二叉树 QMDB、原有执行检查和 WAL 同步。

## 两层缓存的实际限制

此前 node 保留最多 8,192 个 undo，不按字节限制。每块 147k 次账户更新时，
undo 里的旧值、slot/key 和 appended keys 会产生较大的线性增长。
另外 core 只缓存 256 个 sealed twig 的叶哈希堆；undo 即使还在，也可能已无法
重新打开旧 twig。node 的注释说较老分支可以从 base 重放，实际代码却直接报
`ReplayDepthExceeded`，没有这个恢复路径。

现在 undo 同时受 **8,192 条 / 256 MiB** 限制。按两类 Vec 的 capacity、元素大小
和旧 value 的 capacity 计费，不只是 len，也不包含 allocator 开销或进程其他内存。
始终保留最新一条，即使它单独超限，保证当前块失败时还有回滚记录。

移动前检查 undo 所需叶堆；缺 undo 或叶堆时，先加载并认证初始 base，再按存储的
完整操作重放目标分支，逐块核对根。原来的 ancestry 深度和缺父块拒绝仍有效。
缓存中的不可变读视图继续按精确 block hash 固定；限量不影响已经持有的 Arc。

## 恢复起点与持久化边界

持久化 store 在打开时从调用方提供的已认证 base tree 生成独立文件
`<checkpoint>.replay.base.qmdb`。它与后台滚动更新的 `<checkpoint>.base.qmdb`
分开，格式沿用有 checksum 的 leaf form；写入经过临时文件、同步和 rename。
它是当前进程初始 base 的派生恢复文件，内部 chain/height 标签为占位值，不能当作
链的 bootstrap 证明；加载时重新校验树及精确 base block hash/root。

打开时生成该文件增加一次 base 写入和磁盘占用，但持久化进程不另留完整 base RAM
副本。memory-only store 则保留一份初始树。冷恢复先完成加载认证，再替换当前树，
所以会暂时同时持有两个树；没有把这个路径称为低内存常驻或无锁读取。
缺失、损坏或有效文件中的错误 block identity 都会报错，不先丢掉当前可用分支。
后台 rebase 更新滚动文件后，初始恢复文件仍能恢复旧版本。

## 回滚正确性修复

core 在 recorded block 执行期间保留这个块可能重新打开的全部叶堆。一个块超过
256 个 twig 时，不再提前丢掉它自己的回滚材料。undo 预检查也覆盖起始的**部分 twig**，
缺少对应堆时在修改树之前报错，避免后续 reopen 才失败。

另添加可重复的 WAL 并发故障回归：先挂起一个 3,000 次更新块的 WAL，再在其上
定价 527,000 次更新的推测候选，随后令 WAL 失败。推测候选虽已撤销，却淘汰了
失败父块所需的旧叶堆。修改前返回 `Undo(Ahead)`，不能完成父块回退；
[失败日志](benchmarks/20260914-qmdb-undo-budget/wal-before.log) 保留了复现结果。

现在 WAL 失败通过同一个认证分支移动路径恢复父块，必要时从 base 重建。
`pop_applied` 在 undo 验证成功后才删除记录。回归验证失败块不发布、正确重试成功，
重新打开后也只恢复耐久前缀和重试块。

## 容量对照

使用完整 node 的显式测量测试，200k 个真实编码账户，每块更新 147k 个不同账户，
连续 16 块。比较同一实现的“只保留条数上限”和“启用默认字节预算”，其他逻辑一致。
每个块的根由独立树计算并核对，两组全部根一致。

| 第 16 块 | 仅条数上限 | 启用字节预算 |
| --- | ---: | ---: |
| 保留 undo 条数 | 16 | 10 |
| undo 计费 capacity | 419,117,184 B（约 399.7 MiB） | 261,948,240 B（约 249.8 MiB） |
| 分支操作记录 capacity | 169,344,000 B | 169,344,000 B |

undo 计费量下降 **37.5%**；第 11 块起受字节预算限制，不再随这个序列增长。
这是 debug 构建中的分配容量核算，**不是 RSS、release 耗时或链 TPS 基准**。
为归因关闭读视图和 WAL，不能代表完整 only 模式节点内存；最新单条超限仍允许超预算。

分支操作记录继续增长，这是尚未解决的另一大项。滚动 base 文件并不会清理当前
进程中的 `blocks`；下一步需要结合执行/最终确定水位设计安全的操作记录驻留边界或
磁盘读取，不能直接删掉 lagging reader 或 sibling 仍需要的历史。

## 验证

- core release：64 passed、2 ignored。含超过 256 twig 的整块撤销、部分 twig
  缺堆拒绝，以及既有跨客户端根、proof、快照和 undo 对照。
- node QMDB：45 passed、6 ignored。新增小字节预算下的历史视图、外部 pin、
  兄弟分支、WAL 失败、重启、滚动 base 分离，以及损坏/缺失/错身份恢复文件检查。
- 真实 `BlockchainProvider` + 内存 JSON-RPC 集成：1 passed。
- 容量实验：1 passed；workspace all-targets clippy `-D warnings` 通过。
- `git diff --check` 通过。[日志、源码快照与哈希](benchmarks/20260914-qmdb-undo-budget/)。

这批改善 undo 内存与历史恢复正确性，没有证明总内存有界、冷重放尾延迟可接受，
或四节点持续 1M TPS。真实联网、多窗口及重启追赶仍受当前 socket 限制，目标继续推进。
