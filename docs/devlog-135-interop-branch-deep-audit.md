# devlog-135：gov5 互操作分支深度审计（2026-07-25）

## 背景

`feat/gov5-n42-live-interop` 累计 39 个提交、14,388 行生产代码，是
`gov5-n42-production-interop-plan.md` 总目标（Gov5 与 n42-26 互为同一条 N42 链的
可互换实现）的全部实现载体，尚未合入 `main`。P0/P1/P2/P3/P5 已 PASS，P4 与 P6 在
真机窗口中。本次审计在合入 `main` 之前对该分支做一次独立复核，重点是跨客户端引入
的新攻击面：未鉴权 peer 数据的解码边界、混合客户端签名域、执行门控投票的活性。

审计范围为该分支相对 `main` 的全部代码差异，不含 docs 与 scripts。

## 发现

### HIGH-1（已修）：gov5 block-by-hash 响应的 Snappy 解压未受声明长度约束

`crates/n42-network/src/gov5_rpc.rs` 的 `decode_chunked_block` 先校验
`declared_len <= MAX_GOV5_BLOCK_SIZE`（1 MiB），随后

```rust
FrameDecoder::new(frame).read_to_end(&mut decoded)?;
```

把整个压缩帧解压到底，再比对长度。声明长度与帧内容都由 peer 控制，二者无需一致：
长度检查约束的是**声明值**，`read_to_end` 消耗的是**实际解压产物**。入站字节被
`MAX_SNAPPY_FRAME_SIZE`（≈1.17 MiB）限住，但 Snappy 对高重复输入的实测压缩比约
21x（16 MiB 全零 → 约 790 KiB），因此单次 wire-legal 响应即可让节点分配约 20 倍于
块上限的内存，且每个并发请求线性叠加。长度不符的错误在分配之后才抛出，拒绝得太晚。

同文件另外两条解压路径（`decode_status`、`read_status`）都已正确使用
`.take((declared_len + 1) as u64)`，本处是三取二的遗漏。

修复：对齐另外两处，解压上限设为声明长度加一字节——足以判定"超长"，又不会为超长
数据分配空间。回归测试 `rejects_snappy_expansion_beyond_the_declared_length` 构造
一个压缩后仍小于块上限、解压后 16 MiB 的真实炸弹帧，断言解码失败。

### HIGH-2（已修）：H2 执行门控投票的 import 证据可能被挤出，导致该 view 永久不投票

H2-v4 参与者模式下 R1 投票是执行门控的：`handle_proposal` 只在
`imported_blocks` 已含该 hash 时立即投票，否则挂起 `pending_proposal`，等
`on_block_imported` 释放。`imported_blocks` 由一个容量 64 的 FIFO 约束，且在 H2-v4
模式下**跨 view 保留**（新 leader 常重提同一个未提交块）。

驱逐不看这条证据是否正在被等待：

```rust
if self.imported_block_fifo.len() >= MAX_IMPORTED_BLOCKS
    && let Some(oldest) = self.imported_block_fifo.pop_front()
{
    self.imported_blocks.remove(&oldest);
}
```

追赶会成批导入区块，64 个即可把 `pending_proposal` 正在等的那个 hash 挤出。而这一
丢失是终局性的而非仅仅浪费：orchestrator 对区块数据（`pending_block_data` 去重）
与 eager import（`eager_import_already_validated`）都做了去重，reth 已执行过的
hash 不会产生第二次 `BlockImported`，挂起的投票于是永远等不到释放。症状与 P4 记录
的 execution-stall 一致——该验证者在整个 view 静默，7 节点下单点尚可（n−f = 5），
追赶期多点命中或恰为关键票则拖住 view 推进。

修复：驱逐时跳过 `pending_proposal` 等待的 hash，把它轮转到队尾。缓存最多多留一条，
仍然有界。回归测试 `imported_block_eviction_keeps_the_hash_a_deferred_vote_waits_on`
在挂起投票后灌入 128 个块（两倍容量），断言被等待的 hash 存活且缓存不超过 65 条。

`24210f0`（orchestrator 层 `h2_v4_block_views` 用 `BTreeMap::pop_first` 按 hash 序
而非年龄序驱逐）是同一类缺陷的另一处实例，已在该提交修复；本处是状态机层的对应
问题，此前未覆盖。

### MEDIUM（记录，未改）：H2-v4 路径没有批量验签

`ConsensusSigningProfile::H2V4` 在 `build_qc_with_profile_message` 与状态机的入站
验签里都退化为逐签名 `verify_single`，Native 路径走的
`batch_verify_with_fallback`（devlog-101，500 节点 QC 351.0ms→137.3ms，2.56x）在
H2-v4 下不生效。`verify_h2_v4_aggregate` 存在，缺的是多消息批量接口的 H2-v4 变体。

7 节点混合委员会下无实际影响，是 P3/P4 规模的正确取舍；若 Rust 验证者比例提高或
委员会规模上去，需要补 H2-v4 的 `batch_verify_with_fallback`。

### LOW（记录，不改）：`N42_QUALIFICATION_ABORT_AT` 是生产路径上的 crash 注入钩子

`qualification_abort_at` 在 5 个持久化边界（commit QC、vote、QMDB commit、execution
validated）上按环境变量触发 `std::process::abort()`。默认 inert，需要精确匹配点名
才生效。它留在生产路径是**有意的**：P4 的 crash-consistency 用例必须打在与生产完全
相同的 pinned 二进制上，feature-gate 会让被测对象不再是交付物。维持现状，仅记录。

`crates/n42-node/src/qmdb_state_root.rs` 里重复实现了一份同名函数，可在后续合并到
`n42-consensus-service` 的公共实现。

## 复核确认无误的部分

- **签名域隔离**：Native 与 H2-v4 使用不同 DST，`ConsensusSigningProfile` 贯穿
  proposal/vote/commit/timeout/new-view 全部消息的构造与验签，跨域重放不成立；
  选错 profile 只会验签失败（拒绝），不会伪造成功。
- **view 绑定授权**（`f25e968`）：删除了"按 bitmap 长度在已知集合里找匹配"的回退，
  改为严格按 certificate view 解析验证者集，不匹配即 fail closed。这条修复本身
  堵的是一个真实漏洞——旧委员会的合法签名此前可被当作新 view 的有效 QC。
- **默认关闭**：`N42_OBSERVER_MODE` 与 `N42_GOV5_H2_PARTICIPANT` 均默认 false 且
  互斥，bootstrap bundle 需要模式与 header profile 双重满足，现有七节点在不设任何
  环境变量时行为不变。
- **compact receipts 解码**：收据数/日志数/主题数/游标边界四层限制齐备，
  `log_count > cursor.remaining() / 22` 在预分配前挡住了声明放大。
- **finalized range 解码**：块数上限、`materialize` 才预分配、聚合物化字节上限、
  逐块 header hash 与 tx/receipt root 重算，整帧认证后才物化。

## 与 P4 窗口的关系

当前 P4 正式窗口自 2026-07-24T22:06:27Z 起跑，acceptance 要求 86,400 秒。两项修复
对该窗口的处置不同：

- HIGH-1 只收紧解码上限，不改变合法数据的行为，可在窗口结束后随其他改动一并纳入。
- **HIGH-2 改动的正是 P4 正在验证的执行门控投票路径**，必须在当前窗口自然结束后
  纳入并重建二进制；若在窗口期内替换二进制，该窗口作废。

因此两项修复提交在独立分支 `fix/gov5-interop-audit-20260725` 上，不推入
`feat/gov5-n42-live-interop`，由真机侧在窗口结束后决定纳入时机。

## 后续

1. P4 窗口结束 → 合入本分支两项修复 → 重建 pinned 二进制 → 按现有流程重跑窗口。
2. P6 participant 激活仍以 P4 通过为前提，不因本次审计提前。
3. 合入 `main` 前需解决 `hardening/gov5-cross-port` 与本分支在
   `n42-network/src/lib.rs`、`gossipsub/handlers.rs` 的 2 处冲突（双方都动了 gossip
   上限常量，同值不同来源）。
4. H2-v4 批量验签在委员会规模或 Rust 占比提升前补齐。
