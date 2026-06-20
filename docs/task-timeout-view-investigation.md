# TASK (codex, Linux/mac + 7-node testnet): timeout-view 路径定位（devlog-82 钉出的真瓶颈）

> devlog-82 收口了高 TPS cadence：30s 是 harness 假象（已修），leader build 1.9–2.6s 浮出，但
> **真正的 12–14s 尾部是「pool 满 90k 时 view 仍跑到 ~10s base timeout 才跳过」**（view 260/265/267）。
> 这是共识 liveness/pacemaker 问题，不是 build、不是 ingest——是现在头号 wall-TPS 限制。
> 基线：reth `chore/merge-upstream-fc2cc1e` @ 449ecfdce。**先 git -C ../reth 清干净再干**（用配对
> worktree 最稳，见 devlog-82 的做法）。

## 要回答的核心问题

那些在 pool 满的情况下仍然 timeout 的 view（每次吃掉 ~10s base timeout），到底卡在哪？三个候选，用数据区分：

1. **build 超出 slot**：leader build_start→broadcast 1.9–2.6s vs 2s slot——是不是 leader 偶尔 build/broadcast
   超过 slot 窗口，导致 follower 等不到块、该 view 走到 base timeout？（最可能）
2. **某节点 stall**：是不是特定 leader（round-robin）慢/卡，它当 leader 的那些 view 才 timeout？
3. **view-change 太慢**：timeout 触发后，到下一个 leader 真正出块之间的恢复路径有没有额外延迟？

## 要做的（先观测，别改共识行为）

把每个 **skipped/timeout view** 和它的上下文关联起来，加诊断日志（observation-only）：
- timeout 触发时记：view、该 view 的 leader 身份（index）、距上次 commit 的耗时、当时 pool_pending。
- 该 view 的 leader 视角：有没有触发 build？build_start→broadcast 花了多久？块有没有广播出去？
- follower 视角：有没有收到该 view 的 block_data？收到时间 vs timeout 触发时间——是「块根本没来」还是
  「块来晚了但 timeout 先到」？
- 关联表：timeout view → {leader_idx, prev_leader_build_ms, block_broadcast?(y/n), follower_received?(y/n)}。

复用现有埋点（PipelineTiming、N42_CADENCE、pool-depth gauge、N42_FOLLOWER_IMPORT），主要是在 timeout/
view-change 触发点（pacemaker/round timeout 那段）补一条结构化日志，把「timeout 当时的世界状态」打全。

## 实测

沿用 devlog-82 完全相同的 7 节点 continuous 配置（N42_INJECT_PORT=19900 防呆、7 端口验证、ulimit 65536、
per-node-continuous、简单转账、≥90s）。这次目标是**采到足够多的 timeout view 样本**（devlog-82 里 90s 采到
约 3 个，可能要跑久一点或多跑几轮凑样本）。

## 交付：docs/devlog-83-timeout-view.md

- timeout view 关联表（至少 10+ 样本）+ 上面 3 个候选各自的占比。
- 明确结论：那 ~10s 空档主因是 build-overran-slot、node-stall、还是 slow-view-change？
- 据此给出**下一个该改的点**：是把 leader build 压到 < slot、还是缩短/自适应 base timeout、还是优化
  view-change 恢复路径。**先别改，给数据 + 建议**，改法等验收后再开任务。
- 基于 main 开分支 PR，提交别带 Claude 字样，Cargo.lock 若变必须是干净 reth 重生成的。
