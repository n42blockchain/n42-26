# 审计：跨客户端 interop 分支 `feat/qmdb-h2-cross-client-main`（2026-07-21）

> 对象：`origin/feat/qmdb-h2-cross-client-main` @ `5f23ef0`（基于 main `96250da`，相对 main 7 提交、~3336 行）
> 配套 Go：`N42-gov5` `feat/qmdb-h2-cross-client-interop-v1` @ `0e17be4e`（本仓不审 Go，仅作字节对拍参照）
> 方法：3 面并行审计 + 全量构建 + 我亲自复核承重结论（门控隔离、HighTC 对称、QMDB DoS）。
> 环境：verify worktree @ 该分支，`../reth` @ `c533db8ba`（reth 2.4.1）。

## 一句话判定

**生产安全上可合并**：这批是 `N42_OBSERVER_MODE` 门控、默认关闭、只读的 observer 使能层，
**对生产共识/出块/网络零影响**（我亲自核实门控）。wire 编解码与 BLS 终局验证 soundness 通过。
唯一实质问题集中在 **QMDB 只读验证工具**内（2 个 HIGH + 若干 caveat），不威胁 n42-26 生产，但应回给 Codex 修/澄清。

## 构建

check + `clippy --all-targets -- -D warnings` 零警告；`cargo test --workspace` 44 套件全绿。Codex "全绿"声明属实。

## 三面结论

### 1. 门控隔离（安全根基）—— ✅ CONFIRMED 零生产影响（我亲自核实）
- 新 topic（`/n42/h2/4/ssz_snappy`、gov5 H2）**只在** `NetworkService::new_gov5_h2_observer` 订阅，
  唯一调用点在 `bin/n42-node/src/main.rs:996` 的 `if env_bool("N42_OBSERVER_MODE")` 分支内，该分支末 `return Ok(())`（:1055）。
- 生产两个构造入口（`new` :629、`new_with_expected_validator_peer_ids` :659）都传 `None,None` → topic 不订阅、
  `h2_v4_topic_hash=None` → gossip 新分支的 `is_some_and` 守卫恒 false，永不进入；现有 5 个 gossip 臂逐字未改。
- 新事件走 `reliable_data`（非 `consensus_event_tx`）；observer 无 BLS 私钥/无 ConsensusEngine，从不签名/投票/提案/推进 head。
- **只读名副其实，默认关闭，生产零影响。**

### 2. H2 v4 wire 编解码 —— ✅ clean（2 LOW）
- **HighTC 编解码对称性已修复（我亲自核实）**：Vote/Timeout/NewView 三处 high_tc + TC 内部成员，
  encode（`h2_wire.rs:362/415/435/516-518`）与 decode（`:343/395/423/498-501`）两侧同用 `MAX_TC=4096` 及 MAX_SIGNATURE/MAX_BITMAP/MAX_QC；有 `MAX_TC+1 → FieldTooLarge` 断言。Codex 声称修的不对称确已消除。
- 解码 DoS 有界：所有变长读先校验上界再切片，无 `with_capacity(untrusted)`，snappy 先 `decompress_len` 封顶防炸弹，无可达 panic。
- 签名域绑定正确：`N42H2V4 || phase || chain_id || genesis || view || block || changes`，跨链重放在解码即 `ChainIdentityMismatch` 拒绝；POP/NUL DST 隔离。
- testdata 是**真 gov5 字节向量**（SHA-256 与 devlog 声称值吻合），测试做 decode+reencode 对拍。
- LOW-1：可选 TC 尾字段 decode 非单射（省略 vs 显式空都→None，encode 恒产显式空）——observer 不转发，影响小。
- LOW-2：空 bitmap 在 wire 层被接受（终局层会拒）。

### 3. BLS 终局验证 —— ✅ CONFIRMED sound（2 LOW）
- 聚合公钥来自本地 `ValidatorSet`（`try_new` 时逐键子群校验），**不来自 wire** → rogue-key/POP 攻击不适用。
- NUL/POP 域隔离正确（`bls/mod.rs:8-12`，测试证明交叉验证失败）；消息严格绑定链身份，无短路；
  bitmap/quorum（n−f）强校验；失败即拒绝。伪造/拼凑 gov5 CommitQC **无法骗过**（需受信验证者集法定多数私钥）。
- 不触碰 `bad_blocks`/`exec_output_cache` → **非 HIGH-1 同类，无新生产无鉴权入口**。
- LOW：h2_v4 topic 未按链命名空间隔离（靠解码时 ChainIdentityMismatch 过滤，成本有界）；已验证 Decide 驱动 sync 目标（入块仍逐块校验 commit_qc，无法污染 head）。
- 依赖仅新增 `snap = "1.1"`（Snappy 纯 Rust，生态可信、必要）。

### 4. QMDB 跨客户端兼容 —— ⚠️ 隔离干净但 2 HIGH（均在只读验证工具内）
隔离 CONFIRMED（`lib.rs` +1 行挂模块、无生产调用方、hex 仅 dev-dep、自带独立类型不改生产状态树）；核心 root/proof soundness 正确、golden vector 真字节对拍。但：
- **HIGH-1（CONFIRMED，DoS）** `qmdb_compat.rs:154-156`：`verify_portable_stream` 在校验任何 entry 前
  `Vec::with_capacity(next_slot/TWIG_SIZE)`，`next_slot` 是头部攻击者可控 u64（唯一守卫 `entry_count==next_slot` 也可控）。
  头里谎报 `2^40` → 预留 ~17GB → OOM abort。影响面：离线验证工具读恶意快照文件（非网络节点）。
  修法：改 `Vec::new()`（本就追加增长）或设 cap。
- **HIGH-2（CONFIRMED，测试缺口）**：实际跑 87.8M-slot 全量 replay 的**流式路径 `verify_portable_stream` 零单测覆盖**；
  golden vector 只测 set/from_snapshot 路径。partial-twig/boundary 若与已测路径分叉，仓库测试抓不到。
  建议：加"流式读回 == pin 死 root"的测试。
- **MEDIUM-1（CONFIRMED）**：验证是自证的（`claimed_root`+digest 都在快照内、由提供方控制），外部锚只有 chain_id/genesis；
  工具无 `--expect-root`，"对拍"靠 operator 肉眼比对，未进代码/CI。建议加 `--expect-root` 让对拍自动化。
- **MEDIUM-2（PLAUSIBLE，关键范围问题）**：leaf = `blake3(0x01||key||value)`，**省略了 gov5 QMDB 的 serial/death-stamp 等字段**。
  故"字节级兼容"是**与专用 exporter 格式兼容**，未必等于 gov5 生产共识 QMDB root——除非 exporter 也用这套简化编码。
  本仓无法证实。**这决定"跨客户端对拍的到底是不是共识 root"**，Codex 应澄清（对照 Go `feat/...interop-v1` 的 exporter 叶编码）。
- MEDIUM-3（PLAUSIBLE）：非 2 幂 twig 数的 padding 路径未被任何向量覆盖（fixture 恰好 2 twig）；若 gov5 用 nullTwigRoot 而非零填充，非 2 幂时 root 分叉。
- LOW-1：流式路径缺 duplicate-active-key 去重（内存路径 `from_snapshot` 有），两路径 soundness 不一致。

## 合并建议

1. **生产安全角度可以合**：production-isolated、default-off、observer-only、build 全绿。合入 main 只带来独立 observer 脚手架，
   为下一阶段跨进程联调铺路，不影响任何现有节点。合并冲突仅 DEVLOG.md 双向索引追加（取并集）。
2. **回给 Codex（不阻断合并，但用于联调前）**：QMDB HIGH-1（DoS 改 `Vec::new()`）、HIGH-2（流式路径补测）、
   MEDIUM-1（`--expect-root` 让对拍进 CI）、**MEDIUM-2（澄清简化叶编码 == gov5 共识 QMDB root，否则整个"对拍"证明力存疑）**。
3. HIGH-1（compact-output 投毒）仍独立开着，与本批无关（本批不碰 bad_blocks）。
