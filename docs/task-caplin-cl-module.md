# TODO: 把共识层做成 Caplin 式"进程内解耦模块"（plan 模式设计）

> 状态：**待办 / 设计阶段**。动手前**用 plan 模式**先出方案，再实现。
> 记录日期：2026-06-14。

## 目标

参考 **Erigon Caplin**（`C:\n42\erigon\cl`，Erigon 的独立共识层/beacon CL）的架构，把 n42
的共识层做成一个**充足解耦、但仍跑在同一个程序（单二进制）内**的模块——既不是散落耦合进
node 编排里，也不是拆成独立进程靠 RPC 通信，而是"clean module + in-process"。

## 为什么看 Caplin

Erigon 把一整套 CL（Caplin）作为模块嵌进同一个二进制：

- 入口/装配：`cl/main.go`、`cl/caplin1/`、`cl/caplincli/`、`cl/caplinflags/`；
- 清晰分层子模块：`beacon`（API/链状态）、`gossip` + `p2p` + `sentinel`（网络）、`pool`
  （消息/attestation 池）、`transition` + `phase1`（状态转换）、`validator`、`aggregation`、
  `persistence`、`fork`、`rpc`、`clstages`（阶段化驱动）等；
- 与 EL 解耦：通过明确的接口/通道交互，而非把共识逻辑铺进执行端。

这正是"解耦又同进程"的范本，值得对照 n42 当前结构。

## 对 n42 的落点（plan 模式里再定）

n42 共识是 HotStuff-2（不是 beacon chain），所以**借的是 Caplin 的模块化/解耦"形"，不是它的
协议**。当前 n42 共识相关散在：`n42-consensus`（状态机）、`n42-node/orchestrator`（3-way
select 编排、把共识/执行/网络缝在一起）、`n42-network`。设计时考虑：

- 把共识层抽成一个边界清晰的进程内模块（类似 Caplin 之于 Erigon），与执行端（reth）、网络、
  状态树通过明确接口/channel 交互；
- 解耦 orchestrator 里共识与执行/FCU/状态树更新的纠缠（参见 devlog-69 审计：FCU 在 select
  热路径同步执行是已知耦合点）；
- 保持单二进制、确定性事件驱动（n42 共识引擎已是纯事件驱动，是好底子）。

## 待办清单（plan 模式启动时展开）

- [ ] 仔细通读 `C:\n42\erigon\cl` 的模块边界、装配方式、CL↔EL 接口（重点 `caplin1`、`clstages`、
      `beacon`、`sentinel`/`p2p`、与 EL 的交互点）；
- [ ] 梳理 n42 当前共识层的耦合点（orchestrator/consensus/network）；
- [ ] 出"进程内解耦共识模块"的目标架构 + 迁移路径（plan 模式产物）；
- [ ] 评估改动面与风险（不破坏现有 HotStuff-2 语义、确定性、persistence）。

## 注意

- **先 plan 后码**：这是架构级改动，必须先在 plan 模式产出方案、评估 trade-off，再分阶段实现。
- 参考资料：`C:\n42\erigon\cl\readme.md`、`cl/CLAUDE.md`、`cl/agents.md`（Erigon 侧的说明）。

## 已完成调研 + 已批准方案（2026-06-14）

plan 模式调研完成、方案已批准。核心结论与分阶段计划：

**关键发现**：n42 的 `ConsensusEngine::process_event`（`crates/n42-consensus`）**已经是纯
事件驱动状态机、且 crate 级隔离**（不依赖 network/node/reth-node-builder）；唯一真正的耦合
是 **orchestrator 里对 reth EL 的内联调用**（`consensus_loop.rs:592/635/804` 的 FCU/new_payload、
`execution_bridge.rs:366/454` 的 build），**没有接口边界**。network/状态树/mobile/staking
**已经解耦**（NetworkHandle + 后台 channel）。`ObserverOrchestrator` 是第二个 EL 消费者，
重复了 FCU/new_payload —— 正好证明"一接口两实现"的收益（Caplin 范式）。

**对标 Caplin**：CL↔EL 是单一 Go 接口 `ExecutionEngine`（local 进程内 + remote RPC 两实现），
一个 `RunCaplinService(ctx, el, cfg)` 同时支持 embedded 与 standalone、零重复。

**推荐方案**：渐进式 ports-and-adapters，**先抽 EL 接口**，逐步演进到独立
`n42-consensus-service` crate。核心 artifact 是 `ExecutionLayer` trait（按 reth 2.3 的 wire
类型 `ExecutionData`/`ForkchoiceState`/`PayloadAttributes` 定义，一个 `RethExecutionLayer`
进程内适配器包 `ConsensusEngineHandle`+`PayloadBuilderHandle`）。

**分阶段**（每阶段可编译、测试过、行为不变）：① 定义 EL trait + adapter（纯加）；② 把 6 处
内联 reth 调用改走 `el.*`（纯重构，行为不变）；③ 抽 sink ports（state/mobile/staking）；
④ 抽 `ConsensusNetwork` port；⑤ `ConsensusServiceConfig` + 改名 `ConsensusService`；
⑥ 移进 `n42-consensus-service` crate；⑦（选）Observer 复用 EL；⑧（选/perf）finalize-FCU
移出热路径（修 devlog-69 的 in-loop FCU 卡顿）；⑨（未来）remote EL 实现 → standalone 模式。

**不可动**：`crates/n42-consensus/**`（纯引擎，永不加 reth-node-builder）、NetworkHandle、
后台 sink、`select!` 分支优先级、wire 格式。

详细 plan（含 trait 签名草图、文件:行、验证步骤）见会话 plan 文件
`enumerated-wobbling-fox.md`（plan 模式产物）。reth 2.3 基线对齐。
