# Codex 留言：互操作分支合入前深度审计（两项修复 + P4 窗口处置）

> 收件：OpenAI Codex
> 日期：2026-07-25
> 背景全文：`docs/devlog-135-interop-branch-deep-audit.md`
> 分支：从 `feat/gov5-n42-live-interop` 拉 `fix/gov5-interop-audit-20260725`（已推送）
>
> **提交规范**：不要包含 "Claude" / "Codex" / "Co-Authored-By" 等字样。作者模板：
> ```
> GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
>   git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" -m "..."
> ```

## 一句话

合入 `main` 前对 `feat/gov5-n42-live-interop` 的 14,388 行做了独立复核，找到两个真实
缺陷并已修复+回归；其中一个改的正是 P4 正在验证的路径，**当前窗口不要动二进制**。

## 两项修复

### HIGH-1：`decode_chunked_block` 的 Snappy 解压未受声明长度约束

`crates/n42-network/src/gov5_rpc.rs`。校验了 `declared_len <= MAX_GOV5_BLOCK_SIZE`
之后仍 `read_to_end` 整帧——声明与帧内容都是 peer 控制且无需一致，长度检查约束的是
声明值，分配发生在比对之前。实测 Snappy 对高重复输入压缩比约 21x（16 MiB 全零 →
790 KiB），单个 wire-legal 响应即可分配约 20 倍块上限，并发线性叠加。

同文件 `decode_status` / `read_status` 两条路径本来就有 `.take(declared_len + 1)`，
这里是三取二的遗漏。修法与那两处对齐。

对正常数据零行为变化。

### HIGH-2：H2 执行门控投票的 import 证据被挤出 → 该 view 永久不投票

`crates/n42-consensus/src/protocol/proposal.rs`。H2-v4 下 R1 是执行门控的，
`pending_proposal` 等 `on_block_imported` 释放；`imported_blocks` 是 64 条 FIFO 且
跨 view 保留。驱逐不看条目是否正被等待，而追赶成批导入，64 个块就能把被等的 hash
挤出。丢失是终局性的：`pending_block_data` 与 `eager_import_already_validated` 双重
去重，reth 已执行过的 hash 不会有第二次 `BlockImported`，投票永远等不到释放。

症状与你们记录的 P4 execution-stall 一致。`24210f0` 修的是 orchestrator 层
`h2_v4_block_views` 的同类问题（`BTreeMap::pop_first` 是 hash 序不是年龄序），
状态机层这处此前没覆盖。

修法：驱逐时把正被等待的 hash 轮转到队尾，缓存最多多留一条。

## 需要真机侧配合的三件事

1. **当前 P4 窗口（2026-07-24T22:06:27Z 起，86,400 秒）照常跑完，不要换二进制。**
   HIGH-2 动的就是执行门控投票路径，窗口期内替换二进制会作废该窗口。
2. 窗口自然结束后（无论 PASS 还是 FAIL），把
   `fix/gov5-interop-audit-20260725` 合入 `feat/gov5-n42-live-interop` → 重建
   pinned 二进制 → 按现有流程重跑窗口。若这次窗口 FAIL 且症状仍是 execution
   stall，HIGH-2 很可能就是根因，优先带上它重跑。
3. 重跑时建议加一条断言：整个窗口内不出现"某验证者在一个 view 内既收到提案又持有
   `pending_proposal` 却始终未发 R1"的情况——这是 HIGH-2 的直接可观测特征，现有
   监控只看 lag 和 root，看不到单节点静默投票。

## 复核确认无误、不必重查的部分

- 签名域隔离：Native / H2-v4 不同 DST，`ConsensusSigningProfile` 贯穿全部消息的构造
  与验签，跨域重放不成立。
- view 绑定授权（`f25e968`）：删掉"按 bitmap 长度猜集合"的回退是正确且必要的，
  堵的是旧委员会签名被当作新 view 有效 QC 的真实漏洞。
- 默认关闭：`N42_OBSERVER_MODE` / `N42_GOV5_H2_PARTICIPANT` 默认 false 且互斥，
  bootstrap bundle 需模式与 header profile 双重满足；不设环境变量时现有七节点行为不变。
- `compact_receipts` 与 `finalized_range` 的解码边界齐备（收据/日志/主题/游标四层，
  块数上限 + 聚合物化上限 + 逐块 root 重算），无需再审。

## 记录但未改

- H2-v4 路径退化为逐签名验签，Native 的 `batch_verify_with_fallback`（devlog-101，
  2.56x）不生效。7 节点无影响，委员会规模或 Rust 占比上去之前需补。
- `N42_QUALIFICATION_ABORT_AT` 留在生产路径是有意的（crash 用例必须打在与交付物
  完全相同的 pinned 二进制上），维持现状。`n42-node/src/qmdb_state_root.rs` 里有一份
  重复实现，可择机并到 `n42-consensus-service` 的公共实现。

## 合入 main 的剩余阻塞

`hardening/gov5-cross-port` 与 `feat/gov5-n42-live-interop` 在
`n42-network/src/lib.rs` 和 `gossipsub/handlers.rs` 有 2 处冲突（双方都动了 gossip
上限常量，同值不同来源）。两边都要进 main，合并顺序定了之后需要有人解一次。
