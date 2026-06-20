# TASK (codex, Linux/mac + 7-node testnet): 钻 FCU-no-payload 与 leader-no-build 两类真因（devlog-84 的残余瓶颈）

> devlog-84 修好了 span panic（0 panic），但**不是 TPS 胜利**——wall 没涨，inter-block p95 反而到 51s。
> panic 只是症状。pool 满的 timeout view 现在分两类，这两类才是 50–60s 尾部的真因：
> - **A. FCU 返回 no payload_id**（`outcome="none"`，6/12）：build 起了，reth SYNCING 不给 payload，限流耗尽干等；
> - **B. leader 根本没起 build**（6/12）：连 `leader_build_start` 都没打。
> 基线：reth `chore/merge-upstream-fc2cc1e` @ 449ecfdce。**先 git -C ../reth 清干净（配对 worktree）。**
> 本任务基于 `fix/payload-build-span-panic` 分支继续（panic 修复 + N42_BUILD_OUTCOME 已在上面），别回退它。

## 注意：reth 侧改动的处理

devlog-84 你在 ../reth 上做了 `fix/payload-span-parent-lifetime`（93a903c130），但那没推到我们的 reth
remote、也不在基线 449ecfdce。**请把它推到我们的 reth fork**（作为 fc2cc1e 之上的 additive fix，PR/分支都行），
并在 devlog 里写清 commit，否则别人 checkout 基线 reth 复现不了你的 0-panic 结果。同时确认：**n42 侧
`.instrument(Span::none())` 单独（不带 reth 改动）能不能也消掉 panic**？跑一轮基线 reth + 只带 n42 改动，
告诉我 panic 数——这决定我们要不要动 reth 基线。

## 要诊断的（先观测，别改业务逻辑）

### A. FCU-no-payload（reth SYNCING 不给 payload）
为什么 pool 满 90k 时 `fork_choice_updated_with_attrs` 返回没有 payload_id？补诊断：
- FCU 返回时记 reth 的 **payload status / forkchoice status**（VALID/SYNCING/ACCEPTED/INVALID）、
  reth engine tree 的 **canonical head block number** vs 共识期望的 parent number（**reth 落后多少块？**）、
  上一次 finalize FCU 是否还在飞、eager import queue 深度。
- 假设：高负载下 eager import / finalize FCU 把 reth engine tree 顶到 SYNCING，导致 build-trigger FCU 拿不到
  payload。验证这个因果：把「FCU SYNCING 次数」和「pending eager import / 未完成 finalize」时间序列对齐。

### B. leader-no-build（连 build 都没起）
为什么有些 view leader 不调 `do_trigger_payload_build`？补诊断：
- 在 leader 决定要不要 build 的调度点（schedule_payload_build / slot 定时器 / next_build_at 那条路径）打日志：
  该 view 是否 is_current_leader、`building_on_parent` 是不是被上一个**没清掉的 in-flight build 卡住**
  （devlog-84 你加了「completion 匹配 guarded parent 才清」——确认有没有 parent 不匹配导致 guard 永不清）、
  `next_build_at` 有没有被设、retry budget 是不是已 exhausted 所以这一 view 被跳过。
- 假设：A 类的「限流耗尽」+「guard 没清」会让 leader 在后续 view 直接不 build（B 类），两类可能同源。

## 实测

devlog-84 同配置（7 节点 continuous、240s、N42_INJECT_PORT 防呆）。目标是把 A/B 两类各自的**触发条件**用
时间序列钉死，不是再测一遍 TPS。

## 交付：docs/devlog-85-fcu-nopayload-noleaderbuild.md
- A 类：FCU 返回 no-payload 时 reth 的 forkchoice status 分布 + reth head 落后块数 + 与 eager/finalize 队列的相关性。
- B 类：leader-no-build 的具体原因分布（guard 卡住 / retry 耗尽跳过 / 调度没触发）。
- **明确结论：A 和 B 是不是同源（guard 没清 + 限流耗尽 → 后续 view 不 build）？根因是 reth engine-tree 在高负载
  被顶成 SYNCING，还是我们这边的 build 调度/guard 逻辑 bug？**
- 据此给下一步改法建议（先别大改，给数据 + 建议）。
- reth 侧 span 修复推到我们 fork 的情况说明 + n42-only-on-baseline 的 panic 数。
- 基于 fix/payload-build-span-panic 开分支 PR；提交别带 Claude 字样；Cargo.lock 若变必须干净 reth 重生成。
