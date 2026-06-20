# TASK (codex, Linux/mac + 7-node testnet): 修 payload build 的 tracing span panic（devlog-83 钉出的头号瓶颈）

> devlog-83 定位：高 TPS 下 pool 满还 timeout 的 ~10s 空档，主因是 leader 侧「no-broadcast view」
> （9/11），症状是 build 反复起、永不产块。凶手是全节点 panic：
> `tracing-subscriber sharded.rs:306: tried to clone Id(...), but no span exists with that ID`，
> 集中在 reth 内部 **`deferred-trie`（×29）和 `payload-convert`（×7）线程**。spawn 出去的
> payload-resolve/state-root 任务被这个 panic 打死 → 该 view 走到 base timeout → 连环拖成 10–30s。
> 这是现在头号 wall-TPS 限制，是真 code bug。基线：reth `chore/merge-upstream-fc2cc1e` @ 449ecfdce。
> **先 git -C ../reth 清干净（配对 worktree，按 devlog-82/83 的做法）。**

## 根因假设（待你用真实 stack 证实）

`sharded.rs:306` 这个 panic 的本质：一个 span 的 **Id 在 span 已经关闭/drop 之后被 clone**，通常发生在
owned span 跨任务/跨线程池传播、但拥有它的 async 任务先结束了。可疑路径：

- `crates/n42-node/src/orchestrator/execution_bridge.rs:596` 建 owned `eager_span = info_span!(...)`，
  `:743` `.instrument(eager_span)` 把它 move 进异步任务；
- `:1064` `handle_built_payload` 里 `Span::current()` 记字段；
- 进入 reth 的 payload build / deferred state root 时，reth 在 **rayon/blocking 池**（`deferred-trie`/
  `payload-convert` 线程）里 spawn 工作，并 **clone 当前 span 的 Id 做上下文传播**。等这些池线程真正跑时，
  外层 async 任务（和它的 span）可能已经 drop → clone 一个已关闭的 Id → panic → 该 build 任务死掉 →
  永不 `leader_ready`/broadcast。

`N42_DEFER_STATE_ROOT=1` + parallel-evm + twig WAL 这些近期改动会加重跨线程 span 传播，怀疑是它们暴露/
引入的（如果 bisect 便宜，确认下是不是某个 commit 引入；不便宜就直接修）。

## 要做的

1. **修 span 跨任务/跨池传播**：让 n42 的 owned span **不要**作为 `current` 跨进 reth 的 spawn 池。可选：
   - 调用 reth payload build / state root 的那段，用 `tracing::Span::none()` 或 `in_scope` 把它从 n42 span
     scope 里摘出来（reth 内部池就不会去 clone 一个会被关掉的 parent Id）；
   - 或确保 instrument 的 span 生命周期覆盖整个 spawn 工作（别提前 drop）；
   - 或给跨进池的工作设 `parent: None`。
   先用最小改动消除 panic，**别动 payload/import 的业务逻辑**。
2. **加显式 per-attempt build 结果日志**（devlog-83 建议 #2）：在 payload resolve 任务收尾处打
   `N42_BUILD_OUTCOME`：`resolved | timeout | none | err | join_panic`，带 elapsed_ms、view、parent、
   payload_id、attempt_id。这样即使还有别的失败模式也能看清，而不只是「started but not emitted」。
3. **防静默 retry 连环**（建议 #3）：同一 parent/view 若已有 build 在飞，别每 2s 再 spawn 一个 resolve；
   要么跳过、要么取消上一个并记录 cancellation。
4. **panic 兜底**：spawn 的 payload/trie/convert 任务即使内部出错也要 **fail-fast 并记一条结构化 error**，
   不能静默吞掉导致 view 干等 timeout。

## 验证（E2E，你负责）

1. **回归**：默认 E2E（单节点 1、ERC-20 3、多节点 4）行为不变。
2. **重跑 devlog-83 同配置**（7 节点 continuous、240s）：
   - 确认 `N42_TIMEOUT_VIEW` 的 no-broadcast 样本**大幅下降或归零**；
   - 确认日志里**不再有 `sharded.rs` / span clone panic**；
   - 量 `inter_block_commit_ms` p50/p95：那 12–14s（甚至 30s）尾部应显著回落，wall sustained TPS 应上升；
   - 若 no-broadcast 修掉后 `build_start->broadcast` 仍 > 2s slot，再回到 pack/finish/compress 优化（下一任务）。
3. 结论写 devlog-84，附修复前后的 no-broadcast 占比 + inter-block + wall TPS 对照。

## 交付
- 基于 main 开分支 PR；提交别带 Claude 字样；Cargo.lock 若变必须干净 reth 重生成。
- devlog-84-fix-payload-span-panic.md：根因（真实 stack）、修法、修复前后对照、是否还有残余 no-broadcast。
