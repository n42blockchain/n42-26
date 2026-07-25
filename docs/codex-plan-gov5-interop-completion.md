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

## 红线（任何任务都不得突破）

- 不初始化、不重建、不格式化、不压缩、不清理、不删除任何既有七节点数据库。
- **正式窗口计时期间不替换二进制**——替换即作废该窗口，必须从零重开。
- 只读 observer 在 participant 激活前不得写共识状态。
- 任何 fail-closed 不变量触发 → 立即 `rollback-replacement`，不做"再看一会儿"。
- 密钥材料只经 `@file` 引用，保持 0600，日志与证据快照扫描不得命中密钥模式。

---

## 任务

### T1 — P4 当前窗口判定（真机 / 阻塞全局）

**前置**：无。窗口约在 `2026-07-25T22:06Z` 达到 86,400 秒阈值。

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

## 依赖图

```
T1 (P4 窗口判定)
 └─> T2 (纳入修复 + 重建)
      └─> T3 (P4 gate 关闭)
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
