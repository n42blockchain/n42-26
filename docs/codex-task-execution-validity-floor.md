# Codex 任务:committed 块必须有执行有效性下限(CRITICAL)

> 这份文档是给 **OpenAI Codex** 的任务说明,自包含可直接开工。
> 完整审计背景见同目录 **`h2-consensus-audit-2026-07-11.md`**(跨仓审计:Go 仓同族缺陷已实弹爆过一次)。
>
> **提交规范**:所有 git 提交**不要包含 "Claude" / "Codex" / "Co-Authored-By" 等 AI 署名**。
> 用项目模板作者:
> ```
> GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
>   git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" -m "..."
> ```

## 1. 背景(为什么这是 CRITICAL)

2026-07-11,Go 仓(N42-gov5,HotStuff+QMDB 栈)在 7 节点实弹中爆出同族事故:
投票门把"块体已到"当成"块已执行",一个分叉 leader 的**不可执行块**(携带 stale-nonce 交易)
拿到了 2f+1 的 CommitQC 被 committed;committed head 从此钉死在一个任何诚实节点都无法执行
的块上,协议层(正确地)拒绝回滚 committed,全网只能靠停机手术(清 persisted 共识状态)恢复。

本仓(Rust)的形态**更激进**:R1 投票是乐观的——follower 验完提案信封(leader 身份 + BLS 签名 +
`justify_qc` + 安全规则)后立即投票,**完全不执行块**(`crates/n42-consensus/src/protocol/proposal.rs:363-380`,
注释明说 "R1 vote … does NOT commit to block validity")。提交时 `handle_block_committed`
**无条件**推进 `head_block_hash` 并递增 `committed_block_count`
(`crates/n42-consensus-service/src/orchestrator/consensus_loop.rs:451-452, 353`)。
若该块随后 `new_payload` 返回失败,`handle_import_done` 的失败分支**只调用 `initiate_sync`,
从不回滚 head**(`consensus_loop.rs:1012-1021`)→ 下一次 payload build 会以不可执行块为父块。

缓解因素(也是为什么至今没炸):leader 自己执行过该块;默认配置下 reth 在 `new_payload`
里逐块验 state root。暴露面:Byzantine leader、EVM 非确定性、以及设置了
`N42_SKIP_STATE_ROOT` / `N42_DEFER_STATE_ROOT` 的部署。

## 2. 任务

**目标:一个 committed 块在本地被 reth 确认执行有效(`Valid`/`Accepted`)之前,
不得成为本地 `head_block_hash`;确认失败时节点宁可停止出块,也不得在其上建块。**

### 涉及文件

- `crates/n42-consensus-service/src/orchestrator/consensus_loop.rs`
  (`handle_block_committed`、`finalize_committed_block`、`handle_import_done`)
- `crates/n42-consensus-service/src/orchestrator/execution_bridge.rs`
  (eager import 的结果如何回流)

### 期望改动

1. `committed_block_count` 照旧在 QC 上推进(共识层面的 agreement 是安全的,不要动)。
2. `head_block_hash` 的推进改为**只在确认的执行结果上**发生:eager import、finalize FCU、
   或 bg import 三条路径中任意一条对该块返回过 `Valid`/`Accepted` 才算。
3. `handle_import_done(success=false)` 且该块**已 committed** 时,升级处理:
   - `error!` 级日志(现在只有低调 warn+sync);
   - 新增 metric `n42_committed_block_unexecutable_total` 并递增;
   - `head_block_hash` 保持在**最后一个确认执行过的块**,后续
     `do_trigger_payload_build` 不得以坏块为父 → 节点停止扩展无效链
     (宁可停,不可错)。
   - `initiate_sync` 照旧保留(万一是本地暂态,sync 能救回来)。

### 红线

- **不要**改投票时机本身(乐观投票是有意的设计,改它是另一个更大的讨论);
  本任务只加"提交后的执行下限"。
- **不要**回滚/改写 `committed_block_count` 或任何 QC 持久化——安全规则不许。
- 失败路径必须区分"该块 committed" vs "该块只是 speculative/eager"——后者维持现状。

## 3. 验收标准

1. 新增单元/集成测试:mock `ExecutionLayer` 对某 committed 块的 `new_payload` 返回
   `Invalid` → 断言:
   - `head_block_hash` 停在前一个已执行块;
   - `n42_committed_block_unexecutable_total` 递增;
   - 其后没有任何 `do_trigger_payload_build` 以坏块 hash 为父。
2. happy path 回归:`Valid` 块照常推进 head(现有行为不变)。
3. 现有 7 节点混沌测试 `crates/n42-consensus/tests/chaos_7node.rs` 保持全绿。
4. `cargo clippy` 零新告警。

## 4. 完成后(可选,同区域顺手)

同一审计还列了 Task 2(sidecar JMT/Twig 树的 apply_diff 应推迟到 reth 确认之后,
文件相同,`consensus_loop.rs:466-540` → `handle_finalize_done` 的 `finalized==true` 分支)
和 Task 3-5,见 `h2-consensus-audit-2026-07-11.md` 的 "Task list for codex" 一节。
Task 1 验收通过后可以继续。
