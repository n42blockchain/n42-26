# Devlog 98: S4 equivocation 与崩溃后双投防线

日期：2026-07-18
范围：`codex-task-sync-from-gov5-2026H1.md` / S4

## 审计结论

S4 审计发现并修复了五个同族缺口：

1. R1/R2 接收路径先按 collector 的目标 block hash 过滤、后做 BLS 验签与 equivocation 跟踪。兄弟哈希先到时会被直接丢弃，因此检测结果依赖消息到达顺序。
2. 只有 Vote/CommitVote tracker，没有 Proposal tracker。诚实 follower 的 `last_voted_view` 能阻止第二张 R1 票，却不会记录 leader 的双提案证据。
3. R1 有内存、快照和 8-byte fsync vote log 三层守卫；R2 没有 `commitVotedInView`，进程内重复 PrepareQC 及崩溃恢复后都可能再次签 CommitVote。
4. 节点启动对损坏快照、打不开的 vote log、读取失败分别采取“fresh state / NoopVoteLog / view=0”降级。这会把持久化安全状态错误转成双签窗口，违反“宁停不错”。
5. Proposal tracker 若只比较 block hash，会漏掉“同 block hash、不同 validator changes”的两份有效签名。后到消息还会覆盖本地 changes-hash 缓存，使 R2 签名域分裂。

## 修复

- R1 和 R2 都在完成对应验证者的 BLS 验签后更新 tracker，再检查 collector 目标哈希。合法兄弟投票无论先到还是后到都能形成本地 authenticated evidence；伪造签名不会污染 tracker。
- 新增逐 view Proposal tracker。索引值是完整 proposal BLS signing message 的 BLAKE3 commitment，签名域包含 `view + block_hash + validator_changes_hash`；普通双提案仍报告两个 block hash，同块不同 changes 时报告两个不同 signing-message commitment。
- `Some([])` 与 `None` 共享签名表示，因此统一按“无 validator change”处理，避免相同签名消息产生不同状态变更语义。
- `RoundState` 新增独立的 `last_commit_voted_view`。leader R2 self-vote 和 follower PrepareQC 路径均按 `guard -> 更新内存 -> fsync -> 签名` 顺序执行。
- 快照升至 v5，写入 R1/R2 两个 last-voted view。v4 及更早快照保守地令 R2 view 至少等于 R1 view；v5+ 缺字段直接拒绝启动。
- vote log 从 8 bytes 扩为固定 16 bytes（offset 0 = R1，offset 8 = R2）。旧 8-byte 文件迁移时把 R1 值复制到 R2；异常长度拒绝启动。
- 损坏快照、vote log 打开/读取失败不再降级；没有 snapshot 却存在非零 vote log 也拒绝 fresh start。

保守迁移可能让升级后的节点在当前 view 少投一次 R2，最多损失一个 view 的活性；它不会让节点在无法证明历史签名状态时再次签名。

## 兼容性与上线

- 共识消息和签名域没有变化，协议版本仍为 v3；网络层可滚动升级。
- 快照 v5 与 16-byte vote log 是本地磁盘格式升级。旧二进制只认识 R1 守卫，升级后不应回退到旧版本继续参与验证；生产验证者应在一个维护窗口完成升级。
- 与 S1/S2/S3 叠加时沿用其协议 v4，S4 不增加新的线格式门槛。

## 验证

- `cargo test -p n42-consensus --lib`：187 passed。
- `cargo test -p n42-consensus-service --lib persistence`：34 passed。
- `cargo test -p n42-consensus --test integration_test fault_tolerance`：9 passed。
- `cargo check --all-targets`、`cargo clippy --all-targets -- -D warnings`、`cargo test --workspace`：全绿。
- 新增覆盖：双提案两种到达顺序、伪造 proposal、不同行为但同 block hash 的双提案、R1/R2 兄弟哈希两种到达顺序、重复 PrepareQC、恢复后的 R2 guard、v4/8-byte 保守迁移、损坏持久化状态 fail-closed。
