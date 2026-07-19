# gov5-sync 分支群验收报告（2026-07-19）

> 验收对象：Codex 按 `codex-task-sync-from-gov5-2026H1.md` 产出的 16 个 `*/gov5-sync-*` 分支。
> 验收方法：全部分支合并到本地 worktree `verify/gov5-sync-acceptance`（D:\N42\n42-26-verify），
> 跑 `cargo check --all-targets` + `clippy -D warnings` + `cargo test --workspace`（全绿，1000+ 测试 0 失败），
> 并对 16 个实质提交逐一做验收标准对照 + 对抗性代码审查。

## 总判定

**15/16 项通过；1 项 CRITICAL 必须返工后才能合 main（S5 坏块缓存）；另有 1 项 HIGH 跟进项与若干合并纪律要求。**

## 逐项判定

| 提交 | 任务 | 判定 | 关键发现 |
|---|---|---|---|
| `cecd5c3` n-f quorum | S1 | ✅ 可合并 | 验收点全 PASS（历史 QC 用当时集、n=5→quorum 4、21 节点边界翻转、手机 2/3 阈值未误改）。MEDIUM：混入了无关的 `execution_catchup_floor` 重写（实测驱动的真修复，但应独立提交） |
| `d4b1690` proof-gated view | S2 | ✅ 可合并（**必须与 739c79d 捆绑**） | view-jump 收紧到 qc.view+1、单签名者不能拉 view、TC 去重+聚合验证全 PASS。单独合入有已实测证实的重连活性回归（3 节点停在高度 57）。devlog-96 "wire protocol remains v3" 与实际 v4 矛盾，需改文档 |
| `4f7fd2d` locked-qc parent | S3 | ✅ 可合并 | 唯一合法父块 + PayloadBuildContext 关闭异步竞态 + 兄弟块回归测试，全 PASS。LOW 残留：follower R1 不校验 payload parent 与 justify_qc 绑定（devlog-97 已声明，建议立后续小任务） |
| `739c79d` timeout quorum recover | S2 后续 | ✅ 可合并 | 修 S1+S2 组合暴露的真实活性问题（GossipSub 去重吞重播）；relay 在验签后，无放大攻击面；失败复现→修复→验证闭环完整 |
| `cb9ad52` equivocation guards | S4 | ✅ 可合并 | 双投票守卫持久化（快照 v5 + 16B vote log）、先验签后记证据、乱序检出、fail-closed 启动，全 PASS。3 个 LOW 记档 |
| `929f353` bad block cache | S5 | 🔴 **返工** | **CRITICAL 缓存投毒**：envelope 校验比较的是两个攻击者可控的声明字段；构造 payload 声明哈希=诚实块哈希 H、内容为垃圾 → reth 对 hash-mismatch 返回 Invalid → H 入坏块缓存 → 6 个消费点全部过滤真块，committed 块永远无法导入（正是任务书红线要防的"自堵恢复路径"）。**修法小**：入缓存前本地重算 payload 真实块哈希（seal header）等于声明哈希才允许写入；或对 `latest_valid_hash == null` 的 block-hash-mismatch 类 Invalid 一律不入缓存。其余（LRU 有界、暂态不入缓存、6 消费点、与执行有效性下限不冲突）全 PASS |
| `bb36345` BLS 批量验签 | P1-1 | ✅ 可合并 | measure-first 合规（500 验证者 351ms→137ms=2.56×，超 2× 门槛）；随机加权正确（64-bit 非零 scalar，与以太坊客户端同档）；坏签名回退定位有测试 |
| `32dd092` twig staged flush | P1-2 | ✅ 可合并 | 审计结论"无先改内存后落盘窗口"经复核与代码一致（WAL append+fsync→内存 apply→poison 机制）；重启恢复断言已补。LOW：WAL durable 后 apply 失败分支无注入测试 |
| `9560b48` twig 发散 fail-closed | P1-3 | ✅ 可合并 | 探针绑定精确块哈希、发散锁存不自愈、停发手机包不 panic，质量高。MEDIUM：格式错误与值发散共用计数器，建议分离 |
| `53d66c4` 网络活性包 | P1-4 | ✅ 可合并（附条件） | 四项全落地（trusted 无限重连+源绑定防中继污染、投票重发幂等、分级限流）。MEDIUM：夹带 `collect_future_timeout` 共识协议变更（实现经查安全：逐票验签、严格集合解析、n-f、仅 leader 聚合），需在 devlog 补"共识断代变更"声明 |
| `32cc784` selfdestruct 重放测试 | P1-6 | ✅ 可合并 | 三个真 EVM 用例（同块 recreate 双 fork、未读 slot 无幽灵、Cancun 语义差异）；"手机侧无键值流重放结构性免疫 wipes 问题"论证成立 |
| `fb36371` evidence bitfield | 额外 | ✅ 可合并 | 真 bug（稀疏高 index 参与者被按置位数截断丢失）；仅节点本地 MDBX 格式，手机协议无影响。LOW：需在 devlog 加"不可回退旧版读同一 datadir"警示 |
| `5ba718d` parallel-evm 验证串行化 | 额外 | ✅ 可合并（保留意见） | 真竞态（val_cursor 领取即前移，热账户余额差错可复现 2/30）。MEDIUM：串行化是比标准 Block-STM 更重的锤，无性能前后数据 |
| `e1c6303` parallel-evm selfdestruct wipes | 额外 | ⚠️ 可合并 + **HIGH 跟进** | 真 bug 族（Destroyed 无生产者、MVCC 无 wipe 版本），修复设计正确、14/14 测试过。**HIGH 残留**：同一交易内 destroy→recreate（revm 全局 SelfDestructed flag 语义，`execution.rs:103-133`）会丢弃重建账户，与顺序执行发散——须补差分测试并处理 DestroyedChanged 组合。实验 crate 未接产线，不阻断合并但必须立项 |
| `e34cbf1` mobileverify 对照 | P1-5 | ✅ 通过 | 10 项做/不做结论表 + 带宽核算 + 低能耗铁律为硬约束；附带发现 evidence bitfield 真 bug 并独立修复，纪律好 |
| P2 四文档 + devlog | P2-1~4 | ✅ 通过 | 数据抽查无编造（gov5 归档数字逐字吻合、0x07 算式复核正确）；三个 NO-GO 结论与任务书预期一致且有数据支撑；还主动纠正了任务书两处过期引用 |

## 合并前必办清单（打回 Codex）

1. **CRITICAL**：修 `929f353` 坏块缓存投毒（见上，修法二选一，补"声明哈希=诚实哈希、内容不符"的对抗测试）。
2. **HIGH**：`e1c6303` 同交易 destroy→recreate 边界——补差分测试 + 修 DestroyedChanged 组合（可独立 PR）。
3. **合并纪律**：`d4b1690` 与 `739c79d` 必须同批合入，不得单独 cherry-pick。
4. **DEVLOG.md 索引**：补 devlog-104~108 及 P2 四文档、mobileverify 对照文档条目（当前只索引到 103）。
5. **文档修正**：devlog-96 的 "wire protocol remains version 3 / 可滚动升级" 表述与实际（v4 + 握手拒绝混版本）矛盾；`53d66c4` 的 devlog-104 补"共识断代变更"声明。
6. **规范**：提交 email 统一到模板地址（当前混用 3 个）；`docs/devlog-93-*.md` 在主仓仍是未跟踪文件，需单独提交入库。

## 建议记录的后续项（不阻断）

- S2 残余：视图分散 + 无单视图 quorum + high_qc 陈旧的理论停摆角落（需多数节点丢持久化状态才可达，`53d66c4` 已部分收窄）。
- S3 残余：follower R1 投票不校验 payload parent == justify_qc.block_hash（靠 import 兜底）。
- P1-3：探针计数器分离；采样账户 slot 数上限。
- S4：快照存在但 vote log 被删的反向 fail-closed 窄窗口。
- `test_emit_retries_non_block_committed_output_when_channel_is_full` 并行偶发 flake（blame b291887，与本批无关），单独立 issue。

---

# 第二轮验收补充（2026-07-19 晚）

## A. 全量提交清点（回应"上次验收是否漏审"）

对 origin 全部分支做 `rev-list ^main` 权威清点（40 个提交），结论：**首轮验收没有漏审任何实质内容**。

1. gov5-sync 的 17 个最终提交（堆叠版）首轮全部审过。
2. 独立分支 `fix/gov5-sync-s1..s5` 上 6 个不同哈希的提交，经逐对 range-diff 确认是**同一工作的早期草稿**（时间线：独立版 18:34-21:30 → 堆叠版为最终形态）。差异均为堆叠适配与改进（S3/S5 的 PayloadBuildContext 绑定、bad_blocks 跨提交接线、协议 v4 注释），无独立新内容。
   **⚠️ 合并纪律（新增）**：合并时只用 `validate/gov5-sync-p0` + 各 tip 分支；**绝不合并独立的 S1-S5 分支**（过时草稿）。建议合并后删除这 5 个分支防误用。
3. 其余 10 个陌生提交全部是 6-7 月历史遗留分支（`docs/mtps-reproduction-guide`、`proto/blake3-binary-roots`、`feat/windows-native-7node`、`chore/reth-upstream-bump`、`chore/deps-toolchain-20260714`、误推的 stash `0ed0ed8`），与本次工作无关，建议清理。

## B. 新分支 `chore/deps-toolchain-20260719` 验收：✅ 通过

| 项 | 结果 |
|---|---|
| `74d4516` deps 升级 | reth 系 101 crate 2.3.0→2.4.1、Alloy 2.1.1→2.2.0、Rust 1.97.0→1.97.1、tokio/serde/borsh 补丁级上调——**全部升级，无红线违规**；提交不含源码改动 |
| reth 新基线 | `n42/chore/reth-upstream-20260719` @ `c533db8ba`，CLAUDE.md 宣称的 `8e84768246` 是其祖先（已验证）；reth Cargo.toml version=2.4.1 与文档一致 |
| CI 对齐 | e2e/nightly/execution-spec-shards 四处 ref 一致更新到 20260719 |
| `e69e84f` warmup floor | 真修复：peer-connect 事件可让 quorum 门在 warmup 到期前提前放行，现单独跟踪 `leader_build_not_before`；带单测 + scenario4 分级 rendezvous（7-10 节点 15s / >10 节点 35s）+ 新增 `E2E_SCENARIO4_PROFILE=seven` 回归通道 |
| 沙箱验证 | 独立 worktree 对（reth@c533db8ba + n42-26@e69e84f）：check 3m17s 零警告、clippy -D warnings 零警告、44 个测试套件全 ok、Cargo.lock 未被构建改写（与新基线精确一致） |

**合并前置**：本地与 CI 的 `../reth` 必须先切到 `chore/reth-upstream-20260719`（分支已在 n42blockchain/reth 远端）；旧基线上无法编译此分支。

## C. 返工状态跟踪

首轮"必办清单"6 项（含 S5 CRITICAL 缓存投毒）**截至本轮仍未交付**——远端 gov5-sync 分支自 07-19 00:32 后无任何新提交。返工到位后在 `D:\N42\n42-26-verify` worktree 复验。

---

# 第三轮验收（2026-07-19 深夜）：返工分支 `fix/gov5-sync-p1-rework-20260719`

## 根因先说：Codex 一直看不到验收报告

三轮下来 S5 CRITICAL 始终没修，根因是**本报告一直是主仓库未提交的本地文件**（`?? docs/codex-acceptance-*.md`）。
Codex 响应的是任务书 `codex-task-sync-from-gov5-2026H1.md`（已入库），而非验收清单。返工分支
`fix/gov5-sync-p1-rework-20260719` 是对**任务书 P1-1~P1-6** 的重做，不是对首轮"必办清单"的返工——
它改进了很多东西，但对首轮点名的 S5 投毒漏洞只字未提。**要让返工闭环，必须先把本报告提交推送，Codex 才看得到。**

## 分支结构

`validate/gov5-sync-p0`（S1-S5，含未修的 `929f353`）→ 重做 9 个 P1 提交 → 新增 6 个提交。共 22 提交。
署名规范全部合规（无 AI 署名、作者统一模板 email）。构建：check/clippy 零警告，44 测试套件全绿
（唯一失败的 `test_emit_retries_non_block_committed_output_when_channel_is_full` 已确认是 main 上就有的存量
并行 flake，本分支未改它、单跑与复跑并行均绿，非回归）。

## 已修复的首轮遗留项 ✅

| 首轮问题 | 返工处置 | 提交 |
|---|---|---|
| DEVLOG 索引缺 104-108 | 已补全，devlog-95~109 全部入索引 | 分支内多提交 |
| 共识断代变更未声明 | devlog-109"兼容性"节已声明（consensus + state-sync wire 断代、禁混跑） | `17d96a5` |
| BLS 批量 CommitVote 域绑定 LOW | 实质修复：token 改枚举绑定准确 validator-change domain，stale 降级逐条；顺带修 batch 首元素随机系数 + fallback 坏索引覆盖 tail 的真实口子 | `b8e055b` |
| Twig 探针"边界空 diff 漏采早期坏写" | 实质改进：改跨 interval 窗口确定性蓄水池 + apply_lock 串行化 | `27e7565` |
| e1c6303 同交易 destroy→recreate HIGH | 经复核 revm 41 源码判定**不可达**（该形态触发 CreateCollision，metamorphic 必须跨两笔交易）→ 降级为文档表述夸大（LOW）。跨交易 recreate 的 wipe 传播缺陷已实质修复 | `6074c71` |

## 未修复 / 新发现的问题 ❌

### 🔴 CRITICAL（阻断合并）— S5 坏块缓存投毒，返工后升级为缓存污染 wedge

**首轮的投毒漏洞原样保留**（`929f353` 未动，已亲自读 `bad_block_cache.rs:88-99` 确认：`insert_if_invalid`
对任何 `Invalid` 写缓存，不区分 hash-mismatch 型），且新提交 `95bba66` 让它**更严重**：

- `import_and_notify`（`execution_bridge.rs:773-778`）与 eager 路径（`:660-665`）都在 `new_payload` **之前**
  把 peer 提供的 `execution_output` 注入 reth payload cache（key=声明 block_hash），返回非 Valid 时
  `insert_if_invalid`（`:661/:801`）无条件写坏块缓存，**注入的伪造 bundle 不回收**（已读代码确认三条失败分支均无 take）。
- 攻击链：恶意 peer 构造 lineage，声明字段全部链到接收方 exact head（CL 侧只比声明字段，全过），
  声明 hash 用真实块 H、内容伪造 → 伪造 output 以 key=H 注入 → `new_payload` 重算 hash 不匹配返回 Invalid
  → 真实 H 被写坏块缓存 + 伪造 bundle 滞留 cache → 真实 H 到达时 cache-hit 取走伪造 bundle → state root
  不匹配 → 真实 H 判 Invalid → 永久 wedge，仅重启可救。**单个 Byzantine peer 一次 sync 响应即可卡死恢复中节点**，破 f 容错活性。
- 95bba66 之前 sync 路径的 broadcast `execution_output: None` 不注入；本提交首次让 sync peer 的 raw output 进入注入路径，故是它把"声明 hash 毒化"升级成"缓存污染"。
- **修复（小改，返工时给 Codex）**：① `insert_if_invalid` 过滤 block-hash-mismatch 类 validation_error（不入缓存）；② import 非 Valid 时 `take` 回收本次注入的 cache 条目。

### 🟠 MEDIUM

- `95bba66` sidecar StateDiff 在内容验证前就 staged（`state_mgmt.rs:617`），flush 时不校验 (hash, diff) 与该 view canonical 导入 hash 一致。缓解：P1-3 探针会锁存 unhealthy（fail-closed，不静默污染手机 proof）。建议 flush 绑定 canonical hash 校验。
- `docs/devlog-96-view-proof-gates.md:99` "wire protocol remains version 3" 仍错（实际 v4，`messages.rs:251`）——返工未触及该文件。
- `collect_future_timeout` 共识行为变更仍夹带在 `1e8ccb0` fix(network) 里，未拆分（devlog-109 笼统声明兜底，未逐条点名）。

### 🟡 LOW

- `95bba66` lineage 响应最多 128 条 raw payload（含 execution_output，可数 MB），可能超 16MB 帧上限致响应失败/反复超时。建议按帧预算截断。
- Twig 探针候选 storage_slots 无上限，窗口累积后热点合约敞口比旧版更大（首轮建议的 slot 上限未加）。
- `3b0d4f1` 未加 bitfield v1→v2"不可回退旧版读同一 datadir"降级警示；`devlog-104` 引用悬空旧哈希 `5ba718d`（实为 `71bf98e`）；`mobileverify-gov5-comparison.md` 未入 DEVLOG 索引。

## 95bba66（prepared-lineage sync/2）核心设计评价

**链验证骨架健全**：信任链 = CommitQC(子块) → 子块 hash → 子块 payload 的 parent_hash → ancestor hash，
内容真实性最终由 reth `new_payload` 重算 state root 裁决（`n42_skip_state_root` bypass 已被 P1-3 生产硬拒）。
不存在"绕过 QC 导入任意块"的口子；raw lineage 不完整/不连 exact head 时 fail-closed，有测试。
Scenario 9 修复方向正确。**唯一硬伤就是上面的 CRITICAL**——"CL 预检只比声明字段 + reth 拒绝后的副作用
（缓存注入、坏块记录）不回滚"，一个小补丁即可收口。

## 第三轮判定

**返工质量明显高于首轮**（fail-closed 意识贯穿、测试针对性强、6528f60 的 epoch 恢复对齐是扎实的真修复），
但**仍不可合并**：S5 CRITICAL 未修且被 95bba66 放大。合并前必办：

1. **CRITICAL**：修 S5 投毒 + 95bba66 缓存注入回滚（`insert_if_invalid` 过滤 hash-mismatch + import 失败 take 注入）。
2. **首要动作**：提交推送本验收报告，让 Codex 能看到"必办清单"——否则第四轮大概率仍不修 S5。
3. MEDIUM：sidecar staging 绑定 canonical hash；devlog-96 版本号改 4；collect_future_timeout 拆分或 devlog 逐条声明。
4. 合并纪律不变：用 `fix/gov5-sync-p1-rework-20260719`（已含 P0 全栈），**勿再合独立 S1-S5 草稿分支**。

---

# 第四轮验收（2026-07-19，返工后）：✅ 全部通过，可合并

Codex 读到留言后用 2 个提交（`b2af731` fix + `f5aa472` docs，在 `fix/gov5-sync-p1-rework-20260719` 上）
把 CRITICAL 连同全部 MEDIUM/LOW 一次收清。**逐项亲自核实（不外包核心结论）如下：**

## 🔴→✅ CRITICAL：S5 缓存投毒 —— 两处修复均正确且完整

**Fix ①（坏块缓存不接受 hash-mismatch）**：`bad_block_cache.rs` 的 `insert_if_invalid` 新增
`is_declared_block_hash_mismatch()`，对 `"block hash mismatch:"` 前缀（trim+小写）的 Invalid 不写缓存。
**承重的字符串判据经端到端核实无误**：
- alloy-rpc-types-engine **2.2.0**（在用版本）`PayloadError::BlockHash` 的 Display =
  `"block hash mismatch: want {execution}, got {consensus}"`（`error.rs:60`）；
- reth `on_new_payload_error`（`engine/tree/src/tree/mod.rs:3165`）`PayloadStatusEnum::from(error)`
  把该 Display 透传为 `Invalid.validation_error`，且对 block-hash-mismatch 置 `latest_valid_hash = None`；
- Codex 前缀精确命中。红线保持：state-root-mismatch 等确定性失败仍正常入缓存（有 `deterministic_execution_rejection_remains_cacheable` 测试）。

**Fix ②（注入的 execution_output 失败回收）**：`ExecutionOutputCache` trait 新增 `evict(hash)`，
node 侧 `RethExecutionOutputCache` 实现为 `take_payload_execution` 移除 reth cache 条目。
**16 个 evict 调用点覆盖全部 inject-then-非-Valid 路径**：import_and_notify 的 Invalid/Syncing/Err、
eager 导入、syncing retry、leader eager、committed 后台导入、observer——逐点核对无遗漏。

新增投毒测试 `declared_hash_mismatch_never_blacklists_the_declared_hash` 断言"声明 hash=H 内容不符 →
H 不进坏块缓存、len==0"，实跑通过；既有 `syncing_payload_is_never_added` / `direct_block_arrival_is_filtered` 全绿。

## 🟠→✅ MEDIUM

- **sidecar StateDiff 绑定 canonical hash**：新增 `execution_validated_sidecar_hashes: view→hash`，
  同 view 出现不同 execution-valid hash 时**拒绝绑定**（`conflicting execution-valid hashes; refusing binding` + metric），flush 按此绑定冲刷。
- **devlog-96 版本表述**：改为 "consensus wire protocol is version 4 / handshake rejects mixed v3/v4"。
- **collect_future_timeout 共识断代声明**：devlog-104 已逐条点名声明为共识行为+兼容性断代（禁新旧混跑），并修正悬空哈希引用 `5ba718d`→`71bf98e`。

## 🟡→✅ LOW

- lineage 响应按 `MAX_SYNC_MESSAGE_SIZE`（16MB）帧预算截断（`sync_response_fits_frame` + metric）；
- Twig 探针 `MAX_TWIG_PROBE_SLOTS_PER_ACCOUNT = 256` 上限 + dropped metric；
- bitfield v2 "datadir 单向迁移、不可原地降级旧版"警示补入 devlog-106；
- devlog-110 收尾文档准确复述攻击链与修复。

## 构建与测试

check/clippy 零警告；44 测试套件全绿；唯一失败仍是 `test_emit_retries_non_block_committed_output_when_channel_is_full`——已三轮确认是 main 上就有的**存量并行 flake**（单跑绿、串行复跑绿），非本批引入，建议单独立 issue，不阻断。

## 最终判定：**整批可合并**

`fix/gov5-sync-p1-rework-20260719`（含 P0 全栈 + P1 重做 + 6 新提交 + S5 硬化）全部验收项通过。
合并前置与纪律：
1. 本地/CI 的 `../reth` 切到 `chore/reth-upstream-20260719`（配套 `chore/deps-toolchain-20260719` 已单独验收通过）；
2. 用 `fix/gov5-sync-p1-rework-20260719`，**勿合并独立 S1-S5 草稿分支**（过时）；合并后建议删除草稿分支与历史遗留分支。
3. 合并后把存量 flake `test_emit_retries_...` 单独立 issue。

## 验收环境记录

- worktree：`D:\N42\n42-26-verify`（分支 `verify/gov5-sync-acceptance`，八爪合并 + DEVLOG 并集），返工后可在此复验。
- 构建：check 3m11s 无警告；clippy -D warnings 无警告；`cargo test --workspace` EXIT=0。
- 审查中本机复跑：n42-consensus lib 200、integration_test 67、chaos_7node 12、n42-consensus-service 139、n42-jmt、n42-parallel-evm 14/14 等全绿。E2E 场景 4/5/9/10 未复跑，采信 devlog-95/100/103/104 实跑记录。
