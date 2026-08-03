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

补充 fail-closed 回归已通过：伪造的错误 upstream SHA 以状态 1 退出且没有 completion
marker；空 Gov 数据目录同样在产生 PID 前以状态 1 退出。两条安全门不会静默降级。

---

### T14 — Gov5 latest-main 再同步与 runtime20 严格窗口（2026-08-03）

Gov5 `main` 在 runtime18 运行期间先推进到 `1114f1dd...`，随后在 runtime19
预检期间又推进到 `c611124d...`。两个旧窗口均按 fail-closed 规则排除；runtime18
保留了 24,464 秒、804 个健康零交易样本作为非资格诊断证据，runtime19 尚未开始正式
计时。最新合并候选 `0f688685...` 已推送；新增上游提交仅修改 txflood 的 HTTP 响应体
回收，不触及 genesis、HotStuff 或存储格式。

Gov 全量测试和 `go test -race ./...` 通过；随机负缓存故障模拟器曾单次未触发，随后目标
测试连续 20 次、整个 state 包连续 3 次及全量重跑全部通过。两个独立冷缓存构建逐字节
一致，Gov SHA-256 为 `3a2ed3e0...e0da`。905 血统数据在停止态复制，99 个持久文件的
源/目标清单逐字节一致，清单 SHA-256 为 `19624fe1...a993`；创世仍为
`b71c2810...1392ec`。

runtime20 使用五个 Gov 5.7.906 节点和官方稳定 Reth 2.4.1
(`91725e3aa...`, binary `0a4dbcf3...62b9f`)。正式零交易窗口从
`2026-08-03T04:07:59Z` 重新计时，首样本高度 90,704、lag 0、六端点
hash/stateRoot/receiptsRoot 一致。资源流绑定 Rust PID 12655；Gov 上游流绑定
`c611124d...`。独立预检、交易零发送预检、轮值、超时恢复与日志分区均 PASS。
24 小时后自动执行 17 笔双入口交易、burst 后稳定性、归档/QMDB 对比、Rust 重启重入
和最终独立复验；随后同一官方 Reth 还会做停止态字节快照和额外一小时复验。T14 仅在
两级独立最终 PASS 均生成后关闭。

---

### T15 — Gov5 65a768 current-main 与 runtime21（2026-08-03）

runtime20 的上游监控在 `2026-08-03T04:27:31Z` 检测到 Gov5 `main` 从
`c611124d...` 前进到 `65a76826...`，因此按 fail-closed 规则立即停机并排除。该窗口
保留 41 个全部健康的零交易样本，跨度 1,219 秒，高度 90,704 到 90,836，最大 lag 0；
没有释放任何交易。排除证据 SHA-256 为 `e4e597e0...828d`。

新上游只修改 txflood 与 txpool 容量环境变量，不涉及 genesis、HotStuff 或存储格式。
合并候选 `1331c0df...` 已推送；全量测试、完整 race suite 和两次隔离冷缓存构建均通过，
两个二进制逐字节一致，SHA-256 为 `73d01d1a...8ac6`。runtime20 在停止态的 905
血统数据复制到 runtime21，102 个持久文件的相对路径清单逐字节一致，清单 SHA-256
为 `99ec46f9...cc5`。

runtime21 的六个 RPC 仍报告创世 `b71c2810...1392ec`，并从停止高度 90,836 同高同
hash/stateRoot/receiptsRoot 恢复增长。严格预检覆盖 90 秒、18 个零交易样本，最大
lag 0；两次 Rust 轮值均在六端点形成同一规范块并得到 `5+5`，缺席 Gov6 的 timeout
集合精确且下一视图恢复，日志无未知告警，独立验证器与交易零发送预检均 PASS。
正式 formal/resource/upstream 三流于 `2026-08-03T04:51:02Z` 启动，首样本高度
90,895、lag 0，上游绑定 `65a76826...`；24 小时闭环仍按零交易、burst、重启重入及
官方稳定 Reth 2.4.1 额外一小时的顺序执行。

---

### T16 — Gov5 ddcdaa current-main 与 runtime22（2026-08-03）

runtime21 启动约九分钟后，独立守卫先于十分钟周期上游监控发现 Gov5 `main` 从
`65a76826...` 前进到 `ddcdaa2f...`。全部控制进程和节点立即停止；19 个正式样本
跨度 548 秒，高度 90,895 到 90,955，增长 60 块、最大 lag 0、全程零交易，六端点
latest/pending nonce 都保持 `0x11`。排除证据 SHA-256 为 `7d7dd40d...711a`。

最新上游增加 txpool 配置文件/CLI 路径和滞留交易驱逐测试，不修改 genesis 或
HotStuff。合并候选 `673299ab...` 已推送；全量测试、完整 race suite 和两次独立
冷缓存构建通过，二进制逐字节一致，SHA-256 为 `f84ac8e9...6ea3`。runtime21
停止数据复制为 runtime22，105 个持久文件清单逐字节一致，清单 SHA-256 为
`7d16d977...6d9d`。

runtime22 从高度 90,961 的精确 hash/root 恢复；90 秒严格预检、两次 Rust `5+5`
轮值、timeout 下一视图恢复、日志分区、六端点创世/nonce、CommitQC、零
equivocation、独立验证器和交易零发送预检全部 PASS。新的 formal/resource/upstream
三流于 `2026-08-03T05:14:42Z` 启动，首样本高度 90,991、lag 0，上游绑定
`ddcdaa2f...`。

---

### T17 — Gov5 连续更新、runtime23–27 与 d12257 current-main（2026-08-03）

Gov5 `main` 随后连续推进到 `5afabac1...`、`9c821032...`、`379046b97...`、
`d09b3ad00...` 和 `d12257c92...`。每次变化都由独立上游守卫 fail-closed 检出；
runtime22–26 均保留停止态数据、六端点精确头、Rust `5+5` 轮值和零交易 nonce 证据，
没有把旧 main 上的运行时长计入当前资格。runtime26 在排除前积累了 25 个严格样本、
731 秒、增长 84 块、最大 lag 0，排除证据 SHA-256 为
`a9c8a5235828aec83ec50f78e3ae40eec541087b5e64aa2ecebf586a98386419`。

当前候选 `d0999e7680bfbba71c252de1dd95efe64736e5f9` 合并
`d12257c92e9b1e83d35c981441593663db6db72b` 并已推送。全量
`go test ./internal/... ./cmd/n42/...` 通过，两次独立构建逐字节一致，Gov binary
SHA-256 为 `72e918d9...e3fce95`。runtime27 从 runtime26 的停止态 905 血统数据
复制；124 个文件、17,316,415,839 bytes 的源/目标内容清单完全一致，canonical
manifest SHA-256 为 `1c115b92...37b4b`。创世 artifact 仍为
`56180869...a687`，六端点创世 hash 仍为 `b71c2810...1392ec`，没有误用 906
空目录内置的不同创世。

runtime27 canary 的六端点 hash/stateRoot/receiptsRoot 精确一致；Rust 在 views
95,452 和 95,459 出块并获得 `5+5`，七验证者 CommitQC 存在且 equivocation 为零。
交易、严格独立验证器、最新 Reth rollover 和最新 Reth 独立验证器四项预检均 PASS，
sender latest/pending nonce 保持 `0x11`，发送数为零。正式三流从
`2026-08-03T09:49:44Z` 重新计时，首高度 92,623、lag 0；最终器、不可变日志门、
两级独立验证器、官方稳定 Reth 监控、精确 PID 守卫以及 1/3/6/12/18 小时里程碑均已
挂载。严格 24 小时通过后才执行 17 笔交易、archive/QMDB、burst 后 10 分钟、Rust
重启重入 10 分钟；其后才执行官方稳定 Reth 2.4.1 (`91725e3aa...`) 的额外一小时
验证。总目标只能由原子最终验证器关闭。

首个严格一小时复合里程碑已于 `2026-08-03T10:50:56Z` PASS，未放宽任何门槛。
冻结 head 流为 121 个样本 / 3,651 秒 / 增长 408 块 / 最大 lag 2 / 全程零交易；
resource 流为 13 个样本 / 3,602 秒 / 单一 Rust PID 89930，峰值 RSS 250,096 KiB、
threads 162、FD 93；upstream 流为 7 个精确 `d12257c92...` 样本 / 3,605 秒。
Rust 已记录 71 次 `5+5` 轮值提交，CommitQC 存在、equivocation 为零、失败证据为空。
独立只读复算再次通过；里程碑 SHA-256 为
`40b0c17fd2d512a6ca80593ae22ef902494d82f483d23eba42a6041fcef1506a`。其余 3/6/12/18
小时里程碑及最终自动闭环继续运行，首小时通过不提前释放交易。

首小时后又对同一冻结 Rust 日志完成闭合深审计：高度 92,624–93,073 的 450 个
规范块 parent 连续、六端精确一致，75 个预期 Rust 轮值块与 75 条 stride-seven
提交日志逐一匹配且全部 `5+5`；77/77 timeout/pacemaker 均在下一 view 由 Rust
`5+5` 恢复，630 条 warning 精确归类，未知 warning 和 critical signal 均为零。
冻结 Rust 日志、leader、timeout、日志审计 SHA-256 分别为 `2ac01623...6a18`、
`ce7dab33...48db`、`d8f76e88...1f26`、`1cc39713...7ff0`。冻结控制脚本的语法、
启动记录哈希、依赖命令及最终输出无碰撞检查也全部通过。

额外 90 分钟复合里程碑于 `2026-08-03T11:20:24Z` PASS：head 流 179 个样本 /
5,415 秒 / 增长 606 块 / 最大 lag 2 / 全程零交易；resource 流 19 个样本 /
5,403 秒 / 同一 Rust PID 89930 / 峰值 RSS 253,616 KiB、threads 162、FD 93；
upstream 流 10 个 `d12257c92...` 精确样本 / 5,408 秒。Rust `5+5` 轮值提交累计
104 次，CommitQC 存在、equivocation 和失败证据均为零。独立复算三份冻结流及汇总
再次 PASS，里程碑 SHA-256 为
`28487e6d0d17e05dd33382c06b857180c5bb5ce5482e937cc3ea0c9a8884a158`。一个未产生
输出的 detached waiter 启动被执行环境回收后已隔离；受托管替代运行没有重启节点或
正式流，只有后者计入里程碑。

90 分钟后的只读 archive/QMDB 重跑也 PASS：高度 93,241 的两个参考 proof 在 Gov/Rust
两侧 root、bytes 精确一致并离线验真，11 个历史 RPC 点全部一致，证据 SHA-256 为
`c981bfc5...dbe3a`。`record-gov5-current-canary.sh` 现同时 fail-close 检查六端
chainId、完整 genesis hash/state/receipts roots、sender latest/pending nonce 和客户端
版本，并保持旧 `.genesis` 字符串 schema；错误 nonce 负向回归被拒绝且无链变化。
最终记录器 SHA-256 为 `e4840036...e770`。高度 93,265 的实网检查点确认 chainId
`0x477`、完整创世三元组、nonce `0x11`、六端同头、Gov5 5.7.906、Reth 2.4.1、
CommitQC、110 次 Rust `5+5` 和零 equivocation；证据 SHA-256 为
`3dd0de1c0956375119c1e1a812bd21aab2b0bbb6c0c5962e3a2c550d63442d43`。

可选 2 小时里程碑暴露的是资源审计语义问题，而非节点或共识失败：同一 Rust PID
持续推进时，`du -sk` 的 consensus 已分配块因 compaction 从 87,400 降至 85,532，
随后恢复到 87,580 KiB；head、log bytes、QMDB WAL、资源上限和六端链身份始终正常。
旧审计器错误地把 allocated blocks 当作仅允许 4 KiB 波动的逻辑单调计数器。修复后
允许非负 allocated-block 测量随 compaction 下降并显式记录最大下降，同时继续严格要求
单一 PID、head/log/WAL 单调、采样间隔、链增长和 RSS/thread/FD 上限。1,868 KiB
合成 compaction 与实网快照均 PASS，合成 log-byte 回退仍 FAIL。

新 harness/finalizer/独立 verifier SHA-256 分别为 `037cc547...5309`、
`e116089d...f9c0`、`39b11db6...102d`，两项零突变 preflight 在 nonce `0x11`
通过。只替换了等待控制器；六节点、三正式 monitor、Reth stable monitor、monitor guardian
和 caffeinate PID 均未变化，正式流未重启、未发交易。重绑证据 SHA-256 为
`f534f806...a285c`。修复后的 2 小时里程碑为 254 head 样本 / 7,690 秒 / 增长 864
块 / 最大 lag 2 / 零交易，26 个同 PID 资源样本 / 7,504 秒，13 个精确 Gov5 upstream
样本 / 7,210 秒，148 次 Rust `5+5`、CommitQC、零 equivocation；里程碑 SHA-256
为 `ce33a8b268acb8a85e0b16b1f0b492c6c76c26fc4922dc08b91abf6cb9cf9806`。

随后在 135 分钟再次执行零突变完整身份检查，专门复核 905 数据延续到 906 二进制后
可能变化的创世信息。六端 chainId 均为 `0x477`，创世 hash/state/receipts 三元组仍为
固定预期值；高度 `0x16d65` 的最新 hash/state/receipts 逐字一致，latest/pending nonce
仍为 `0x11`，客户端版本为 Gov5 5.7.906 与官方 Reth 2.4.1，并观察到 156 次 Rust
`5+5`、CommitQC 和零 equivocation。证据 SHA-256 为
`9f1881315a3a11a18d8ee2d6d4c2e8fde652cea285b9057b8be313e4603effb6`。

140 分钟统一冻结日志深审进一步覆盖高度 92,624 至 93,601 的 978 个连续区块；
163 个预期 Rust 轮值在六端 canonical 完全一致，163/163 日志提交均为 `5+5`，
view stride 和 hash 顺序精确。165 次 timeout/pacemaker 全部由下一 view 的 Rust
`5+5` 恢复，pending 为零；1,351 条 warning 全部归入允许分类，未知 warning 和
critical signal 均为零。冻结日志、leader、timeout、runtime-log 证据 SHA-256
分别为 `a1ad313e...515a`、`1eb7eeb5...8bcc`、`56dcb732...d37b`、
`f096a71e...d54c`。

150 分钟固定路径滚动组合审计同样 PASS：299 个 head 样本 / 9,055 秒 / 增长
1,014 块 / 最大 lag 2，31 个同 PID 资源样本及 16 个精确 upstream 样本。第二次
实网 compaction 的单步下降达到 1,944 KiB，consensus allocation 相对起点净变化
为 -740 KiB，但 head/log/QMDB WAL 仍全部单调且资源上限正常，再次实证修正后的
allocated-storage 语义。组合摘要 SHA-256 为
`349481a3ee0b4a7ab934345deb140878e06b9a612cf22e99d519c99f7120faa0`。

正式三小时里程碑及独立重跑均在未放宽验收条件下 PASS：358 个 head 样本 /
10,845 秒 / 增长 1,218 块 / 最大间隔 31 秒 / 最大 lag 2 / 连续零交易；37 个
Rust PID 89930 资源样本 / 10,806 秒，RSS 最大 268,000 KiB、线程 162、FD 93，
head/log/WAL 单调且正确记录 1,944 KiB compaction；19 个 Gov5 main 精确样本 /
10,815 秒。里程碑记录 206 次 Rust `5+5`、CommitQC、七验证者、零双签与零交易，
SHA-256 为 `953e03d8...d782`；独立重审 SHA-256 为 `1e4f3179...76eb`。

同刻身份检查仍证明 chainId `0x477`、完整固定创世三元组、六端同头、nonce `0x11`、
Gov5 5.7.906、官方 Reth 2.4.1 和零双签，SHA-256 为 `b2afa7bc...c3b1`。
永久资源审计回归脚本 `scripts/test-gov5-resource-auditor.sh` 已覆盖 compaction PASS，
以及 log/WAL/head 回退、PID 变化、非正 allocation、过大采样间隔必须 FAIL；脚本
SHA-256 为 `73822807...f7f`。

完整三小时 canonical leader 深审独立扫描高度 92,624 至 93,841：1,218 个区块
父链连续，203 个预期 Rust 轮值在六端逐字一致，203/203 日志均为 `5+5`，七 view
stride 与 hash 顺序精确。同一冻结日志包含 212 组 timeout/pacemaker，全部由下一
view 的 Rust `5+5` 恢复且 pending 为零；1,732 条 warning 全部精确分类，未知
warning 与 critical signal 均为零。冻结日志、leader、timeout、runtime-log
SHA-256 分别为 `4609b765...aac7`、`cd9e2e38...4876`、`8598364e...764b`、
`6ffb346d...7a18`。

三小时依赖交付复核也 PASS：混合客户端组合分支精确为已推送的 `ab058386...`，
Reth 交付分支精确为已推送的 `91725e3...`，依赖升级分支精确为已推送的
`aec34a0...`；三者 tracked worktree 均 clean 且远端分支与本地 HEAD 完全一致。
Gov5 candidate `d0999e7...` 仍绑定 upstream main `d12257c...`，官方最新稳定 Reth
仍是 v2.4.1 / `8eb21017...`，Gov5/Rust 二进制 SHA-256 仍为 `72e918d9...` /
`0a4dbcf3...`。机器证据 SHA-256 为
`9d3fbf70a7725ed906bf37fa873c3b5b73624137ec12c137238b4a93c9d27b54`。

三小时 905 数据静态边界复核也 PASS：初始复制证据仍将 124 文件 /
17,316,415,839 bytes 绑定到相同源/目标 manifest SHA `1c115b92...`；六个保留
Gov 数据目录中的 24 个 epoch schedule、network config/key、BLS keystore 文件
当前 SHA 均与初始复制一致。创世、consensus/bootstrap、验证者/P2P 密钥、冻结
harness/finalizer/独立 verifier/QMDB verifier 及两个二进制也保持固定 SHA。
运行 chaindata 因正确出块必然变化而按设计排除。证据 SHA-256 为
`b1b4306dc929720719058960d68430344f0b68cc282a226b27ce4d6e45d20955`。

三小时门槛后的只读 archive/QMDB 检查点同样 PASS：当前参考高度 93,871 的两份
Gov5 account proof 与 Rust QMDB proof root/bytes 完全一致并通过离线验证；从创世到
5,189 的 11 个固定历史高度完成 209 项 block/receipt/log/state/storage/proof 检查，
Gov5/Rust RPC 全部精确。六端 pending nonce 仍为 `0x11`，未发送交易；证据
SHA-256 为 `c9336afeb6958cddb2f60f9017c43a242a56f042cbd7cbd822f1b499585ba4be`。

正式四小时复合里程碑继续在未放宽验收规则下 PASS：477 个 head 样本覆盖
14,453 秒并增长 1,626 块，最大采样间隔 31 秒、最大 lag 2、全程零交易；49 个
同一 Rust PID 89930 的资源样本覆盖 14,407 秒，峰值 RSS 269,808 KiB、线程 162、
FD 93，head/log/QMDB WAL 单调并保留 1,944 KiB compaction 记录；25 个 Gov5
upstream 样本覆盖 14,422 秒且全部精确匹配 `d12257c...`。Rust 已累计 274 次
`5+5` 轮值提交，七验证者 CommitQC 存在且 equivocation 为零。里程碑 SHA-256
为 `e5c64c8987a930b9b1a610322d554bdf45a323d760f0845388378da09a495585`。
6/12/18 小时等待器、24 小时最终器、重启重入和最新稳定 Reth 额外一小时闭环继续
挂载；本里程碑未释放交易，也不提前关闭总目标。

同边界独立不可变日志深审扫描高度 92,624 至 94,262：1,639 个规范块父链连续，
274 个预期 Rust 轮值在六端精确一致，274/274 提交均为 `5+5`，view stride 与
hash 顺序精确。276 组 timeout/pacemaker 全部在下一 view 恢复且 pending 为零；
2,252 条 warning 全量归入允许分类，未知 warning 和 critical signal 均为零。
冻结 Rust 日志、leader、timeout、runtime-log SHA-256 分别为
`a185811f...8e55`、`53270ea6...2ebe`、`59c90704...4076`、
`e52303d2...100a`。

四小时门槛后的第二次只读 archive/QMDB 复核也 PASS：当前高度 94,303 的两份
Gov5 account proof 与 Rust QMDB proof 根和字节精确一致并通过离线验证；创世至
5,189 的 11 个历史高度再次通过 209 项 RPC/proof 检查。六端 pending nonce 仍为
`0x11`，未发送交易或重启进程；证据 SHA-256 为
`1060c76b310359b3655a43d0d9c517933290a91eacb3b91cbf5c39ba74785974`。
两次仅包装层的诊断已置于 `excluded/`：一次误拼 verifier 环境变量并在输出前失败，
一次误把实际的 1 条 live proof 加 11 条历史记录断言为总计 11 条；正确绑定的证据
仅生成一次，并以 1+11 schema 原地复核通过。

同刻四小时链身份 canary 再次证明六端 chainId `0x477`、完整固定创世
hash/state/receipts 三元组和当前块身份均精确一致，sender latest/pending nonce 均为
`0x11`，客户端仍为 Gov5 5.7.906 与官方 Reth 2.4.1。Rust 已记录 285 次唯一
`5+5` 提交，七验证者 CommitQC 存在且 equivocation 为零；身份检查 SHA-256 为
`3e554ff12f4efcc56b501df7640bb01d6e197e9d9423cf69b62f22f26e3142fb`。

四小时 905 数据静态边界复核同样 PASS：初始复制仍绑定 124 文件、
17,316,415,839 bytes 和相同源/目标 manifest SHA `1c115b92...37b4b`；六个 Gov
数据目录的 24 个 epoch schedule、network config/key、BLS keystore 静态文件均
与初始 SHA 一致。创世、共识/bootstrap、验证者/P2P 密钥、冻结工具和两端二进制
也保持锁定值。运行 chaindata 因正常出块必然变化而继续按设计排除；检查未执行
任何突变，证据 SHA-256 为
`4322ede81bd6d5102cad96e94e35ede59d899bafa458178b8dd7347768c47381`。

四小时依赖交付复核也 PASS：主分支、Gov5 candidate、混合客户端组合、Reth 交付和
依赖升级五个分支均 tracked clean 且与已推送远端 HEAD 精确一致。Gov5 仍为
candidate `d0999e7...` / upstream main `d12257c...`；官方最新稳定 Reth 仍为
v2.4.1，tag object `8eb21017...`，两端二进制 SHA 也未漂移。机器证据 SHA-256
为 `5b7eb21ebc003aafb71ff3b11b105fae4d10aab790047c0d1326cdfef8db6cbe`。

额外五小时复合里程碑在未放宽规则下 PASS：595 个 head 样本覆盖 18,031 秒并
增长 2,034 块，最大间隔 31 秒、最大 lag 2、全程零交易；61 个同 PID 资源样本
覆盖 18,009 秒，峰值 RSS 275,616 KiB、线程 162、FD 93，head/log/QMDB WAL
单调且保留 1,944 KiB compaction 记录；31 个 Gov5 upstream 样本覆盖 18,029 秒
并全部精确匹配 `d12257c...`。里程碑记录 342 次 Rust `5+5`、七验证者 CommitQC、
零双签、零交易和空失败流，SHA-256 为
`cffb11780ddee8aca95cefdbe2234ede2309e477bdc09523328f118b154b3d68`。

五小时独立不可变日志深审扫描高度 92,624 至 94,664：2,041 个规范块父链连续，
341 个预期 Rust 轮值在六端精确一致，341/341 日志均为 `5+5`，view stride 和
hash 顺序精确。343 组 timeout/pacemaker 全部在下一 view 恢复，pending 为零；
2,794 条 warning 全量归类，未知 warning 与 critical signal 均为零。冻结 Rust
日志、leader、timeout、runtime-log SHA-256 分别为 `7390709d...3bec`、
`dfa0365f...eeea`、`6ac00a2a...da71`、`a9c2593d...031c`。

正式六小时复合里程碑在未放宽验收规则下 PASS：715 个 head 样本覆盖 21,668 秒
并增长 2,412 块，最大间隔 31 秒、最大 lag 2、全程零交易；73 个同 PID 资源
样本覆盖 21,610 秒，峰值 RSS 275,616 KiB、线程 162、FD 93，head/log/QMDB
WAL 单调并记录 1,944 KiB compaction；37 个 Gov5 main 精确样本覆盖 21,634 秒。
里程碑记录 405 次 Rust `5+5`、七验证者 CommitQC、零双签、零交易和空失败流，
SHA-256 为 `c906d490bff8e62eeb741191cc4d4e9e1b44b9e0609651e56af9e15d18d9ef74`。
12/18 小时等待器与完整受控闭环继续挂载。

同刻六小时链身份 canary 再次证明 chainId `0x477`、完整固定创世三元组、六端
当前块身份和 sender nonce `0x11` 均精确一致；客户端仍为 Gov5 5.7.906 与 Reth
2.4.1。Rust 已记录 406 次唯一 `5+5`，七验证者 CommitQC 存在、零双签；证据
SHA-256 为 `2db923d8521e310b4cd55af0a7be36a4d56a3a0ff941e5a5a20a7c349a5fd15a`。

六小时只读 archive/QMDB 检查也 PASS：当前高度 95,047 的两份 Gov5 proof 与
Rust QMDB proof 根和字节一致并离线验真；11 个历史高度再次通过 209 项精确检查，
六端 pending nonce 仍为 `0x11`。未发送交易或重启节点；证据 SHA-256 为
`1e4c44543cb8561096d5fcc6f84ac6e33252f2c1116e0d627bd332b2d6849dcc`。

六小时独立不可变日志深审扫描高度 92,624 至 95,048：2,425 个规范块父链连续，
405 个预期 Rust 轮值在六端精确一致，405/405 日志均为 `5+5`，view stride 和
hash 顺序精确。407 组 timeout/pacemaker 全部在下一 view 恢复，pending 为零；
3,309 条 warning 全量归类，未知 warning 与 critical signal 均为零。冻结 Rust
日志、leader、timeout、runtime-log SHA-256 分别为 `bfee67d8...2327`、
`f2606ff2...17dc`、`8a089dd9...36c7`、`0ea18d51...f889`。另已挂载只读九小时
复合等待器，缩短 6→12 小时观察间隔。

额外七小时复合里程碑继续在未放宽验收规则下 PASS：833 个 head 样本覆盖
25,245 秒并增长 2,784 块，最大间隔 31 秒、最大 lag 2、全程零交易；85 个同一
Rust PID 89930 的资源样本覆盖 25,212 秒，峰值 RSS 275,760 KiB、线程 162、
FD 93，head/log/QMDB WAL 单调并保留 1,944 KiB compaction 记录；43 个 Gov5
upstream 样本覆盖 25,239 秒且全部精确匹配 `d12257c...`。里程碑记录 467 次
Rust `5+5`、七验证者 CommitQC、零双签、零交易和空失败流，SHA-256 为
`167b2c53ef9819cbec0ee2dd5abf4e6532da964406b57e46501564b911829756`。

七小时冻结日志增量审计进一步扫描六小时后的 Rust 槽位高度 95,054 至 95,407：
59 个预期 Rust 规范块在六端精确一致，59/59 均为 `5+5`，父链、view stride 和
hash 顺序精确。累计 467 组 timeout/pacemaker 全部在下一 view 恢复且 pending
为零；3,798 条 warning 全量归类，未知 warning 与 critical signal 均为零。
冻结 Rust 日志、leader、timeout、runtime-log SHA-256 分别为
`366baf19...edf4`、`8086be8c...b9e`、`ce96ce70...efd`、
`66cd4e49...88f8`。24 小时前仍不发送交易或重启节点。

额外八小时复合里程碑继续在未放宽验收规则下 PASS：953 个 head 样本覆盖
28,884 秒并增长 3,192 块，最大间隔 31 秒、最大 lag 2、全程零交易；97 个同一
Rust PID 89930 的资源样本覆盖 28,814 秒，峰值 RSS 276,064 KiB、线程 162、
FD 93，head/log/QMDB WAL 单调并保留 1,944 KiB compaction 记录；49 个 Gov5
upstream 样本覆盖 28,844 秒且全部精确匹配 `d12257c...`。里程碑记录 535 次
Rust `5+5`、七验证者 CommitQC、零双签、零交易和空失败流，SHA-256 为
`ba9bb4ed1f2800cea120da2e03def11fdd96a0f9d698adb687fc7a6651b51c0e`。

八小时冻结日志增量审计扫描七小时后的 Rust 槽位高度 95,408 至 95,815：68 个
预期 Rust 规范块在六端精确一致，68/68 均为 `5+5`，父链、view stride 和 hash
顺序精确。累计 535 组 timeout/pacemaker 全部在下一 view 恢复且 pending 为零；
4,346 条 warning 全量归类，未知 warning 与 critical signal 均为零。冻结 Rust
日志、leader、timeout、runtime-log SHA-256 分别为 `d81f611a...4df2`、
`72a2e549...bb9d`、`aa5cf464...a6de`、`c961ced4...31d1`。

---

### T18 — Gov5 b8c17d current-main 与 runtime28（2026-08-03）

runtime27 第九小时期间，Gov5 `main` 从 `d12257c92...` 前进到
`b8c17d046...`。严格上游门按预期失败关闭；八小时 PASS 证据保留，但该运行被排除，
没有释放交易。新提交只修改 `internal/txlookup`：segment 可从任意 source 按交易数
构建，并增加可从 durable block bodies 重建的内存 tail。现有 905 血统数据没有
`txindex.ranges`，按兼容路径读取，无需破坏性迁移。

候选 `a2da47a70f6c83c765d8a626b86ac383a4fb9551` 已推送并精确包含
`b8c17d04614346bace2fbb5c05393bdaf454cf5a`。`go test ./...`、
`go test ./internal/txlookup` 和 `make n42` 均通过；两次构建逐字节一致，Gov binary
SHA-256 为 `705abbb2...664`。Reth 保持官方稳定 v2.4.1 / source
`91725e3aa...` / binary `0a4dbcf3...62b9f`。

runtime28 从停止态 runtime26 克隆并重新核验全部 124 个持久文件、
17,316,415,839 bytes；源/目标 records SHA-256 均为 `1c115b92...37b4b`。
创世 artifact SHA-256 仍为 `56180869...a687`，六端 chainId 为 `0x477`，创世
hash 为 `b71c2810...1392ec`。canary 在 views 95,452/95,459 记录两次 Rust
`5+5`，六端同头、CommitQC 存在、零 equivocation，SHA-256 为
`13c087af...a914`。

新的严格零交易流从 `2026-08-03T19:15:19Z`、高度 92,695、lag 0 重新计时。
head 监控请求 86,640 秒，resource/upstream 请求 87,000 秒；Gov5 main 和官方
Reth stable 均被独立持续监控。最终器 mutation-free preflight 在六端 nonce
`0x11` 通过，交易发送数为零；1/3/6/8/12/18 小时等待器、最终器和 30 小时防休眠
均已挂载。只有完整 24 小时门通过后才允许执行 burst、archive/QMDB、Rust 重启追高
及后续最新 Reth 附加验证。

正式流首次启动约七分钟后，静态生命周期审计发现 supervisor 会在 head monitor
正常完成 86,640 秒时误判退出，并提前终止仍需运行到 87,000 秒的 resource/upstream。
该短流已隔离；修正后的 wrapper 会把成功完成的 monitor 托管到最终关闭。节点、链数据、
最终器和 nonce 均未重启或改变，正式时长仅从上述新起点计算。

修正流十分钟复合里程碑 PASS：22 个 head 样本覆盖 637 秒并增长 66 块，最大
lag 0、全程零交易；3 个同一 Rust PID 资源样本覆盖 600 秒；2 个 Gov5 main
精确样本覆盖 601 秒。Rust 累计 26 次 `5+5`，CommitQC 存在、零双签；里程碑
SHA-256 为 `723db1a6...fd29`。只读 archive/QMDB 初检也通过 1 个当前 reference
proof 和 11 个历史高度的精确 Gov/Rust 对比，证据 SHA-256 为 `6b814b2f...ce1`。

早期闭合日志深审扫描高度 92,696–92,797 的 102 个连续规范块；17 个预期 Rust
轮值在六端精确一致，17/17 日志提交均为 `5+5`，view stride 和 hash 顺序精确。
31/31 timeout/pacemaker 全部在下一 view 由 Rust `5+5` 恢复，pending 为零；
251 条 warning 全量归类，未知 warning 和 critical signal 均为零。冻结日志、
leader、timeout、runtime-log SHA-256 分别为 `f976d11c...95e8`、
`81e9e574...f097`、`aa9eff62...14c`、`0dc84ad1...54a`。两个诊断截面恰在
timeout 与恢复 view 之间，已移入 `excluded/`，不作为闭合证据。

十五分钟复合门进一步 PASS：41 个 head 样本覆盖 1,213 秒并增长 126 块，最大
lag 0、连续零交易；5 个同 PID 资源样本覆盖 1,200 秒；3 个 Gov5 main 精确样本
覆盖 1,201 秒。Rust 累计 36 次 `5+5`、CommitQC、零双签；里程碑 SHA-256 为
`0e236d19...9719`。

905 血统静态边界审计重算六个 Gov 数据目录中的 24 个 epoch schedule、network
config/key 和 BLS keystore，全部与初始 124 文件复制清单精确一致。创世、共识/
bootstrap、Rust 验证者/P2P 密钥、冻结工具与两端 binary 哈希也保持锁定；正常推进的
chaindata 明确排除。证据 SHA-256 为 `6ea80521...203c`。

随后又对复制执行边界做了只读块身份复核：六端在创世、bootstrap 高度 29、复制态
持久头 92,605 及其前后块、初始 archive 头 92,677 和实时共同高度 92,857 上，
number/hash/parentHash/stateRoot/receiptsRoot/transactionsRoot/miner/交易数均逐项
一致。复制头仍为 `b88a3571...5a82`，其后一块由 Rust 按轮值产生；证据 SHA-256
为 `04f58aef...2e82`。

复制边界还增加了最终独立门：提交 `6fc5d326...bae2` 的验证器已在 nonce `0x11`
完成零变更预检，并持续重放上述 7 个历史块。它等待原子总验收 PASS 后，才会在最新
Reth 进程上再次要求 nonce `0x22`、六端实时头精确、CommitQC 和零双签。启动证据
SHA-256 为 `078089fc...bb6`。

正式运行一小时后再次从初始证据重算静态边界，而不是复用原结论：24 个 Gov 静态
文件、创世/共识/bootstrap、Rust 验证者与 P2P 密钥、冻结验收工具以及 Gov5/Reth
二进制全部保持精确；原始 124 文件复制清单仍为 PASS，推进中的 chaindata 按设计
排除且未发生写操作。证据 SHA-256 为 `dee51343...929a`，可重复复核脚本为
`scripts/recheck-gov5-runtime-static-boundary.sh`。

三小时门另挂载 fail-closed 深审计 V2 PID 71290。它只在原复合里程碑 PASS 后冻结六端
日志，并从正式首个 Rust 块 92,696 扫描到闭合 Rust 块，逐块检查六端规范链、六块
轮值节奏、顺序 `5+5`、timeout 下一 view 恢复、warning 全量分类和 critical signal；
随后再次执行 905 静态边界复核。80 分钟全路径演练正确发现 V1 的冻结边界竞态：选择
历史最近 Rust 块后，复制前可能已记录下一 timeout、复制后才出现 recovery，因而严格
门以 pending=1 拒绝。提交 `5b855bab...ed4` 改为先同时确认 live log 与 committed view
已含最新 timeout 的下一 view `5+5` recovery，再选择并冻结闭合块。

修复后的 V2 全路径 PASS：高度 92,696–93,218 的 523 块六端连续精确，88 个 Rust
轮值全部顺序 `5+5`，102/102 timeout 下一 view 恢复且 pending 0，824 条 warning
全量归类、unexpected/critical 均为 0，24 个静态 Gov 文件仍精确。复合证据 SHA-256
为 `5210a5e8...fbff`，冻结 V2 SHA-256 为 `aea2c249...a73`，预检为
`8e894c62...6565`。V1 部分证据和旧启动均可恢复地移入 `excluded/`；节点、数据、
nonce 与正式计时未变化。

80 分钟 head 快照唯一 lag 1 出现在 `20:16:57Z`：共同高度 93,071、最快端
93,072；31 秒后样本已在 93,073 恢复 lag 0。独立固定高度重读证明 93,071–93,073
在六端的 number/hash/parent/state/receipts/transactions root、miner 与交易数全部
一致，父链连续且零交易；全区间深审计还继续覆盖到 93,218。因此该行是 RPC 采样
边界竞态而非规范分叉，证据 SHA-256 为 `d5b3339b...f1a3`。

90 分钟复合门继续 PASS：180 个 head 样本覆盖 5,426 秒并增长 552 块，唯一 lag 1
仍是上述已闭合采样行，没有新增 lag；19 个资源样本均为原 Rust PID 70765，RSS 峰值
248,256 KiB、线程 161、FD 93；10 个 Gov5 main 样本精确。Rust 累计 108 次
`5+5`，CommitQC、零双签、零交易；里程碑 SHA-256 为 `cc440f07...53cd`。

冻结的 90 分钟资源序列进一步显示：原 PID 70765 在 5,402 秒内增长 552 块，RSS
端点增长 20,288 KiB、约 13,520 KiB/小时；按同一线性斜率投影 24 小时约
550,152 KiB，低于 1 GiB 门限。线程固定 161、FD 固定 93；Reth 数据约
133 KiB/小时、QMDB WAL 约 60 KiB/小时，consensus 数据无增长。证据 SHA-256
为 `0d4fbf81...9699`。

五个 Gov5 进程的 90 分钟侧审计也 PASS：原 PID 70737/43/49/55/61 均未替换，
进程运行约 1:47；全部报告 `N42/5.7.906`、chainId `0x477`，共同高度 93,271、
lag 0，并与 Rust 在固定块八项身份及创世 hash 上完全一致。RSS 为
143,152–145,168 KiB，相比一小时截面最大仅增 4,832 KiB；线程 18–19、FD 34。
证据 SHA-256 为 `3f5326c4...989f`。

90 分钟生产者全量审计覆盖高度 92,696–93,277 的 582 块、97 个完整轮次。六端每一
块的 number/hash/parent/state/receipts/transactions root、miner 与交易数序列完全
相同，各端序列 SHA-256 均为 `95259664...27dd`；父链连续且全程零交易。Rust 与
五个 Gov 地址各自产生恰好 97 块，每个 modulo-six 槽位始终绑定同一生产者；配置中
第七个缺席验证者继续由 timeout 恢复审计覆盖。证据 SHA-256 为
`03e36dd5...d336`。

网络/共识矩阵进一步区分 execution peer 与混合共识通道：五个 Gov 端点各报告 5 个
devp2p peers；Rust 执行层 peer API 为 0，但 PID 70765 实际建立到 Gov
30301–30305 的五条连接，并认证索引 1–5 的五个 validator peers。最新 leader build
连接数 5、quorum 需求 4、direct push 5，Rust view 96,271 以 `5+5` 提交；
CommitQC、零双签，status committed hash 在六端反查完整身份一致。证据 SHA-256
为 `7e7df6e0...1f8d`；可在 Reth 重启后复用的审计器为
`scripts/audit-gov5-mixed-network-matrix.sh`。

提交 `3093cc7f...8c5f` 又增加最终 post-rollover 网络门 PID 94851。它等待含最新
Reth 附加一小时的原子总验收 PASS 后，动态绑定新 Rust PID 并重新执行 socket、认证
validator peers、quorum/direct push、`5+5`、CommitQC、零双签与 committed block
六端反查，同时要求六端 latest/pending nonce 均为 `0x22`。冻结验证器 SHA-256 为
`be0471b4...c809`，nonce `0x11` mutation-free 预检 SHA-256 为
`f8a6529c...d729`。该门不发送交易，也不改变已有总验收与复制边界门。

提交 `71d11a6b...bf65` 增加最终目标级 completion auditor：只有原子总验收、复制
905 边界门和 post-rollover 网络门三者均独立 PASS，且所有仓库/远端 pin、官方稳定
Reth tag、两端二进制、六端 live identity/genesis、CommitQC、零双签、空失败流与
latest/pending nonce `0x22` 全部重检通过，才发布目标完成证据。审计器 SHA-256 为
`b87aa985...b3f0`，冻结 mutation-free 预检为 `6591008d...1f5f`，当前明确记录
`completionNotClaimed=true` 和 nonce `0x11`。最终文档推送后才手动执行，避免其记录
的 primary HEAD 被自身交付文档再次推进。

超过 100 分钟后再次执行 905 数据谱系审计：正式流 6,366 秒、增长 648 块时，五个
Gov datadir 仍均无 `txindex.ranges` 和 migration marker，未发生破坏性迁移；每节点
分配空间相比早期兼容截面仅增 24 KiB。24 个静态 Gov 文件仍精确；创世、复制持久头
92,605 前后及全部 7 个边界高度再次六端一致，创世仍为 `b71c2810...1392ec`，复制头
仍为 `b88a3571...5a82`。静态与复合证据 SHA-256 分别为 `1588df47...32e1`、
`64428199...bf23`。

严格两小时复合门与 V2 闭区间深审均 PASS。head 流 240 个样本覆盖 7,245 秒，链从
92,695 推进到 93,433、增长 738 块，最大 lag 1 且零交易；原 Rust PID 70765 的 25
个资源样本覆盖 7,203 秒，RSS 峰值 248,256 KiB、线程最多 163、FD 93；13 个 Gov5
上游样本连续精确。深审扫描 92,696–93,434 的 739 个规范块，124 个 Rust 槽位全部
六端精确并按序 `5+5`；138/138 timeout 均在下一 view 恢复、pending 0，1,113 条
warning 全量归类且 unexpected/critical 为 0，24 个 905 静态文件再次精确。复合与
深审证据 SHA-256 分别为 `0a4f2057...b314`、`e4e87236...61ef`；未发送交易、未替换
进程、未修改数据。

两小时后的网络矩阵再次 PASS：五个 Gov execution peer count 各为 5；Rust execution
peer count 为 0，但原 PID 70765 到 Gov 30301–30305 的五条共识 TCP 均已建立，并认证
五个唯一 validator peer。leader build 连接 5、quorum 需 4、direct push 5，view
96,432 以 `5+5` 提交；CommitQC 存在、双签为零，committed block 六端身份精确。
只读证据 SHA-256 为 `ac1234e5...9ab0`。

两小时 archive/QMDB 复核也 PASS：共同高度 93,457 的两份 Gov/Rust reference proof
在 root 与编码字节上完全一致并通过冻结离线 verifier；创世、bootstrap 边界及高度
999–5,189 的 11 个历史点共 209 项 RPC/root/proof 检查全部精确。只读证据 SHA-256
为 `959cb74e...d3af`。

两小时后又以最新已推送主仓库 HEAD `07682df3...5ccf2` 重跑冻结 completion auditor
的 mutation-free 预检：主仓库动态 HEAD/远端一致，Gov5/Reth/deps 固定 pin、两端
二进制、六端身份、创世与 nonce `0x11` 全部精确；结果仍明确
`completionNotClaimed=true`。这证明中途证据文档提交不会让最终门误报源码漂移。
证据 SHA-256 为 `76ddcd3e...2396`。

新增 `scripts/audit-gov5-burst-readonly.sh`（SHA-256 `4fb70ee3...1ae4`）并在两小时
状态重跑最终 17 笔 burst 的只读执行审计。所有 raw 签名均恢复到预期 sender，chainId
均为 `0x477`，nonce `0x11–0x21` 连续、17 个声明 hash 全部精确，计划入口 Rust/Gov
为 9/8。六端 latest/pending nonce 仍为 `0x11`，部署和转账 `eth_call` 全成功；Gov
部署估算 `0x12799`、Rust `0x12b0c`，均低于签名 gas `0x186a0`，转账均精确
`0x5208`。审计发送 0 笔交易，证据 SHA-256 为 `206b8ba4...bc3d`。

该 burst 审计器的负向夹具把第 9 笔声明 hash 置零后被立即拒绝（退出码 1），且没有
产生 PASS 文件或触碰活动运行；证据 SHA-256 为 `2b6f305f...b6af`。同期实时资源
重审覆盖原 Rust PID 70765 的 27 个样本 / 7,803 秒 / 798 块，RSS 峰值 253,088
KiB、线程最多 163、FD 93；head/log/WAL 等逻辑计数全部单调，证据 SHA-256 为
`65e04bac...896e`。

长测主机容量审计 PASS：正式流 267 个样本 / 8,064 秒、最大间隙 31 秒、坏行 0；
数据卷可用 730,728,404 KiB，runtime 当前 18,002,992 KiB。即按极保守的每小时
1 GiB 增长并另留 64 GiB，也只投影到 44,217,392 KiB。caffeinate PID 72825 的
system/user/disk sleep assertions 均有效，剩余 99,502 秒；覆盖 87,336 秒的严格
上游窗口、post-window、附加一小时和收尾预算后仍余 12,166 秒。证据 SHA-256 为
`5f712367...bef9`。

新增可重复的 `scripts/audit-gov5-six-producer-range.sh`（SHA-256
`37aace7a...e003`）。稳定版原子保留六端 raw JSONL，并全量扫描高度
92,696–93,565 的 870 个块、145 个完整六槽轮次；六个 RPC 端点各自 870 行的完整
number/hash/parent/state/receipts/transactions root/miner/txCount 序列 SHA-256 均为
`67b6bf6d...f24a`。父链连续、全程零交易，Rust 与 Gov1–Gov5 各出恰好 145 块且
槽位绑定精确。证据 SHA-256 为 `c1d3749f...229f`；含无效临时路径及未保留 raw 的
前两版输出已可恢复地移入 `excluded/`，不计入结论。

提交 `c469bba7...fee91` 增加通用 milestone raw producer waiter（SHA-256
`393d2b36...5b70`）。冻结预检 SHA-256 `40c69083...2d31` 确认六节点、失败流、
目标路径和冻结审计器精确；持久 PID 3492 / session 38246 已等待三小时复合门。门
PASS 后它按里程碑 `endHeight` 向下闭合到完整六槽轮次，调用冻结审计器并原子保存
六端 raw JSONL。启动证据 SHA-256 为 `d3097c9b...642d`，未修改节点或交易状态。

同一冻结工具又为 6/8/12/18 小时门分别启动 PID 4853/4854/4856/4861（sessions
9868/40880/94311/7904）。四份预检均确认目标门尚未出现、节点存活和失败流为空；
每个 waiter 只消费自己的复合门并写入独立 JSON/raw 目录。合并启动证据 SHA-256 为
`e4bc03f4...d3ab`，全程 mutation-free。

提交 `98daf559...2aa0` 又增加 strict24h 专用 raw waiter（SHA-256
`20c7f542...2fc6`）。它只等待 finalizer 在 burst 前原子发布的
`mixed-soak-24h-audit.json`，按其中零交易 `endHeight` 固定历史闭区间；随后交易无法
改变已审计历史。冻结预检 SHA-256 为 `af4a0025...a2ad`，PID 7527 / session 50942
已启动；最终除六端 raw 外还发布 soak/producer 两份 SHA 绑定。启动证据 SHA-256 为
`a4a3de4d...7aa2`。

2.5 小时（9,000 秒）复合门严格 PASS：299 个 head 样本覆盖 9,034 秒，增长 924
块、最大 lag 1、零交易；原 Rust PID 70765 的 31 个资源样本覆盖 9,004 秒，RSS
峰值 263,824 KiB、线程最多 163、FD 93；16 个 Gov5 main 样本覆盖 9,012 秒并
全部精确。Rust 累计 170 次 `5+5`，CommitQC 存在、双签为零。里程碑 SHA-256 为
`dc639f5d...cdf7`，未放宽验收。

同一冻结 raw 工具随后精确消费该里程碑：高度 92,696–93,619 共 924 块、154 个
完整轮次。六端各保留 924 行完整区块身份，序列 SHA-256 均为
`8763d282...6691`；父链连续、全程零交易，Rust 与 Gov1–Gov5 各自产生 154 块且
槽位固定。复合 raw 证据 SHA-256 为 `448b88f7...faa6`。

约 80 分钟处再次完整执行只读 archive/QMDB parity：当前共同高度 93,199 的两组
Gov/Rust proof root 与编码逐字节一致并通过冻结离线验证器；创世到 5,189 的 11 个
历史高度再次通过全部 RPC/root/proof 检查。证据 SHA-256 为 `03f3de7d...3d57`。
高位 905 复制边界 92,605 仍由独立持续验证器覆盖，二者不混淆。

三十分钟复合门 PASS：65 个 head 样本覆盖 1,940 秒、增长 198 块、最大 lag 0；
7 个同 PID 资源样本覆盖 1,801 秒；4 个可达且精确的 Gov5 main 样本覆盖 1,802
秒。Rust 累计 48 次 `5+5`，CommitQC 仍存在，双签与已发送交易均为零；里程碑
SHA-256 为 `8276dfae...d6ab`。

905 数据兼容路径的运行中复核也通过：正式链从高度 92,695 推进到 92,911 后，
五个 Gov datadir 仍均无 `txindex.ranges`，未发生破坏性迁移，六端保持 lag 0。
最终门控的 17 笔交易与 archive RPC 会继续覆盖新增交易的 lookup 路径；本次证据
SHA-256 为 `f5432630...e5a8`。

三十分钟闭合日志深审覆盖高度 92,696–92,930 的 235 个连续规范块，40 个预期
Rust 槽位全部六端精确且日志为 `5+5`，view stride 与 hash 顺序一致。54/54
timeout/pacemaker 均在下一 view 由 Rust `5+5` 恢复，pending 为零；435 条 warning
全量归类，未知与 critical 均为零。冻结 Rust 日志、leader、timeout、runtime-log
SHA-256 分别为 `cc519e7e...1d4f`、`513f8e01...21a2`、
`90432cd5...360f`、`30c7d65e...f180`。

正式生产者分布审计另扫高度 92,696–92,947 的 252 个连续块：Rust 与五个 Gov
出块地址各自产生 42 块，完全均衡，父链连续且无交易。六个在线 leader 槽位精确，
配置中的第七个缺席验证者由上述 timeout 恢复路径处理；证据 SHA-256 为
`80028a25...72c1`。

共同头推进到 92,953 后重复执行 archive/QMDB 只读审计也 PASS：2 份当前
reference proof 在 Gov/Rust 间 root 与 bytes 精确并通过固定离线验证器；11 个历史
高度各自的 19 项 RPC/root/proof 检查全部一致，覆盖创世与 bootstrap 边界。证据
SHA-256 为 `3d1ab47e...4ff9`。

实时客户端身份矩阵确认 28501–28505 均报告 `N42/5.7.906`，29545 报告
`reth/v2.4.1-91725e3/aarch64-apple-darwin`；六端 chainId 均为 `0x477`、均非
syncing，并在固定高度 92,971 返回精确相同的创世与块 hash/state/receipts 身份。
矩阵证据 SHA-256 为 `67eea2d0...5351`。

复制数据启动/追高的直接证据链也已闭合：活动 Rust 进程于 `19:01:54Z` 从持久头
92,605、snapshot-exact view 95,450 和认证 QMDB root 恢复，29.438 秒后就在
92,606 产生规范 `5+5` 块；随后认证执行血统连续推进到六端正式共同头 92,695，
并在 92,696 再次按 Rust 槽位出块。四个检查点哈希均从六端 RPC 重读一致；证据
SHA-256 为 `586d04fe...4676`。

正式出块节奏审计扫描 301 个连续块、覆盖 2,942 个块时间戳秒；时间戳严格递增，
平均块间隔 9.81 秒、最大 40 秒，即使包含缺席验证者 timeout 周期也未超过 61 秒
无停顿门限。证据 SHA-256 为 `418c4e10...1071`。

五十分钟资源趋势覆盖同一 Rust PID 的 11 个样本、3,001 秒和 306 块；线程恒为
161、FD 恒为 93，RSS 首末仅增 11.5 MiB（约 13.8 MiB/小时）。Reth allocated
data 约增 158 KiB/小时，consensus data 不变，QMDB WAL 约增 60 KiB/小时；即使
保守线性投影 24 小时 RSS 也约 551 MiB，低于冻结的 1 GiB 门限。证据 SHA-256
为 `383e11e3...0991`。

保守一小时复合门在未放宽条件下 PASS：120 个 head 样本覆盖 3,607 秒并增长
366 块，最大 lag 0 且全程零交易；13 个同 PID 资源样本覆盖 3,601 秒，RSS 峰值
241,040 KiB、线程 161、FD 93；7 个 Gov5 main 精确样本覆盖 3,605 秒。Rust
累计 76 次 `5+5`，CommitQC 存在、零双签；里程碑 SHA-256 为
`64c648af...9505`。

一小时闭合日志深审扫描规范高度 92,696–93,068：六端 373 块父链连续，63 个
预期 Rust 槽位全部匹配有序 `5+5` 日志。77/77 timeout/pacemaker 均在下一
Rust `5+5` view 恢复，pending 为零；624 条 warning 精确归入允许类别，未知与
critical 均为零。冻结 Rust 日志、leader、timeout、runtime-log SHA-256 分别为
`f16a27a1...e644`、`5763da46...4a8a`、`40ff2727...97ca`、
`e9fb2aca...5410`。

补充的一小时 Gov5 进程基线确认五个原始 PID 均存活且运行超过一小时，共同高度
93,091；RSS 为 140,016–141,920 KiB，线程 18–19，FD 均为 34，没有 Gov
进程替换。证据 SHA-256 为 `df9227ed...1906`。
