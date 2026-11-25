# devlog-191：验证者重启后反向补块停住不恢复（2026-11-25）

分支 `fix/restart-stalled-reverse-walk`，基于 origin/main `e097380`。改动只在
`crates/n42-consensus-service/src/orchestrator/mod.rs` 与 `state_mgmt.rs`。

## 现象

三客户端混合链（3 个 Go、2 个 n42-26、2 个 n42-rs，全部是验证者）里，轮流重启一个 n42-26 验证者。
约 6% 的重启之后，该节点的执行头停在某个高度不再前进，共识 view 仍在走，只能再次重启才恢复。
统计：`30e4178` 24 轮 0 次，`c648b34` 24 轮 2 次，另一次演练里 `30e4178` 也出现 1 次，三次日志症状相同。
不是新引入的问题。

## 原因

n42-26 的 leader 把每个块发两份：原生块数据（进 `pending_block_data`，跟随者乐观导入）和 Gov5 拷贝。
执行落后时，父块不是执行头的块进入 `h2_v4_catchup_blocks`，按父哈希反向拉取，凑齐从 head+1 起连续的一段后按升序导入。

1. 重启节点还在反向补块时，另一个 n42-26 验证者出了块 P。P 的原生拷贝先到，乐观导入返回 Syncing，留在
   `pending_block_data`；Gov5 拷贝随后到达，被 `handle_h2_v4_gov5_block` 的"已缓存"捷径丢弃。节点随后按 QC
   跳过了 P 的 view，没有任何提交路径再去导入 P。子块 P+1 到达后要拉父块 P，`request_h2_v4_gov5_block`
   看到 `pending_block_data` 里有 P 就直接返回。P 永远不进补块队列，队列凑不齐。
2. 补块等待期间，另一个 n42-26 验证者之后出的每个块都重复第 1 条的前半段：原生拷贝先到，Gov5 拷贝被当成重复丢弃。
   队列在这些高度上不断出现新的空洞。只修第 1 条时实测 13 轮仍卡住 2 次：缺的块拉回来了，但又多出别的空洞。

## 修复

- `refetch_natively_cached_ancestry_gap`（`state_mgmt.rs`，由每次换 view 的 `rearm_gov5_execution_pull` 调用）：
  执行头超过 `H2_V4_FETCH_RETRY_TIMEOUT`（2 秒）没有变化时，对队列里"父块高于执行头、未入队、但在
  `pending_block_data` 里"的块，绕过原生缓存去拉父块的 Gov5 拷贝。`request_h2_v4_gov5_block` 拆出
  `request_h2_v4_gov5_block_inner(peer, hash, native_copy_counts)`，原有调用方行为不变。
  计数器 `n42_gov5_native_cached_parent_refetch_total`，WARN 日志
  "reverse walk stalled on a parent held only as native block data"。
- `handle_h2_v4_gov5_block` 的"已缓存"捷径加条件 `!self.h2_v4_catchup_active`：补块进行中不再丢弃 Gov5 拷贝。

考虑过但没有采用：让 QC 跳 view 时顺带导入 `pending_block_data` 里的块。这要动提交路径，范围大；
补块队列本来就是落后时唯一的导入通道，让它拿到完整数据即可。

## 测试

单元测试（`orchestrator/mod.rs`），三个都先在没有修复时确认失败：

- `stalled_reverse_walk_refetches_a_parent_held_only_as_native_block_data`
- `waiting_reverse_walk_stages_the_gov5_copy_of_a_natively_cached_block`
- `advancing_execution_head_does_not_refetch_a_natively_cached_parent`

`cargo test -p n42-consensus-service --lib`：346 通过。`cargo clippy -p n42-consensus-service --all-targets -- -D warnings` 无警告。

本地 7 节点混合链，交替重启两个 n42-26 验证者，重启 60 秒后落后 Go 节点超过 3 个块记为卡住：

| 版本 | 轮数 | 卡住 |
|---|---|---|
| `c648b34` | 24 | 2 |
| 只有第一处修复 | 13 | 2 |
| 两处修复 | 48 | 0 |

两处修复的 48 轮里触发条件出现 2 次（第 14、42 轮，重拉日志共 15 条），两次都在重启后 30 秒时落后 12 块、60 秒时追平。

## 已知限制与后续

- 触发时恢复需要 30 到 60 秒：网络层对刚收到过的哈希有 30 秒不重复拉取的限制
  （`GOV5_BLOCK_FULFILLED_RETENTION`，`n42-network/src/service.rs`），本次未改。
- 只在本地混合链验证，未在测试网上运行。
