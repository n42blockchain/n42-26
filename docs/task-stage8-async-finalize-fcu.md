# TASK (codex, Linux/mac + E2E): Stage 8 — finalize-FCU off the consensus hot path

> 承接 Caplin EL-seam 重构(stages 1-2,7 已合并到 main,PR #2 / `e77df7b`)。
> 这步**改共识 finalization 关键路径、时序敏感**,必须多节点 E2E 验证。Claude 在 Windows
> 跑不了 testnet,故交给 codex。设计见下,实现+验证归 codex。
> 基线:reth `chore/merge-upstream-fc2cc1e` @ 449ecfdce(reth 2.3)。

## 背景:devlog-69 的 in-loop FCU 卡顿

`ConsensusOrchestrator::run()` 是 biased `tokio::select!`。`finalize_committed_block`
(`crates/n42-node/src/orchestrator/consensus_loop.rs:570`)在 select 分支里**同步 await**
reth 的 `fork_choice_updated`(经新建的 `ExecutionLayer` trait,`consensus_loop.rs:592` 及
rescue 重试 `:635`)。高负载下单次 FCU 往返 200–500ms+,会**阻塞整个 select 循环**(投票、
网络、定时器全等),devlog-69 已记录。EL seam(`el: Arc<dyn ExecutionLayer>`)已就位,正好
是把 FCU spawn 出热路径的前提。

## 纠缠点(为什么不能简单 spawn)

`finalize_committed_block` 里 FCU 的结果 `finalized` 与三处耦合,都要 `&mut self`:

1. **eager-import-rescue**(`consensus_loop.rs:613-654`):FCU 往返期间 drain
   `self.eager_import_done_rx`,若目标块在这期间被 eager import 进了 reth,就重试一次 FCU。
2. **Case A/B 分支**(`:659-708`):按 `finalized` 决定——
   - Case A(已在 reth):清 `pending_block_data`/`pending_executions`、`enqueue_mobile_packet`、
     leader 调度下一块(`schedule_payload_build`);
   - Case B(未导入):`enqueue_bg_import`;
   - 兜底:drain `leader_payload_rx` 再试 Case B。
3. **下一块构建调度**(Case A 内,leader)。

spawn 出去的任务拿不到 `&mut self`,所以 rescue(读 self 的 channel)和 Case A/B(改 self)
都没法在任务里跑。

## 设计:flag-gated async finalize(默认关 = 行为完全不变)

加 env 开关 `N42_ASYNC_FINALIZE_FCU`(默认 0/关)。

### flag 关(默认,生产路径,零改动)
保持现状:inline await FCU + rescue + 内联 Case A/B。**这条路径必须逐字节不变**。

### flag 开(新 async 路径)
1. **抽方法**:把 `:659-708` 的 post-FCU Case A/B 逻辑抽成
   `async fn handle_finalize_done(&mut self, view: u64, block_hash: B256,
   commit_qc: QuorumCertificate, finalized: bool)`。flag-off 路径在 await 完 FCU 后**也调它**
   (保证两条路径共用同一段 post-FCU 逻辑,降低分叉风险)。
2. **新 channel**:`finalize_done_tx/rx: mpsc::Sender<(u64, B256, QuorumCertificate, bool)>`
   (view, block_hash, commit_qc, finalized)。容量参照 `import_done`(256)。加到
   `ConsensusOrchestrator` 字段 + 两个构造器(`new` / `with_engine_api`,mod.rs)。
3. **flag-on 的 finalize_committed_block**:取 `el`(`self.el.clone()`)后,**spawn** 一个
   任务:构造 `fcu_state`、`el.fork_choice_updated(fcu_state).await`、把
   `(view, block_hash, commit_qc, finalized)` 经 `finalize_done_tx` 送回,然后**立即返回**
   (不阻塞 select 循环)。**flag-on 放弃 rescue 微优化**——FCU 失败就走 Case B 后台导入兜底
   (仍正确,只是少一层"FCU 期间被 eager import 救回"的优化)。这是可接受取舍,需在 E2E 量
   Case A 命中率有无回退。
4. **新 select 分支**:在 `run()`(mod.rs)加一支收 `finalize_done_rx`,调
   `self.handle_finalize_done(view, block_hash, commit_qc, finalized).await`。优先级放在
   引擎输出之后、数据网络事件之前(它是 commit 后续,属高优先)。

### 关键不变量
- flag-off 路径**零行为变化**(逐字节)。
- `handle_finalize_done` 两条路径共用。
- commit_qc 通过 channel 传(它在 post-FCU 逻辑里用到吗?确认 `:659-708` 是否需要 qc——
  若不需要可不传,简化)。
- 不动 EL seam(`ExecutionLayer` trait)、网络、状态树、mobile、staking。

## 验证(E2E,codex 负责)

1. **flag-off 回归**:默认跑现有 E2E(单节点 1、ERC-20 3、多节点共识 4),确认行为/指标与
   main 一致(`n42_fcu_latency_ms`、`n42_eager_import_hits_total`、commit rate)。
2. **flag-on A/B**:`N42_ASYNC_FINALIZE_FCU=1` 跑 4-7 节点 testnet,合约重 + 高负载(用
   `n42-stress`,记得 `CONTRACT_CALL_GAS=100k` 已修),对比 flag-off:
   - **select 循环不再被 FCU 阻塞**:量循环 tick 间隔 / `n42_pipeline_total_ms` p95 应改善;
   - **commit rate / 出块稳定性无回退**;
   - **Case A 命中率**(`n42_eager_import_hits_total` 占比)有无因丢 rescue 而下降——若明显
     下降,评估是否值得保留 rescue(可在 async 路径里用另一个 channel 把 eager 结果也喂给
     finalize_done 分支重试,但先测了再说);
   - 崩溃恢复 / view change / epoch 切换正常。
3. 结论写进 devlog,附 flag-on/off 的 p50/p95 对照表 + 是否建议默认开。

## 涉及文件
- `crates/n42-node/src/orchestrator/consensus_loop.rs`(`finalize_committed_block:570`、
  抽 `handle_finalize_done`、flag-on spawn)
- `crates/n42-node/src/orchestrator/mod.rs`(字段 + 构造器 + `run()` 新 select 分支)
- env 开关辅助(参照现有 `N42_FAST_PROPOSE` / `compact_block_enabled` 的读法)

## 注意
- 这是 EL seam(已合并)之上的增量;基于 main 开分支。
- 提交别带 Claude 字样(见根 CLAUDE.md)。
- 设计如有更优(例如保留 rescue),E2E 数据说话。
