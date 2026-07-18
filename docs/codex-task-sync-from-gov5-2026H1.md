# Codex 任务书：N42-gov5 近 6 个月更新同步评估与移植（2026-01-18 ~ 2026-07-18）

> 这份文档是给 **OpenAI Codex** 的任务书，自包含可直接开工。
> 源仓库：`C:\n42\N42-gov5`（Go，go-ethereum 系，HotStuff-2 + QMDB，7 节点主网实弹）
> 目标仓库：`D:\N42\n42-26`（Rust，reth 系执行层，HotStuff-2 + Twig sidecar 状态树）
>
> **提交规范**：所有 git 提交**不要包含 "Claude" / "Codex" / "Co-Authored-By" 等 AI 署名**。
> 用项目模板作者：
> ```
> GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
>   git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" -m "..."
> ```
> 每完成一个任务组，按仓库规范增量追加 devlog（索引见 `DEVLOG.md`）。

---

## 0. 背景与阅读指南

### 0.1 两仓关系

gov5 与 n42-26 是同一设计目标（HotStuff-2 + EVM 执行 + 手机旁路验证）的两个独立实现。
gov5 是 Go 实现，走过了 **7 节点真机主网实弹**（2026-06~07），把共识↔执行↔状态耦合层的
一整族 bug 逐一爆出并修掉；n42-26 是 Rust 实现，架构上更依赖 reth 的引擎树做单一权威。
**"同步"的含义是移植设计结论、安全防线和实测数据，绝不是翻译 Go 代码。**

两仓已有的交叉借鉴（本任务书**不重复**这些内容）：

| 已同步项 | 方向 | 出处 |
|---|---|---|
| fast propose + optimistic voting | Rust → Go（gov5 `36947b0b`，03-21） | gov5 侧移植 |
| 动态验证者 commit-then-activate | 双向对齐（gov5 `89b3bf3a` 明确对齐 n42-26） | devlog-54 / gov5 03-26 |
| committed 块执行有效性下限（CRITICAL） | Go 实弹教训 → Rust PR #21/#22 已合并 | `docs/codex-task-execution-validity-floor.md`、`docs/pr21-reaudit-2026-07-12.md`（残留任务 T1–T5 已立项，**勿重复开工**） |
| Twig sidecar vs QMDB 缺陷族审计 | Go 教训 → Rust F1a/F1b/F3/F5/F6/F7/F8 已立项 | `docs/twig-core-audit-2026-07-12.md`（含 Codex 任务，**勿重复开工**） |
| BMT/QMDB 性能对标、字节级验证 | 双向 | devlog-62 / devlog-64 |
| Rotor 单跳传播 | Go → Rust 已有 | devlog-55 |
| witness 有序流（gov5 `f2604416`）≡ read_log | 同源设计，Rust 侧更早 | devlog-09、memory：read_log beats BAL |
| eth 客户端调研（reth/erigon/geth 三月更新） | Rust 侧独立完成阶段一 | devlog-93 |

### 0.2 必读的 gov5 文档（开工前先读，都在 `C:\n42\N42-gov5\docs\`）

1. `maturity-gap-consensus-2026-07.md` — 实弹战役总结 + 8 条底线防线 + P0/P1/P2 差距（**最重要**）
2. `n42-26-h2-consensus-audit.md` — gov5 对 n42-26 的 8 项交叉审计（07-11）
3. `codex-task-maturity-p0.md` — gov5 自己的 Codex 任务书（Task A–D，其中 A/B 与本书 S5/已完成的执行门同族）
4. `hotstuff-view-convergence-followup.md` — canonical 多写者根因与修复实录
5. `docs/ethel/archive-engineering-summary.md` — 归档分层战役总结（P2 调研任务用）

### 0.3 gov5 六个月主线速览（按时段）

- **01~02 月**：安全审计波（panic→error、MDBX cursor 语义 bug）、erigon-lib 内化、大规模重构。
- **03 月**（377 提交）：功能爆发——HotStuff-2 引擎引入并 7 节点跑通、JMT+Blake3、Block-STM、
  snap sync/freezer/分层存储、SP1 zkVM、ZK 跨链桥、EEST 合规冲刺、EVAL 系列技术评估（砍掉大部分追热点项）。
- **04~05 月**（716 提交）：转向 eth-el（以太坊执行层兼容栈）——freezer 列式压缩、25M 块主网回放、
  归档三层存储（945GB→169GB）、MPT proof、无状态验证 + 手机 minimal node、devp2p 直连同步。
  HotStuff-2 本体这两个月**零改动**。
- **06~07 月**（554 提交）：**HotStuff-2 + QMDB 实弹上线战役**（本任务书 P0 的主要来源）——
  7 节点真机把耦合层 bug 全部爆出修掉；07-14 起协议级安全硬化波（quorum n-f、TC 门控 view 推进等）；
  QMDB 全历史层；mobileverify 统一手机见证栈；对 n42-26 的两份交叉审计。

---

## 1. gov5 更新全景清单与适用性评定

逐主题列出（提交哈希均为 gov5 仓库短哈希）。评定含义：
**[同步]** = 本书立任务；**[已覆盖]** = n42-26 已有等价物或已立项；**[调研]** = 只评估不动码；**[不同步]** = 明确不做（理由见 §4）。

### 共识（HotStuff-2）

| # | 更新 | gov5 参考 | 评定 |
|---|---|---|---|
| C1 | quorum 从 2f+1 改 n-f（n≠3f+1 时 2f+1 两 quorum 交集可全为拜占庭节点 → 可双提交；动态验证者集下 n 任意，是潜伏安全洞） | 07-14 硬化波（`b83e281f` 附近） | **[同步→S1]** |
| C2 | view 推进只认 quorum 证明的 TC；QC view-jump 上界收紧到 qc.view+1（不信攻击者报的 msgView；否则单拜占庭节点循环签 timeout(v+k) 可拖全网跳 view） | 07-14~18（`deae48ca`、`4f8925ee`） | **[同步→S2]** |
| C3 | leader 提案必须扩展 LockedQC 块而非本地 head（实弹：每节点 12-15 次分支切换震荡、quorum 无法形成） | 07-09（`59c5e6b4` 附近） | **[同步→S3]** |
| C4 | equivocation 检测与到达顺序无关 + 先验签再记证据；votedInView/commitVotedInView 双投票守卫 | 07-14~18、04-01（`9d942f6e`） | **[同步→S4]** |
| C5 | 坏块/执行无效分支缓存（catch-up/fetch-on-miss/direct-push 三路径不查缓存 → 每 8s 死循环拉同一坏分叉） | `25a2aa99`、codex-task Task A | **[同步→S5]** |
| C6 | 重启模型：启动回卷未 commit 的投机块、只信 committed；重启后 view 发散恢复 | 07-10 | **[已覆盖]**（pr21-reaudit T1–T3 残留任务已立项；S2 顺带核对 view 恢复） |
| C7 | commit-only canonical 单一权威（三个并发 canonical 写者互相覆盖是 Go 侧最大根因） | `09e6e1be`、view-convergence 文档 | **[已覆盖]**（gov5 audit 确认 Rust 靠 reth 引擎树结构性免疫） |
| C8 | 投票证据分级：只认 applied lineage | `ef5781d5` | **[已覆盖]**（= 执行有效性下限工单，PR #21/#22） |
| C9 | 逐块 state root 校验（"稳定数周"是不验根的假稳定）+ 失败原子揭除 | `75351eaa` | **[同步→P1-3]**（Rust 侧 reth 默认验根，但 `N42_SKIP_STATE_ROOT` 仅 warn + twig sidecar root 无逐块对账） |
| C10 | order-then-execute 两相投票门实验：**实测更差**（17.6-18.8s/块 vs 3.75s，view 超时 3-6 倍），默认 OFF | `1842c367` | **[不同步]**（反向证据：支持 n42-26 保留乐观 R1 + commit 门路线） |
| C11 | 批量 BLS 投票验证 14.5×（blst 批量接口） | `579845da` | **[同步→P1-1]** |
| C12 | 跨 view 同高候选按最低 hash 收敛、embedded-CommitQC catch-up、baseTimeout 60s→6s | 07-10 | **[调研]**（S2 附带核对 n42-26 是否有同类死锁面） |

### 状态树 / 存储

| # | 更新 | gov5 参考 | 评定 |
|---|---|---|---|
| S-1 | QMDB staged flush：FlushTo/Evict/TakeUndo 原本在块事务内就变更内存态，事务回滚后内存已污染；改为暂存、事务 durable 后才 adopt | 07-18（`31cffdd4`/`0e5681a8`） | **[同步→P1-2]** |
| S-2 | ApplyUndo 活树单块回卷（同高兄弟等价性 base+A+revert+B == base+B） | 07-08 | **[已覆盖]**（twig-core-audit Item 2：Rust 侧无回卷路径、HotStuff commit 即 final，结构性不适用） |
| S-3 | QMDB 全历史层：冻结叶树 + activeBits 分离承诺、每 entry 4 字节 death stamp、ProofAtHeight 产实时同构 proof | 07-03（root 公式变更） | **[调研→P2-1]** |
| S-4 | JMT 在主网密度下比 reth MPT 大 80×（386M 账户实测；价值只在历史 proof 去重与 ZK 哈希，不在存储压缩） | `3ca647e2`、`dd297012` | **[已覆盖]**（n42-26 已弃 JMT 默认、走 Twig 全 DRAM，devlog-63；结论一致，无行动） |
| S-5 | 归档三层存储：Snapshot/History/Warm-CS 三层不变式、warm CS 7 天窗 397GB→820MB（-99.8%）、MPHF+4B 指纹替代 52B key（-47%）、BLAKE3 manifest 信任锚；25M 块实测 945GB→169GB | 05-17~19（`c61a5726`、`8e1c1d87`）、archive-engineering-summary | **[调研→P2-2]** |
| S-6 | freezer 列式压缩（cidx/cdat、compact receipt -81%、Elias-Fano 496MB→350KB）、EIP-4444 冷段 BitTorrent 卸载、四层客户端发行（mobile/minimal/full/archive） | 04 月、06-02~13 | **[不同步]**（eth-el 定位；n42-26 用 reth static files） |
| S-7 | DATC 全历史 EIP-1186 proof（170-420GB vs Erigon 4.1TB；checkpoint 物化后早期块 proof 14m59s→2.0s） | 06-10~07-10，**62 提交未合并 main** | **[不同步]**（见 §4） |
| S-8 | MDBX 经验：WriteMap 是 Windows OOM 根因、DupSort cursor 语义 bug 族、MapSize/DirtySpace 调优 | 04-22（`b834fdc0`）、02-22（`4c0294dc`） | **[不同步]**（reth 自管 MDBX；n42-26 Windows 原生已跑通。仅备注：若未来出现 Windows 内存异常，先查 WriteMap） |
| S-9 | SELFDESTRUCT pre-wipe slots：witness 重放端无法枚举被毁合约 slots → 幽灵行毁 root；wipes sidecar addr-only 格式（比 per-slot 小 ~3000×） | `d7e70658`、`9d06596f` | **[同步→P1-6]**（对抗测试） |

### 网络 / 同步

| # | 更新 | gov5 参考 | 评定 |
|---|---|---|---|
| N1 | validator-mesh：信任 peer 限流豁免（控制消息限流误伤 trusted peer 是实弹活性杀手）、静态 peer 重拨、投票重发 | 07-08 Layer 6 | **[同步→P1-4]** |
| N2 | fetch-on-miss 限流、BestPeers 取最高广告高度、孤儿区间自旋改中止、多块败方分叉回溯 | 07-18 | **[同步→P1-4]**（并入同一任务） |
| N3 | Rotor leader-direct/relay 真正接线（原 peer 注册表无生产调用方，全部退化为全量 gossip） | 07-14 | **[已覆盖]**（devlog-55 已有 Rotor；任务 S5 顺带确认 n42-26 的 Rotor 路径有真实调用方，不是同样的死代码） |
| N4 | devp2p 直连 EL 同步、Caplin CL 移植（B/B+ 两档）、checkpoint sync | 05 月、06-06 | **[不同步]**（n42-26 已独立完成 Caplin 式 ports-and-adapters，devlog-88/89） |

### 手机验证 / witness

| # | 更新 | gov5 参考 | 评定 |
|---|---|---|---|
| M1 | mobileverify 统一栈：BLS 注册表（PoP 强制、稳定 MobileIndex）、72B 规范签名消息、稀疏签名者掩码（delta-varint 唯一编码）、滚动缓存 | 07-16~18（`206cb01f` 等） | **[调研→P1-5]** |
| M2 | 跨节点 cohort 合并：512 IDC 下每块最多 512 张碎片证书 → 每真实结果一张（CohortCoordinator、重复提交检测、合并鉴权） | 07-17 | **[调研→P1-5]** |
| M3 | header 承诺锚：EIP-4788 模式 Header.MobileRegistryRoot 可选字段 + 8191 槽环形缓冲系统合约，nil 时头哈希字节不变 | 07-17 | **[调研→P1-5]** |
| M4 | 三层信任手机 minimal node（header 链锚 / witness 重放 / 按需 EIP-1186）+ K-块锚节奏（~2MB/100 块 vs 每块独立 94.6MB） | 05-30~31（`7ab70a62`、`5f36a4d4`） | **[调研→P2-1 附带]** |
| M5 | witness 有序流格式（append 执行序、不存 key、重放端重执行按序弹值；v2 sorted-map 试过即 revert） | `f2604416` | **[已覆盖]**（= read_log，Rust 侧更早且更完整） |

### 执行 / EVM / 其他

| # | 更新 | gov5 参考 | 评定 |
|---|---|---|---|
| E1 | Block-STM 终审：25M tx-dense 块 8 worker 端到端仅 1.15×，低于 1.5× 门槛 → 不上主路径，保留 shadow-bench | `b48f55fe` | **[不同步]**（n42-26 parallel-evm 有自己的实测；结论备案，印证 measure-first） |
| E2 | evmone cgo 集成净亏损（回调开销）；S3-FIFO 4GB = LRU 24GB 同命中率（85% 是首触强制 miss 天花板）；secp256k1 cgo 互斥把 32 worker 串行成 1 | 04-27~05-08 | **[不同步]**（Go 特有；S3-FIFO 数据点备案） |
| E3 | 三客户端审计移植波：对 geth/reth/erigon 各做 3 个月修复借鉴（EIP-7702 txpool 授权约束、eth_simulateV1 限界、WS 升级限流、Block-STM SELFDESTRUCT shadow 等） | 06-04~05 | **[同步→P2-4]**（机制化，devlog-93 的续篇） |
| E4 | BAL（EIP-7928）：规范构建 + fork 门禁默认 off + out-of-band devp2p 分发，用途是执行预热 | 07-14~15 | **[已覆盖/不同步]**（n42-26 已判 no-go：手机侧倒退、follower 侧 reth 自带） |
| E5 | RPC 修复波（feeHistory 用 receipt 求和、filter 范围限界等）、VM PopPtr 微优化、prune 行预算 | 07-14~18 | **[不同步]**（n42-26 RPC 走 reth，多数由上游覆盖） |
| E6 | PQ 交易（0x05 + Falcon/Dilithium gas 计划）、0x06/0x07 批量交易（0x07 = N 发送方一个 96B BLS 聚合签名） | 06-18~19，**未合并** | 0x07 **[调研→P2-3]**；其余 **[不同步]** |
| E7 | AI/messaging/分布式计算存储/MEV/4337/跨链桥/LtHash/Verkle/Tile 等平台化探索 | 03 月为主 | **[不同步]**（不在 n42-26 定位内） |
| E8 | EVAL 系列否决清单：XDP、SALT、IBC 移植、自建 zk prover、训练 DePIN、bulk-commitment、TopCache、zstd 字典、MPHF trie 压缩 | 03-23~26、05-20 | **[不同步]**（避坑清单，直接采信） |

---

## 2. P0 任务（共识安全，先做，逐个 PR）

> 通用红线：**不得为通过编译降级 `Cargo.toml` 的 revm/alloy/reth-* 版本**（见根 CLAUDE.md）。
> 通用验收：`cargo check --all-targets` + `cargo clippy --all-targets -- -D warnings` +
> `cargo test --workspace` 全绿；每个任务补对应回归测试。

### S1 — quorum 阈值从 2f+1 改为 n-f（CRITICAL）

**背景**：HotStuff 安全性要求两个 quorum 的交集至少含一个诚实节点。n=3f+1 时 2f+1 与 n-f 等价；
但 n42-26 支持动态验证者集（commit-then-activate，devlog-54），n 可以是任意值。当 n > 3f+1
（例如 n=10、f=1 时 2f+1=3），两个 3 人 quorum 的交集可以为空或全为拜占庭节点 → 同一 view
可产生两个冲突 QC → 双提交。gov5 于 07-14 硬化波中修复（同时修了重配置场景）。

**现状**：`crates/n42-chainspec/src/lib.rs:165` `quorum_size()` 返回 `2*fault_tolerance+1`。

**工作项**：
1. `quorum_size()` 改为 `n - f`，其中 `n` = 当前活跃验证者数、`f = (n-1)/3`。注意：该函数目前
   可能只从 `fault_tolerance` 配置推导而不知道 n —— 需要把验证者集大小传进来（或在
   ValidatorSet 上提供权威的 `quorum_size()`，chainspec 只留静态默认）。全仓 grep
   `quorum_size|2 * f + 1|2*f+1`，把 QC 聚合、TC 聚合、投票计数、mobile 收据聚合阈值中
   **属于共识安全**的路径全部统一到新函数（手机收据 2/3+1 属于旁路统计，不在共识关键路径，可不动，但要在代码注释里写明区别）。
2. 动态验证者 epoch 切换处核对：新旧集重叠校验（devlog-54 的 2f+1 重叠不变量）同步改为 n-f 口径。
3. 快照/持久化兼容：若 quorum 大小影响已持久化的 QC 验证，需确认历史 QC 用**当时的**验证者集验证（gov5 保留历史集用于跨界 QC 验证的做法）。

**验收**：
- 单测：n=4/5/7/10/21 各验证 quorum 值（n=5,f=1 时旧值 3、新值 4；n=10,f=3 时旧值 7、新值 7 等价场景也要覆盖）。
- 恶意场景测试：n=5 时 3 票不能形成 QC。
- 7 节点 E2E（场景 4）不回归。

### S2 — view 推进的 TC/QC 门控收紧

**背景**：gov5 实弹发现两类拖网攻击面：(a) view 推进若接受非 quorum 证明的信号，单个拜占庭节点
循环签 timeout(v+k) 可拖着全网跳 view 制造 TC；(b) QC-based view jump 若采信消息自带的 msgView，
攻击者可用一个旧 QC + 假 msgView 把节点拽到任意远的 view。gov5 修复：view 推进只认 quorum TC；
QC jump 上界收紧为 qc.view+1。

**现状**：n42-26 `crates/n42-consensus/src/protocol/state_machine.rs` 的 `try_qc_view_jump`
（约 :807-880）跳到 `max(qc.view+1, msg_view)` 并"cap the gap between msg_view and qc.view"——
有缓解但语义与 gov5 收紧后不同；timeout.rs 有 TC 构造与 genesis-QC 防回退守卫。

**工作项**：
1. 审计 `try_qc_view_jump`：msg_view 超出 `qc.view + 1` 的部分是否有任何未经 quorum 证明的采信。
   建议对齐 gov5：jump 目标 = `qc.view + 1`，msg_view 只用于决定是否缓冲消息，不决定跳到哪。
2. 审计 `timeout.rs`：TimeoutCert 形成是否要求 quorum 张不同验证者的 timeout 签名（而非单节点重复签）；
   收到 TC 推进 view 时是否验证 TC 的聚合签名与成员去重。
3. 审计 pacemaker/round：过期定时器触发是否被忽略（gov5 07-18 修过 stale timer）。
4. 顺带核对 C12：同高多候选、重启后 view 发散场景有无死锁面（gov5 用最低 hash 收敛 + embedded-CommitQC catch-up 解决）。

**验收**：恶意消息单测——(a) 伪造 msg_view=qc.view+1000 的消息不得使 view 跳过 qc.view+1；
(b) 单节点重复 timeout 签名不得形成 TC；(c) 现有 `test_view_jump_*` 系列全绿。

### S3 — leader 提案父块必须扩展 LockedQC 分支

**背景**：gov5 实弹（07-09）：leader 在"本地 head"上出块而非 LockedQC 指向的分支，分支切换
震荡期间每节点 12-15 次换分支、quorum 永远无法形成，全网卡死。修复后 leader 提案强制 extend
LockedQC 块。这是 HotStuff-2 安全规则的直接要求，但在"共识 head"与"执行层 head"分离的架构里
（n42-26 正是：head_block_hash 在 orchestrator、locked_qc 在协议层）容易被工程实现破坏。

**工作项**：
1. 审计 n42-26 leader 出块路径（`n42-consensus-service` orchestrator 的 payload build 触发 +
   `n42-consensus` proposal 构造）：提案的 parent 是取 `locked_qc().block_hash` 还是取
   orchestrator 的 `head_block_hash`/reth canonical head。三者不一致时（视图切换中、执行落后时）以谁为准。
2. 若存在"以执行层 head 为父"的路径：加防线——提案父块必须等于 justify_qc（即 leader 所知最高 QC）
   指向的块；执行层落后时宁可 defer 出块（devlog-85 已有 defer 机制，确认两者协同）。
3. 补回归测试：构造 locked_qc 指向 A、本地 head 为同高兄弟 B 的状态，断言 leader 提案父块为 A。

**验收**：新增单测 + 现有 consensus 集成测试（7 模块）全绿 + E2E 场景 4。

### S4 — equivocation 与双投票防线核对

**背景**：gov5 两轮修复：(a) equivocation（等价投票/双提案）检测必须与消息到达顺序无关；
(b) 必须先验签再记录证据（否则攻击者可伪造他人签名的"证据"污染诚实节点记录）；
(c) votedInView / commitVotedInView 双投票守卫（同一 view 不得投两次 prepare/commit）。

**现状**：n42-26 有共识证据持久化（devlog-56）与 slashing 证据结构，但上述三点未逐一核对过。

**工作项**：
1. 审计 `n42-consensus` 投票路径：同一 view 重复投票守卫是否存在且在持久化状态里（崩溃重启后不失忆——与 pr21-reaudit T1 的快照 v4 工作衔接，字段可一起进快照）。
2. 审计证据记录路径：记录 equivocation 证据前是否完成 BLS 验签；乱序到达（先收后发的提案后到）是否仍能检出。
3. 补测试：同 view 两个不同 proposal 先后/乱序到达均记录证据且只投一票；伪造签名的"证据"被拒。

**验收**：新增单测全绿；`cargo test -p n42-consensus --test integration_test fault_tolerance` 不回归。

### S5 — 坏块/无效分支缓存（liveness）

**背景**：gov5 P0 之首（codex-task-maturity-p0.md Task A）：执行无效的分叉块没有缓存，
catch-up / fetch-on-miss / direct-push 三条路径每 8 秒把同一个坏分叉重新拉回来重新执行失败，
死循环。n42-26 的对应面：`handle_import_done` 失败分支、sync 响应导入、以及 pr21 之后的
catch-up fan-out——目前失败块重新出现时会再次走完整 `new_payload` 流程。

**工作项**：
1. 在 orchestrator（`n42-consensus-service`）加一个有界坏块缓存（hash → 失败原因，LRU 512 即可）。
   **红线（gov5 原文）：只有确定性执行失败（EVM invalid、state root mismatch、共识规则违反）才标 BAD；
   `unknown ancestor`/`Syncing`/超时/IO 错误等本地暂态绝不能标 BAD**——否则把恢复路径自己堵死。
2. 三个消费点：sync 响应导入前查缓存跳过；direct-push/gossip 块到达时查；catch-up 请求结果过滤。
3. 与执行有效性下限工单（PR #21/#22）的关系：那边保证坏块不成为 head，这边保证坏块不反复消耗执行资源——互补，不冲突。
4. 附带（来自 N3）：确认 devlog-55 引入的 Rotor 路径有真实调用方（gov5 发现自家 Rotor 注册表是死代码、全部退化 gossip；n42-26 需 grep 确认不是同样情况，是则记 devlog 不必立即修）。

**验收**：单测——同一无效块第二次到达不触发 `new_payload`；`Syncing` 结果不入缓存；
缓存满淘汰正常。E2E 场景 4 + 9（recovery）不回归。

---

## 3. P1 任务（性能 / 健壮性）

### P1-1 — 批量 BLS 验证（性能，先测后做）

**背景**：gov5 `579845da` 用 blst 批量接口把投票验证提速 14.5×。n42-26 的 8s slot 预算中
**BLS verify 占 69%**（memory：Firedancer evaluation），是最大单项。
gov5 另有委员会消息"每块只 hash 到 G2 一次"与 scalar-sum 聚合优化（后者仅模拟器可用）。

**工作项**（遵循 measure-first 铁律）：
1. 先 bench：`cargo test -p n42-consensus --test performance_bench --release -- --ignored --nocapture`
   基线；确认当前投票/收据验证是逐个 pairing 还是已批量。
2. 若逐个：接入 blst 的 batch verify（Rust `blst` crate 的 `verify_multiple_aggregate_signatures`），
   投票聚合前批量验签、手机收据聚合路径同样处理；同一消息多签名者场景复用 hash-to-G2 结果。
3. 复测同一 bench，把前后数据写进 devlog。若提升 < 2× 则记录数据后停手，不为小收益加复杂度。

**验收**：bench 数据（前/后）；签名验证语义不变（坏签名混入批量时能定位并拒绝——blst 批量接口
需要随机加权，确认实现用了随机 scalar）；全部测试绿。

### P1-2 — Twig sidecar staged flush 模式

**背景**：gov5 07-18 发现 QMDB 的 FlushTo/Evict/TakeUndo 在块 MDBX 事务内就变更内存态，
事务回滚后内存 computer 已被污染（内存与磁盘发散）。修复：所有持久化副作用先暂存，
事务 durable 之后才 adopt 到内存态。n42-26 的 twig sidecar 有同构风险面：WAL append 与
内存树更新的顺序、以及与 reth 提交的耦合（twig-core-audit F1a/F1b 已立项修 WAL 边界）。

**工作项**：
1. 在 F1a/F1b 修复的基础上，审计 `n42-twig-core` + sidecar 桥接（`consensus_loop.rs` 的
   sidecar staging）：是否存在"先改内存、后落盘失败、内存不回退"的窗口。PR #22 的 staged diff
   （commit 门之后才 flush）已挡掉共识侧大头，这里查的是 twig 自身 WAL/快照写失败路径。
2. 若存在：改为 gov5 模式——暂存变更集，WAL fsync 成功后才 apply 内存树；失败则丢弃暂存并让
   F8（缺失 diff 检测，audit 已立项）接手。
3. 与 twig-core-audit 的 Codex 任务合并提交亦可，避免两个 PR 改同一文件打架。

**验收**：注入 WAL 写失败的单测——内存树 root 不前进、重启后恢复到最后 durable 版本、
无静默发散；现有 twig E2E（WAL crash recovery，devlog-70）不回归。

### P1-3 — state root 校验硬防线

**背景**：gov5 最痛教训之一（`75351eaa`）："稳定数周"其实是不验根的假稳定，QMDB 树漂移无人知晓。
n42-26 的 reth 默认逐块验根，但有两个豁免面：(a) `N42_SKIP_STATE_ROOT` / `N42_DEFER_STATE_ROOT`
环境变量仅 warn（h2-consensus-audit 已点名）；(b) twig sidecar root 与 reth root 无逐块对账
（twig 是影子树，漂移只在手机 proof 验证失败时才暴露）。

**工作项**：
1. `N42_SKIP_STATE_ROOT`：release build 下设置该变量直接拒绝启动（或要求显式
   `N42_I_KNOW_THIS_BREAKS_CONSENSUS=1` 双确认）；启动 banner 打印醒目 UNSAFE 标记。
2. twig root 对账：每块 twig 更新完成后，与该块的 reth state root **不做等值比较**（两棵树承诺
   不同，root 本来不同），而是加一致性探针——按固定采样（如每 N 块取 M 个账户）比对
   twig 值与 reth PlainState 值；不一致时告警 + 计数器（Prometheus），连续不一致触发 F8 的重建路径。
   采样率要低（手机低能耗铁律不受影响，此探针在 IDC 节点侧）。
3. 失败路径：探针发现发散时不 panic，标记 sidecar unhealthy、停发手机数据包，直到重建完成。

**验收**：单测（注入一个 twig 写错，探针在 N 块内检出并置 unhealthy）；E2E 场景 5（手机验证）不回归。

### P1-4 — 网络活性修复包

**背景**：gov5 Layer 6 实弹清单：控制消息限流误伤 trusted validator peer（活性杀手）、
静态 peer 断线不重拨、投票丢失不重发、fetch-on-miss 无限流、孤儿区间无限自旋。

**工作项**（对照 `crates/n42-network`）：
1. validator-mesh：已知验证者的 peer 连接标记 trusted，gossip 限流/评分对其豁免或阈值放宽
   （n42-26 用 libp2p GossipSub peer scoring——核对验证者 peer 是否可能被 score 踢出 mesh；
   有 explicit peering（`with_explicit_peers` 类似机制）就把验证者全放进去）。
2. 静态/验证者 peer 断线指数退避重拨（确认现有 reconnect 逻辑覆盖共识直连通道，不只 gossip）。
3. 投票重发：view 未推进且本节点已投票超过 T 秒 → 向 leader 重发投票（幂等，leader 去重）。
4. fetch-on-miss/catch-up 请求加节流（与 S5 的坏块缓存配合）。

**验收**：单测 + E2E 场景 9/10（recovery/chaos，手动跑一轮记录到 devlog）。

### P1-5 — 手机见证栈对照评估（产出文档，不动码）

**背景**：gov5 07-16~18 的 mobileverify 统一栈有四个 n42-26 值得对照的设计（M1–M3、§1 表）。
n42-26 已有 receipts/reward/StarHub 一套（devlog-13），但在"500 IDC 规模下碎片证书聚合"与
"注册表承诺进 header"两点上没有对应物。

**工作项**：写一份 `docs/mobileverify-gov5-comparison.md`，逐项回答：
1. 稀疏签名者掩码（delta-varint）vs n42-26 当前收据聚合的带宽对比（500 IDC × 10K 手机口径）。
2. cohort 跨节点合并是否解决 n42-26 的真实问题（n42-26 手机绑定单 IDC via StarHub，
   碎片化程度可能远低于 gov5——先算清楚再决定）。
3. header 锚（MobileRegistryRoot 可选字段 + 环形缓冲）vs n42-26 现状（手机注册表不上链？）——
   评估手机侧验证"注册表正确性"的信任假设是否需要这个锚。
4. 结论：每项给 做/不做/改造后做 + 理由。**手机低能耗铁律（memory）是评估的硬约束。**

**验收**：文档 + 结论表；不改代码。

### P1-6 — SELFDESTRUCT pre-wipe 对抗测试

**背景**：gov5 witness 重放战役发现：被毁合约的**未读 slot** 在重放端成为幽灵行、毁 state root
（`d7e70658`，wipes sidecar 修复）。n42-26 devlog-69 修过 execution 侧同族 bug
（EIP-6780 DestroyedChanged 误删），但 read_log 重放路径（手机侧）没有针对性对抗测试。

**工作项**：在 `n42-mobile` / `n42-execution` witness 测试中补三个用例：
1. 同块内 create→selfdestruct→同地址 re-create（EIP-6780 边界），手机重放 root 与 IDC 一致。
2. 被毁合约含**未被本块任何交易读过的** slot，重放后不残留幽灵状态。
3. Cancun 语义（同交易内 selfdestruct 才真删）与 pre-Cancun 差异路径各一条。
（devlog-93 已有 witness 截断/插入/篡改三个对抗测试，本任务是其 SELFDESTRUCT 专项续篇。）

**验收**：新用例全绿；若发现真 bug，按执行有效性工单同等级别处理并单独 PR。

---

## 4. P2 任务（调研 / 评估，各产出一份短文档）

### P2-1 — Twig 全历史层可行性调研

gov5 QMDB 全历史层（S-3：冻结叶树 + activeBits 分离、4B death stamp、ProofAtHeight）证明
append-only twig 结构可以用极小代价支持任意高度历史 proof。n42-26 的 twig 同为 append-only，
理论上同样可行。调研：(a) 手机验证是否需要历史 proof（当前只验最新块）；(b) 若需要，
death stamp 方案在 Rust twig 上的改造量；(c) root 公式是否需要 gov5 式破坏性变更
（`twigRoot = hashNode(leafRoot, bitsRoot)`）。参考 gov5 07-03 提交族 + `docs/datc历史proof秒级性能.txt`。
顺带评估 M4（K-块锚节奏）对手机流量的意义。**产出：docs 短文 + 做/不做结论。**

### P2-2 — 冷热分层归档调研

对照 gov5 归档三层（S-5）与 reth static files 现状，回答：n42-26 的 IDC 节点长跑数月后
（gov5 P2 也没有这个数据）磁盘增长曲线、是否需要 warm-CS 式裁剪窗口、手机 proof 需要的
历史深度。**产出：docs 短文；不实现。**

### P2-3 — 0x07 BLS 聚合批量交易评估

gov5 未合并分支上的 0x07 tx：N 个发送方一个 96B 聚合签名，txpool accept-and-expand。
对 n42-26 的意义：tx 转发给 leader 的 O(n) 路径上，验签是 CPU 大头（batch-transfer 快车道
devlog-81 已证明批量 ecrecover 的收益）。评估 BLS 聚合 tx 与现有 secp256k1 生态的兼容成本
（钱包不支持 BLS 签名是硬伤）。**产出：短评；预期结论倾向不做，但要把带宽/CPU 数据算出来。**

### P2-4 — 三客户端审计移植机制化

把 devlog-93 的"调研 reth/erigon/geth 近三月更新"固化为季度例行：每季度产出一份借鉴清单
（gov5 06-04 的做法：每客户端 3 个月 fix 审计 → 落地修复 PR）。本季度剩余项直接沿用
devlog-93 的阶段二清单。**产出：加入 docs 的例行清单模板 + 本季度执行。**

---

## 5. 明确不同步清单（含依据，Codex 勿自行扩大范围）

| 项 | 不同步理由 |
|---|---|
| order-then-execute 两相投票门 | gov5 实测否决：17.6-18.8s/块 vs 3.75s（`1842c367`）。n42-26 的乐观 R1 + commit 执行门（PR #21/#22）正是被该数据支持的路线 |
| import-gated voting 全盘替换乐观投票 | 同上；且 gov5 h2 audit 认可 Rust 侧路线，只要求执行门（已合并） |
| DATC | 62 提交未合并 gov5 main、工程量大；n42-26 无 archive/全历史 proof 需求（P2-1 调研会重新确认） |
| eth-el 全套（回放引擎、F2 压缩、四层发行、snapshot-direct、eldevp2p、Caplin B+） | 定位不同：n42-26 是自主链不是以太坊 EL；CL 重构已独立完成（devlog-88/89） |
| Block-STM / evmone / S3-FIFO 等执行层优化 | Go 侧实测多为否决或 Go 特有；n42-26 parallel-evm 有自己的数据（memory：measure-first） |
| BAL（EIP-7928） | n42-26 已判 no-go（read_log 更优；follower 侧 reth 自带） |
| LtHash、Verkle、PQ 交易、Tile、XDP、SALT、IBC、MEV/4337、AI/messaging/分布式存储市场 | gov5 自己已否决/搁置/与 n42-26 定位无关 |
| MDBX WriteMap/调优经验 | reth 自管 MDBX；仅备注：Windows 内存异常先查 WriteMap |
| JMT 优化族（GC/CachedStore/Haystack） | n42-26 已弃 JMT 默认路径，走 Twig 全 DRAM（devlog-63） |

---

## 6. 执行顺序与总验收

```
S1 → S2 → S3 → S4 → S5        （P0，串行，每个独立 PR，先审计后改码）
P1-1 ~ P1-6                    （P1，可并行，P1-2 与 twig-core-audit 工单协调）
P2-1 ~ P2-4                    （P2，文档产出，穿插做）
```

- P0 每项 PR 必须包含：审计结论（哪怕结论是"n42-26 不存在此问题"也要写进 PR 描述与 devlog）、
  修复（如需要）、回归测试。**审计发现"结构性免疫"时允许零代码变更收尾，但测试要补。**
- 全部 P0 完成后跑一轮完整 E2E（场景 1,3,4 + 5,8,12）并在 devlog 记录。
- 涉及共识行为变更的（S1/S2/S3）需评估协议兼容性：当前无主网，可直接改，不需要 fork 门控；
  但要在 devlog 里写明这是共识断代变更。
- 完成后在 `DEVLOG.md` 索引追加一行（建议 `devlog-94-gov5-sync-p0.md` 起）。
