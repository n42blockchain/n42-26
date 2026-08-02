# Codex 任务计划：Gov5 ↔ n42-26 互操作总目标收尾

> 收件：OpenAI Codex（真机侧，`/Users/jieliu/Documents/n42/live-interop-20260721/`）
> 日期：2026-07-25
> 计划依据：`docs/gov5-n42-production-interop-plan.md`（总目标与 P0–P6 定义）
> 当前证据：`docs/gov5-n42-production-interop-report.md`（gate ledger 与全部 jsonl）
> 本轮审计：`docs/devlog-135-interop-branch-deep-audit.md`、
> `docs/codex-note-interop-audit-20260725.md`
>
> **提交规范**：不要包含 "Claude" / "Codex" / "Co-Authored-By" 等字样。作者模板：
> ```
> GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
>   git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" -m "..."
> ```

## 一句话

总目标（Gov5 与 n42-26 互为同一条 N42 链的可互换实现）只剩三件事：**P4 收尾**、
**P6 participant 24 小时替换窗口**、**把 39 个提交合进 `main`**。P0/P1/P2/P3/P5
已 PASS，不必重做。

## 锚定事实（已复核，不必重查）

1. `feat/gov5-n42-live-interop` = 39 commits / 14,388 行生产代码，**尚未合入 main**。
   main 目前只有 P0 的 HIGH-1 部分（`3bbad4b`，与该分支的 `a74e347` 逐字节相同，
   外加第二个提交 `3ec35be`），合并时不会冲突。
2. P4 正式窗口自 `2026-07-24T22:06:27Z` 起，acceptance 需 ≥86,400 秒、≥1,400 样本、
   无 >120 秒样本间隙、max lag ≤4、七端点 root 精确一致、零交易连续、warning 与
   deadline 计数器不变。已过 3h/4h 里程碑（`PASS_MILESTONE_ONLY`，不计入 gate）。
3. P6 observer 正式窗口已于 `2026-07-24T14:44:24Z` 关闭（1,433 样本 / 86,677 秒 /
   零失败），只读守卫接管中，participant 激活以 P4 放行为前提。runbook 六步已就绪。
4. 本轮审计新增两项修复在 `fix/gov5-interop-audit-20260725`（已推送，两个提交）：
   HIGH-1 gov5 block 响应 Snappy 解压未受声明长度约束；**HIGH-2 H2 执行门控投票的
   import 证据被挤出致该 view 永久不投票**。后者与 P4 记录的 execution-stall 症状一致。
5. `hardening/gov5-cross-port`（4 commits）与 interop 分支的 gossip 上限常量冲突
   **已在 `integration/gov5-interop-main` 解完并验证**（详见 T6）。

### 2026-07-26 运行态更正

- T2 已选定并部署 `f49422f` / `c0ce2778...`；T7 的 QC 与 TC 三个调用点也已纳入。
- `f49422f` 的 P4 控制进程在 45,186 秒后退出。此前 736 个样本全部健康，但未达到
  86,400 秒且留下不可接受的采样空洞，因此整段保留并排除，P4 仍须从零重跑。
- T9 的 RPC batch 方法域修复已推送为 `6180ec5` + `1b8d52b`。定向回归 7/7、
  format/check/Clippy、完整 `cargo test --workspace`（46 条结果记录、零失败）及隔离
  locked release 构建全部通过；`T9.PASS` 已于 `2026-07-27T06:01:08Z` 关闭，
  pinned SHA-256 为 `b03eb3eddcd14a5b81fac6af900cd12b1819221507308fc0e77965c7edc55fae`。
- pinned binary 已按 Rust-1 → Rust-2 顺序部署，两个独立 5 分钟七端点窗口均零失败，
  max lag 分别为 0 和 1，且实机混合 batch 保持 `n42_*` 单次/批量响应一致。新 P4
  已于 `2026-07-27T06:38:31Z` 从零起表；旧窗口时间没有复用，正式窗口期间禁止换
  binary。86,400 秒阈值不早于 `2026-07-28T06:38:31Z`。
- `2026-07-27T19:42:25Z` 的 12h+ 不可变里程碑为 775 样本 / 46,992 秒 / 零失败 /
  max lag 1 / 最大采样间隙 64 秒 / 推进 8,003 块；warning/deadline 计数器与基线
  完全相同，两 Rust CommitQC/hash 一致且 equivocation 为零。该记录只标记
  `PASS_MILESTONE_ONLY`，不提前关闭 P4。
- P6 participant 从未激活。observer 连续守卫退出后，原 observer 停在 65,537；
  使用原二进制、原数据库重启时在默认 65,536 replay-depth 边界 fail-closed。只读审计
  证明 65,537 条 retained block 全部 parent 完整、空 operations、root 等于认证 base；
  显式使用 131,072 深度后，同一二进制和数据库完整验签、追平并保持只读。T10 的
  continuity v2 已从 `2026-07-27T04:44:25Z` 起表，participant 仍未激活。

### 2026-07-28 运行态更正

- `b03eb3ed` 窗口已于 `2026-07-27T20:17:18Z` fail-closed：此前 810 样本 /
  49,118 秒 / 零 parity 失败 / max lag 1，但两台 Rust 在第 65,538 块同时命中
  `QMDB ancestry exceeds the configured replay depth 65536`。该窗口、日志与数据库
  全部保留并排除，burst 未释放。
- Gov5 当前 main 已在隔离分支
  `integration/gov5-interop-current-main-20260727 @ 912a01d29` 完成整合并推送；
  `go test ./...`、`go test -race ./...` 与两次可复现构建均 PASS，pinned binary
  SHA-256 为 `86b61c2d710e09bf5efddac7631d450278930acd4671e6c74362de8e63057452`。
  五个 Gov5 已逐台停机、保留快照、替换和追平，全程没有同时替换两台。
- replay-horizon 修复已以 `9d26d38` 纳入；const 回归修正后的分支 tip 为
  `8fa9c817c`（已推送）。format/check/Clippy/workspace tests、定向回归与两次 release
  构建全部 PASS，pinned Rust SHA-256 为
  `391185a473ee86f6ae4ec8d9ad7be3a458a7e7994ea7553c6852c64c7d8a236e`。
  两台 Rust 使用原数据库、显式 replay depth `1,048,576` 逐台完成完整回放；替换后
  三轮七端点 height/hash/state-root/receipts-root 精确一致。
- 新基线独立 5 分钟存活预检为 30/30 样本、零失败、max lag 0；正式 P4 已于
  `2026-07-28T03:23:23Z` 以这两个 pinned binary 从零起表，86,400 秒阈值不早于
  `2026-07-29T03:23:23Z`。`main` 仍冻结在 `3bbad4b`，P6
  observer/monitor/guard 继续只读，participant 从未激活。

### 2026-07-28 采样空洞更正

- `03:23:23Z` 窗口的链与零交易检查一直健康，但主机在
  `2026-07-28T06:32:30Z` 因合盖进入睡眠，下一正式样本到
  `08:15:37Z`，形成 6,187 秒空洞，超过 ≤120 秒硬门槛。发现时 204 样本零失败、
  max lag 1、两 Rust committed view/hash 一致、equivocation 为零、warning/deadline
  计数器未变；这不改变窗口必须 FAIL_CLOSED 的结论。
- monitor 已终止，burst 从未释放；正式流、baseline、control launch/failure、日志和
  PID 记录已整体归档到
  `excluded/p4-current-main-replay1m-20260728T032323Z-sleep-gap/`，旧时间不复用。
- formal guard 已增加**运行中** sample freshness 与相邻 gap 双检查，任一超过 120 秒
  会立即 fail-closed，而不是只在窗口自然结束时判定。重开控制链使用 `caffeinate`
  禁止主机睡眠；binary、数据库和七节点拓扑不变，无需重建或替换。
- 同一次睡眠也使 P6 continuity-v2 出现 5,801 秒与 448 秒空洞；其 guard 正确
  fail-closed，v2 已归档并排除于最终交接连续性。observer 节点未重启，新的
  continuity-v3 已于 `2026-07-28T08:39:00Z` 从健康只读样本起表；monitor 和同时检查
  freshness/全历史相邻 gap 的 guard 均已启动，participant 仍未激活。
- 防睡眠断言生效后，新 P4 已于 `2026-07-28T08:40:30Z` 从零起表；独立 preflight
  2/2 PASS，baseline 高度 68,722，首正式样本高度 68,723、lag 0。monitor、实时
  gap guard 和 finalizer 全部存活，86,400 秒阈值不早于
  `2026-07-29T08:40:30Z`。
- `2026-07-28T11:15:30Z` 的 2h+ 不可变里程碑为 153 样本 / 9,247 秒 / 零失败 /
  max lag 1 / 最大采样间隙 63 秒 / 推进 1,571 块。零交易验证连续，warning/deadline
  计数器未变，两 Rust committed view/hash 一致且 equivocation 为零；P6-v3 同期
  156 样本、零失败、最大间隙 62 秒。该记录仅为 `PASS_MILESTONE_ONLY`。
- 截至 `2026-07-29T04:45:03Z`，新 P4 为 1,191 样本 / 72,265 秒 / 零失败 /
  max lag 1 / 最大间隙 63 秒；P6-v3 为 1,197 样本 / 72,325 秒 / 零失败 /
  最大间隙 62 秒。P4 尚未到 86,400 秒门槛。
- P6 current-main/replay1m finalizer 已完成重绑并于 `2026-07-29T04:51:36Z` 在防睡眠
  screen 中 ARMED：锁定 Rust `8fa9c817/391185a4`、Gov5 `912a01d/86b61c2d`、
  P6-v3 新鲜交接流与当前 P4 PASS；participant、替换 marker 均仍不存在。P4 PASS 后
  才允许停 observer、制作两份维护快照并进入 24 小时 participant 窗口。

## 红线（任何任务都不得突破）

- 不初始化、不重建、不格式化、不压缩、不清理、不删除任何既有七节点数据库。
- **正式窗口计时期间不替换二进制**——替换即作废该窗口，必须从零重开。
- 只读 observer 在 participant 激活前不得写共识状态。
- 任何 fail-closed 不变量触发 → 立即 `rollback-replacement`，不做"再看一会儿"。
- 密钥材料只经 `@file` 引用，保持 0600，日志与证据快照扫描不得命中密钥模式。

---

## 任务

### T1 — P4 当前窗口判定（真机 / 阻塞全局）

**当前状态**：`b03eb3ed` 窗口已因 65,536 replay-depth 边界失败并排除。Gov5
`912a01d29` / `86b61c2d...` 与 Rust `8fa9c817c` / `391185a4...` 已完成逐台替换、
原库回放和三轮七端点精确对拍；独立 5 分钟预检 30/30 PASS。新正式窗口已于
`2026-07-28T03:23:23Z` 从零起表，但因主机睡眠造成 6,187 秒采样空洞，现已完整
归档并排除。实时 gap 守卫和防睡眠控制已补齐，下一窗口再次从零起表。
P6 continuity-v3 已先行恢复并通过只读 preflight；新 P4 已于
`2026-07-28T08:40:30Z` 从零重开。
acceptance 仍需 ≥86,400 秒、≥1,400 样本，控制命令留有 90,000 秒运行余量。

**步骤**
1. 让窗口自然跑完，**不要因为 T2 的修复而提前换二进制**。
2. finalizer 按既定门控判定：全样本、历史空块区间、lag 界、warning/deadline 计数器
   全过才释放 17 笔签名交易 burst；burst 后再要求 10 分钟七端点精确 root 存活。
3. 无论 PASS 还是 FAIL，把最终 jsonl 与 SHA-256 写入 report 的 P4 一节。

**验收**：gate ledger 的 P4 行从 IN PROGRESS 变为 PASS 或 FAIL，且有不可变证据支撑。

**FAIL 处理**：若症状仍是 execution stall（某验证者持有提案却长时间不发 R1），
先按 T2 纳入 HIGH-2 再重跑，不要在未纳入修复的情况下重开窗口。

---

### T2 — 纳入审计修复并重建（真机 / 前置 T1）

**前置**：T1 窗口已自然结束（PASS 或 FAIL 均可）。

**步骤**
1. `git merge origin/fix/gov5-interop-audit-20260725` 进
   `feat/gov5-n42-live-interop`（预期无冲突）。
2. 全量门禁：`cargo check --all-targets`、`cargo clippy --all-targets -- -D warnings`、
   `cargo test --workspace`（本机已验证 46 套件零失败）、Go 侧 full + race 套件。
3. 从锁定的隔离 target 重建 release，记录新的 SHA-256 与构建溯源，替换 pinned 值。
4. 若 T1 判定为 PASS：这次重建只影响后续 P6，不需要重跑 P4。
   若 T1 判定为 FAIL：用新二进制从零重开 P4 窗口。

**验收**：新 pinned SHA-256 已记录；两项修复的回归测试在真机构建里同样通过。

---

### T3 — P4 gate 关闭（真机 / 前置 T1、T2）

**前置**：P4 判定 PASS。

**步骤**：把 P4 全部证据（含本轮两项修复的纳入记录）归档为不可变快照，写入 report，
更新 gate ledger。

**验收**：P4 = PASS，且 go/no-go 清单里 "P1 through P4 are green on clean disposable
runtimes" 一项可以打勾。

---

### T4 — P6 participant 24 小时替换窗口（真机 / 前置 T3）

**前置**：P4 PASS；observer 只读守卫与 participant 首个正式样本的交接间隙 ≤120 秒。

**当前状态**：current-main/replay1m finalizer 已 ARMED 并等待当前 P4 PASS；
preactivation gate、pinned binaries、回滚快照前置条件和 continuity-v3 均已验证，
participant 尚未激活。

**步骤**：按 report 已写定的 runbook 六步执行——停 observer 并做 manifest 校验的
状态副本 → 停 Gov5 validator 6 并做第二份维护快照 → 用 validator 6 的精确 BLS 与
secp256k1 PeerID（`@file`）启动 Rust → 其余 Gov5 peer 带 Rust QUIC 地址重启 →
验证两次完整 leader 轮转 + Rust 重启/重入 + 实打实 24 小时精确 root 监控 →
任一 fail-closed 不变量触发即 `rollback-replacement`。

**建议新增一条断言**（针对 HIGH-2 的直接可观测特征）：整窗口内不得出现"某验证者
已收到提案并持有 `pending_proposal`、却在该 view 结束前始终未发 R1"。现有守卫看
lag、root、equivocation 和 Rust-leader 的 build→commit 60 秒配对，**看不到单节点
静默投票**——而 HIGH-2 的表现恰恰是静默而非报错。

**验收**：一个 Rust 验证者连续运行 24 小时（含 leader 槽与一次重启），期间七个
canonical head 与 root 始终一致，零 authenticated equivocation，零安全守卫触发。

---

### T5 — 回滚演练归档（真机 / 可与 T4 并行准备）

go/no-go 清单要求 "operator rollback artifacts and commands have been rehearsed"。
`p6-pre-marker-failure-rollback-regression.jsonl` 已覆盖 pre-marker 缺口，但需要一次
**在非生产副本上主动触发**的完整回滚演练记录：停 Rust → 重开未被触碰的 Gov5
validator 目录 → 七节点恢复一致 → 全程时间线与命令归档。

**验收**：演练 jsonl + 命令清单进入证据 manifest，go/no-go 该项可打勾。

---

### T6 — 分支整合 ✅ 已完成（本机代码侧）

集成分支 **`integration/gov5-interop-main`** 已推送，构成为四路合并：

```
feat/gov5-n42-live-interop (39 commits)
  + fix/gov5-interop-audit-20260725 (本轮两项修复)
  + origin/main (HIGH-1 committed 后台路径 + v0.5.1 收尾)
  + hardening/gov5-cross-port (4 commits)
```

三处实质冲突的解法：

1. `n42-network/src/lib.rs` 两处导出列表冲突 → 取并集。
2. `gossipsub/handlers.rs` 的接收端 Reject 阈值 → 采用 hardening 侧的
   `transport::MAX_GOSSIP_MESSAGE_SIZE` 单一来源（原因见该提交：第二份硬编码副本
   一旦低于发送端，leader 发出的块会被所有 follower 拒收，只留一条警告，view 永远
   不到 quorum），并让 interop 侧新增的 gov5 block topic 共用同一常量——同一条
   transport、同样的块数据，本就是同一个 8 MiB。
3. `orchestrator/mod.rs` 与 `docs/devlog-111` → 取 main 的完整版本（含 committed/
   observer 后台路径的同一 no-blacklist 规则及其回归）。

已验证：`cargo check --all-targets`、`cargo clippy --all-targets -- -D warnings`
（n42-26 侧零告警）、`cargo test --workspace` 46 套件零失败。

**尚未合入 `main`**——等 T2 决定修复纳入时机后再快进，避免 main 与真机构建基线分叉。

---

### T7 — H2-v4 批量验签 ✅ 已完成（本机代码侧）

分支 **`perf/h2-v4-batch-verify`**（基于 `integration/gov5-interop-main`），
详见 `docs/devlog-136-h2-v4-batch-verification.md`。

域与单签名校验函数绑成一个 `Ciphersuite` 值，批量本体与定位坏签名的 fallback 从
同一个值取。两者用不同域是个真实的失效模式：批量在 A 域失败、fallback 在 B 域逐个
"通过"，函数就会报告"没有坏签名"，一批域不符的签名整体进 QC。绑定之后传错域在类型
层面不成立。

实测（纯验签微基准，release）：7 节点 3.69ms→0.76ms（4.84x）、21 节点 6.60x、
100 节点 15.36x、500 节点 230.53ms→9.94ms（23.18x）。端到端 QC 构建收益低于此数
（聚合与序列化是固定开销，devlog-101 的 Native 端到端为 2.56x）。devlog-135 当时写的
"7 节点无实际影响"偏保守——实际也省 2.9ms，只是在 8 秒 slot 里可忽略。

**不改任何 wire 格式、签名域或跨语言向量**，与 P4/P6 窗口无关，可随 T2 一并纳入，
也可单独合入。

#### ⚠️ 请纳入 `e89425b` 而不是 `36e7532`

真机侧记录选定了 `36e7532`。那个 commit **漏了第三个调用点**
`TimeoutCollector::build_tc_with_profile`——TC 构建有一段与 QC 结构相同、元组多一个
字段的逐签名回退，本机 grep 时漏过，而真机侧的并行实现
（`feat/gov5-h2v4-batch-verify`）覆盖了它。已在 `e89425b` 补齐，并补一条 TC 专属
回归：四票含一张错 view 签名，TC 仍从其余三票成立且坏 signer 的 bit 保持为 0。

`perf/h2-v4-batch-verify` 当前 tip = **`e89425b`**，三个调用点齐全，
`clippy --all-targets -D warnings` 零告警、`cargo test --workspace` 46 套件零失败。

---

### T8 — 生产替换扩面（真机 / 前置 T4、T5、T6）

go/no-go 全部打勾后，才允许把第二个、第三个 Gov5 验证者替换为 Rust。每次替换重复
T4 的 24 小时窗口与回滚就绪，不并行替换多个。

---

### T9 — RPC batch 方法域收口 ✅ 已完成（本机 + 真机 / 前置新 P4）

`Gov5H2` 下逐条按请求 method 和 response ID 关联，只允许 `eth_*` 成功响应进入
递归归一化；`n42_*`、`debug_*`、`trace_*`、通知、错误与 ambiguous duplicate ID
保持原样。实现提交为 `6180ec5` + `1b8d52b`，隔离构建 source checkpoint 为
`a72180e`。全工作区门禁与实际 release binary 启动验证均 PASS；不可变清单为
`runtime-11-production-qualification/evidence/t9-rpc-batch-method-scope-pass.jsonl`。
下一轮 P4 与后续 P6 participant 的唯一 Rust binary 基线为
`b03eb3eddcd14a5b81fac6af900cd12b1819221507308fc0e77965c7edc55fae`。

---

### T10 — P6 observer retained-branch 恢复（真机 / 前置 T9、T3）

保留失败 observer 原库与全部日志，不原地修复。先只读解析
`gov5_qmdb_branches.bin`，定位具体失败类别，并与认证 QMDB checkpoint 对拍。若是
missing parent、divergent root 或环，只有能证明待排除项非 canonical 且恢复算法自身
fail-closed 时才允许生成候选副本；若只是完整 canonical lineage 超过配置深度，则提高
显式 replay depth 后仍须从认证 base 完整重放，不能跳过任何 block。候选必须用 pinned
binary 冷启动、追平七 Gov5、保持 `hasCommittedQc=false` 与 vote log 不变，再开启
新的 durable continuity stream。任何无法证明的分支一律拒绝恢复，不得删除文件后
“重新同步”。

---

### T11 — Gov5 当前 main 跟进（本机 + 真机 / 前置新 P4）

`origin/main @ 8797f080` 已不再等同于当前在线 Gov5 基线。更新涉及 durable consensus
transition、vote journal、canonical-chain 偏离诊断、payload 信任边界、eth/68–71
协议范围、direct block transfer 独立 64 MiB 上限、QMDB 增量持久化与 RPC 并发指标，
因此不能用旧 P4 证据替它背书。

隔离分支 **`integration/gov5-interop-current-main-20260727`** 已把在线互操作基线
`a35aa629` 与 `origin/main @ 8797f080` 合并为 **`912a01d29`** 并推送。四处显式冲突
按并集解决：两套跨客户端 wire fixture 全保留；恢复路径同时保留 exact phase/
authenticated recovery view 与 durable vote commitments/divergence report；RPC metrics
保留双检锁。另修复一处自动合并未标冲突：v2 durable vote record 与独立 phase key
必须在公共加载入口统一恢复，否则 v2 重启会丢失 `PhaseTimedOut`。定向
HotStuff/RPC 测试及 `go test ./...` 均 PASS。

`go test ./...` 与 `go test -race ./...` 均 PASS；两次 `make n42` 产物逐字节一致，
pinned SHA-256 为 `86b61c2d710e09bf5efddac7631d450278930acd4671e6c74362de8e63057452`。
在线五个 Gov5 已使用原数据库逐台替换；每台停机后均先生成不可变快照，再启动、追平并
完成七端点 exact-root 检查后才处理下一台。T11 已满足新 P4 前置条件。

---

### T12 — QMDB 65,536 高度边界（HIGH / 前置新 P4）

`b03eb3ed` P4 在 `2026-07-27T20:17Z` fail-closed。此前 810 个样本、49,118 秒、
零 parity 失败、最大 lag 1；第 65,538 块开始两台 Rust 同时报
`QMDB ancestry exceeds the configured replay depth 65536`，随后执行 catch-up 高频重试、
leader build stall、timeout 重播，守卫最终由新增 duplicate-publish 警告终止窗口。
该窗口完整保留并排除，burst 未释放。

修复提交 **`9d26d38`** 把 CLI 的硬编码默认收口到 `qmdb_state_root` 单一常量，并把
有界生产默认提升到 `1,048,576`；资格脚本仍显式传同值，防止以后默认漂移或遗漏。
显式小深度的 fail-closed 行为不变，重启仍从认证 base 完整重放，不能跳块。修复后的
分支 tip `8fa9c817c` 已推送；格式、定向测试、全工作区门禁和两次 release 构建全部
PASS，pinned SHA-256 为
`391185a473ee86f6ae4ec8d9ad7be3a458a7e7994ea7553c6852c64c7d8a236e`。
两台 Rust 已使用原数据库、显式深度 `1,048,576` 逐台完成完整回放并追平，三轮七端点
对拍精确一致。T12 已满足新 P4 前置条件。长期运行还须在逼近该有界容量前形成新的认证
checkpoint；本修复不宣称无限保留。

---

## 依赖图

```
T11 (Gov5 current-main 隔离整合) ─┐
T12 (QMDB 65,536 HIGH 修复) ──────┼─> T1/T2 (新基线 P4 从零重跑)
T9  (RPC batch 收口) ─────────────┘
      └─> T3 (P4 gate 关闭)
           └─> T10 (observer 恢复与连续交接)
                └─> T4 (P6 24h 替换窗口) ──┐
                                           ├─> T8 (扩面)
T5 (回滚演练)  ────────────────────────┤
T6 (分支整合) ✅ 已完成 ───────────────┘
T7 (批量验签) ✅ 已完成
```

## 交接协议

- 真机侧发现的代码缺陷 → 开 `fix/<topic>-<date>` 分支 + 一份
  `docs/codex-note-<topic>.md` 说明根因、触发门槛、修法与是否作废当前窗口。
- 本机侧的代码改动一律先推分支、不直接推 `feat/gov5-n42-live-interop`，由真机侧
  按窗口节奏决定纳入时机。
- 每个 gate 状态变更同步更新
  `docs/gov5-n42-production-interop-report.md` 的 gate ledger 与对应章节。

---

### T13 — Gov5 5.7.906 / current-main 重新锁定（2026-08-02）

Gov5 `origin/main` 已前进到 `920f7536...`，因此此前尚未满 24 小时的窗口已按
fail-closed 规则归档，不复用任何 elapsed time。新候选为
`8915b4cc...`，完整 Go 与 race 测试通过；修复 libmdbx 编译时间戳和 Go build ID 后，
两次清缓存构建均得到 `51e68918...`。

905 数据迁移复核确认：保留/复制的数据库仍是 `b71c2810...` 创世链；906 空目录的
内置 private genesis 却是 `75ca525a...`，仅设置 genesis override 不能改变本地链。
启动脚本因此必须拒绝空目录，并要求固定 SHA 的 genesis artifact、已有 MDBX、准确
BLS key、network key、network metadata 和 epoch schedule。新目录只能用固定 artifact
显式 `n42 init`，或复制已经验证的 905 数据。

五个 Gov 节点逐个替换并保留升级前快照后，301 秒恢复监控 PASS：58 个样本、增长
29 块、最大 lag 0、六端点同高同 hash/stateRoot/receiptsRoot。下一步是从该身份重新
开始完整 24 小时零交易窗口，同时持续核对 Gov5 current main；随后才释放交易 burst、
运行 burst 后窗口与 Rust 重启/重入验收。

严格窗口已于 `2026-08-02T20:37:47Z` 从零开始：formal/resource/upstream 三条
证据流和自动 finalizer 均存活，首样本高度 87,860、lag 0、六端点精确一致；Rust
资源流绑定 PID 97040，上游流绑定 `920f7536...`。交易预检确认六端点 nonce 都是
`0x11` 且发送数为 0。只有完整窗口及后续 burst、post-burst、restart/rejoin 全部
通过后，T13 才能关闭。
