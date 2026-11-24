# devlog-188：同步标准 §10 超时轮换（rotate-on-timeout）与创世 tenure（2026-11-24）

依据 `N42-SYNC-SPEC.md` §10（10.2 规则、10.3 激活与 tenure 来源、10.4 向量、10.5 n42-26 调用点），参考 gov5 main
`c2735f76`（`validator.go` `ViewAfterTimeout`、engine `viewAfterTC`、`rotate_on_timeout_test.go`）。分支
`feat/rotate-on-timeout`，基于 origin/main `34bad2b`。

## 设计决策

- **一个启动时解析的值**：`LeaderSchedule { tenure, rotate_on_timeout_from: Option<u64> }`（`n42-consensus`
  `validator/selection.rs`）。节点在 reth CLI 闭包最前面由创世 `config.hotstuff` 加环境覆盖解析一次，
  `install_leader_schedule` 装入进程；引擎构造时拷贝一份，H2-v4 影子校验与 orchestrator 诊断也读同一值。
  若装入前已有组件读过（得到不同值）则拒绝启动。以后若某个 profile 规定"从 0 启用、tenure 必填"，只需改解析函数。
- **引擎仍是纯状态机**：规则只在构造时读入，事件处理不再读环境或全局；`set_leader_schedule` 供测试在同一进程里跑不同链。
- **创世与环境**：`leaderTenure` 缺省或 0 = 1；`rotateOnTimeoutView` 缺省 = 关闭；字段存在但不是 u64、或
  `hotstuff` 不是对象时拒绝启动（共识可见值不猜）。环境变量只是基准覆盖：`N42_LEADER_TENURE` 为正整数时覆盖
  （与创世不同则 warn），解析不了则忽略并 warn（与旧行为一致：非法值视作未设）；`N42_HOTSTUFF_ROTATE_ON_TIMEOUT`
  = `1|true|TRUE|on` 从 view 0 启用，`0|false|FALSE|off` 强制关闭，其他值忽略并 warn，未设则用创世（与 gov5 同义）。
  现有舰队只设 `N42_LEADER_TENURE=32 / 1000000`、创世无字段：结果与之前完全相同（tenure 取环境，轮换关闭）。
- **没有采用 Profile 1**：协调方撤回了 `n42Profile` 补充要求，本次不处理任何 profile 字段。

## 实施细节

- `view_after_tc(x, T, E, rotate)`：与 gov5 `ViewAfterTimeout` 逐行对应（`x+1` 回绕为 0 时、T≤1、未激活均为
  `x+1`；窗口起点溢出或小于 `x+1` 时为 `x+1`；E>0 且 x≥1 时夹到第一个大于 x 的纪元边界）。
- 引擎：新增字段 `leader_schedule`、`leader_in_set`、`view_after_tc`（纪元长度取 `epoch_manager.epoch_length()`）。
  - `timeout.rs`：`collect_future_timeout`（未来 view 的 TC 及其 next_leader 转发）、`on_timeout` 重复超时成 TC、
    `process_timeout`（next_view 及 next_leader 转发，leader 改按目标 view 的验证者集计算）三处都用 `view_after_tc`；
    `advance_with_tc` 发 NewView 的条件仍是 `is_leader_for_view(next_view)`，next_view 即规则结果；
  - NewView 校验改为 `view_after_tc(tc.view) == nv.view`（替代 `nv.view - 1 == tc.view`），leader 校验不变；
  - `proposal.rs` 与 `leader_index_for_view` 改用引擎自己的 tenure；QC/Decide 路径仍 `+1`，未改。
- `h2_finality::verify_h2_v4_shadow_message` 增加 `schedule` 与 `epoch_length` 参数：leader 用 schedule 的 tenure，
  NewView 的 `checked_add(1)` 检查换成 `view_after_tc`。observer 传入进程 schedule 与自身 `epoch_length`。
- orchestrator：诊断用 leader 计算改用引擎 tenure；`N42_TIMEOUT_VIEW` 诊断不再假定超时 view = 新 view - 1，
  改为找 `view_after_tc` 指向新 view 的超时记录。
- `bin/n42-node/src/main.rs`：`install_genesis_leader_schedule` 在启动最前解析、记录日志并装入。

## 与规范/代码对照

- n42-26 没有 SyncInfo / piggybacked HighTC 采纳路径：H2-v4 桥发出的 Vote/Timeout 的 `high_tc` 恒为 `None`，
  收到的 `high_tc` 只在影子校验里验签，不推动 view。S11 中停在 9 的 n42-26 节点靠 view 16 的 NewView 前进
  （NewView 不受未来窗口缓冲限制），或自己在 9 超时形成 TC(9)，同样到 16。
- 规范 10.5 列的行号（39/200/294/393-399、state_machine 1136）对应 `feat/native-port`；main 上同一位置已核对。
- `process_timeout` 转发超时给下一 leader 时原先用被超时 view 的验证者集算 `view+1` 的 leader，现按目标 view 的集合，
  与规范"n 为目标 view 的集合"一致；只影响转发对象，不影响 TC 形成。
- 纪元长度 E 在 n42-26 来自 `ConsensusConfig.epoch_length`（非创世）；observer 未调用 `with_epoch_schedule` 时 E 为 0，
  只影响 observer 的影子校验对跨纪元夹取的判断。

## 测试

- `view_after_tc_matches_the_standard_vectors`：§10.4 全部 16 个向量。
- `view_sequences_match_the_standard_scenarios`：S1–S11。
- `genesis_leader_rules_and_their_defaults`、`bench_env_overrides_the_genesis_and_says_so`：字段缺省、0、非法类型，整份创世读取，环境覆盖与提示。
- 引擎级：`a_tc_moves_to_the_next_tenure_window_only_when_rotation_is_active`（tenure 4、view 1 的 leader 0 宕机：
  激活时一次 TC 进入 view 4、leader 1 发 NewView；未激活时进入 view 2，不发 NewView）；
  `new_view_must_name_the_view_its_tc_leads_to`（激活时接受 4 拒绝 2，未激活相反）；`rotation_applies_from_its_activation_view`。
- `h2_finality::shadow_new_view_follows_rotate_on_timeout`：H2-v4 影子校验同样规则。

## 状态与后续

- `cargo clippy --workspace --all-targets -- -D warnings` 通过；`cargo test --workspace`（nice、`-j 16`、`--test-threads=16`）1985 通过、0 失败。未跑 E2E 与机群。
- 机群待做：混合 gov5 + n42-26 在创世设 `leaderTenure`>1 与 `rotateOnTimeoutView` 后，停掉一个 leader，确认两端同一 TC 进入同一 view；
  在所有客户端（含 n42-rs）实现前，混合网络创世不得设置 `rotateOnTimeoutView`。

---

## E2E 场景 4 长期失败的修复：同步导入后关闭提交生命周期（2026-11-24）

### 问题

CI `E2E Correctness - smoke-consensus` 的场景 4（correctness profile 的 5 节点段）至少从 2026-11-03（5847275）起每次失败，
结果固定为 `heights=[16, 16, 16, 16, 13]`。本地用 11-20 的 release 二进制按 CI 配置可原样复现。

### 根因

- 最后启动的节点 4 收到第 2 块时缺父块，eager `new_payload` 返回 Syncing；随后 Decide 提交第 2 视图，
  提交生命周期（`async_commit_states`）以 `execution_ready=false` 登记为 Committed。
- 状态同步随后补上第 1、2 块，`import_and_notify` → `handle_valid_import` 用 FCU 把第 2 块设为 head/safe/finalized，
  但没有关闭第 2 视图的生命周期记录。
- `drive_async_commits` 按视图顺序只处理最早未完成的一条：第 2 视图永远停在 Committed，第 3 视图起的 finalize FCU 全被挡住
  （节点 4 日志 `N42_FCU` 0 次，其他节点 16 次）。节点 4 的执行层 head 只在它自己出块时前进，于是 `eth_blockNumber` 落后约一轮 leader。
- Gov5 追块路径（`orchestrator/mod.rs` 约 2120 行）早已在 Valid FCU 后调用 `mark_catchup_commits_finalized`，原生同步路径漏了这一步。

### 修复

- `execution_bridge.rs` `handle_valid_import`：Valid FCU 并推进 execution-validated head 后，调用
  `mark_catchup_commits_finalized(block_hash)`，关闭同一哈希的生命周期记录（并记入已最终化哈希集合，供稍后到达的同哈希提交直接置 Finalized）。
- 新测试 `sync_import_closes_a_commit_that_was_waiting_for_execution`：先提交未就绪的第 2 视图，再经同步导入，
  断言其 Finalized，且第 3 视图的 finalize FCU 正常下发。去掉修复时该测试失败。

### 验证

- `n42-consensus-service` + `n42-node` 测试 644 通过、0 失败；clippy `-D warnings` 通过。
- 本地 E2E 场景 4（`E2E_SCENARIO4_PROFILE=correctness`）：修复前 `[16,16,16,16,13]` 失败；修复后 `[16,16,16,16,16]` 通过，节点 4 `N42_FCU` 14 次。
