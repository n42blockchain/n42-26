# TASK (codex, Linux/mac + 7-node testnet): 块间 cadence 埋点 + 持续高 TPS 实测

> 承接高 TPS 调查收口（devlog-76/77/79）：结论是同口径转账链能跑 ~90K，但**持续吞吐的真实
> 限制是「大块 import/commit 节奏 / 块与块之间的空档」**，不是 EVM、不是签名、不是网络传播。
> 现在缺的是**把这个空档钻到底的数据**。Claude 在 Windows 这台机器被 datc（占 100GB+）卡住，
> 跑不了本地 testnet，故交给 codex。基线：reth `chore/merge-upstream-fc2cc1e` @ 449ecfdce（reth 2.3）。

## 为什么现有埋点测不到这个

`crates/n42-node/src/orchestrator/mod.rs:120` 的 `PipelineTiming` 是**单块**计时：每块都从
`created` 起算，只记 `build_complete / import_complete / committed`（全是相对 `created` 的累计
里程碑）。它**看不到块 N 提交 → 块 N+1 开始构建之间的墙钟空档**——而持续 90K 时正是这段
空档（块到达有长间隔、打包尾 ~1s）在吃吞吐。`n42_pipeline_total_ms` 只是单块流水线长度，
不等于出块节奏（cadence）。

## 要做的（加埋点，默认零行为变化）

1. **块间 cadence 埋点**：在 orchestrator 记录「上一块 commit 时刻」，新块 commit 时算
   `inter_block_commit_ms = now - last_commit`，发 `histogram!("n42_inter_block_commit_ms")` +
   一行 `info!`。这是出块节奏的直接度量（理想 ≈ slot 或更短）。
2. **空档分解**（leader 视角）：把 `commit(N) → build_start(N+1) → 首字节广播(N+1)` 这段拆成
   - `commit_to_build_start_ms`（select 循环调度到下一块构建的延迟——是否被 finalize-FCU / 其他
     分支阻塞？关联 devlog-69 + Stage 8 的 `N42_ASYNC_FINALIZE_FCU`）
   - `build_start_to_broadcast_ms`（payload build 本身的墙钟，含 pool 拉取 + EVM + 串行后处理）
   各发一个 histogram + info。复用现有 `schedule_payload_build` / `execution_bridge.rs` 的触发点。
3. **import 尾**（follower 视角）：现有 `n42_follower_ready_to_accept_ms`
   （`execution_bridge.rs:657`）已经是「leader ready → follower 可接受」的端到端。再补一个
   **import 自身耗时**（`new_payload`+fcu 的墙钟，从收到 block_data 到 `import_complete`），
   发 `n42_follower_import_ms`，把传播延迟和 import 计算分开。

## 实测（持续高 TPS，不是单块峰值）

- 7 节点（沿用你 devlog-79 修好的 harness，`--sync-ingest-mode per-node`、TCP 批量注入），
  **简单转账**负载（不是 ERC-20 重合约——要的是冲节奏，不是测 EVM），跑够长（≥ 60s 稳态）。
- 同时给 `N42_ASYNC_FINALIZE_FCU=0` 和 `=1` 各跑一轮，对比上面所有 cadence histogram 的 p50/p95：
  Stage 8 的 async-finalize 到底有没有压低 `commit_to_build_start_ms` / `inter_block_commit_ms`。
- 报告里要能回答：**那 ~1s 的块间空档，到底落在 commit→build_start、build 本身、还是 import 尾？**
  哪个是真正能再压的杠杆。

## 交付

- 埋点改动（默认关行为不变的部分逐字节不变；新 histogram 是纯增量观测，可常开）。
- `docs/devlog-80-interblock-cadence.md`：cadence 分解表（p50/p95）、async-finalize A/B 对照、
  结论（下一个可压的杠杆是哪段）。
- 提交别带 Claude 字样（见根 CLAUDE.md）。基于 main 开分支，PR 回来 Claude 验收。

## 涉及文件
- `crates/n42-node/src/orchestrator/mod.rs`（`PipelineTiming` 附近加 last_commit 状态 + cadence 埋点）
- `crates/n42-node/src/orchestrator/consensus_loop.rs`（commit 路径记 inter-block + import 尾）
- `crates/n42-node/src/orchestrator/execution_bridge.rs`（build_start / broadcast 时刻、follower import_ms）
