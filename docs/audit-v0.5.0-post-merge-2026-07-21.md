# v0.5.0 合并后审计（2026-07-21）

> 对象：main @ `96250da`（v0.5.0，gov5 P0/P1/S5 + reth 2.4.1 升级合并后）
> 方法：同步确认（本地=origin=v0.5.0，无待拉取）+ 三面并行审计（合并接缝 / reth 2.4.1 漂移 / 整合后跨切面），
> 承重结论均由我亲自读 reth 2.4.1 与 n42-26 源码复核，非转述。
> 环境：`../reth` @ `chore/reth-upstream-20260719`（`c533db8ba`，reth 2.4.1）。

## 总结

三面里两面**完全干净**（合并接缝、reth 2.4.1 升级），第三面（整合跨切面）发现**一个 CONFIRMED 的 HIGH**——
S5 坏块缓存投毒的一个新子路径，四轮分支审查因只针对"声明 hash 不符"向量而漏掉。**非阻断已发布代码的安全，
但应作为 v0.5.1 的首要修复**（单 follower 精准 DoS；拜占庭 leader 可放大为链级 liveness 停摆）。

---

## 🔴 HIGH-1（CONFIRMED）：伪造 compact execution_output 绕过 S5 过滤，把诚实块拉黑

### 攻击链（逐环已读代码/源码证实）

1. `BlockDataBroadcast`（`crates/n42-consensus-service/src/orchestrator/mod.rs:111-130`）**无 leader 签名**；
   `execution_output: Option<Vec<u8>>`（compact-block 缓存 blob）是独立字段，**不被 block_hash 覆盖**，任意 peer 可控。
2. 拜占庭 peer 复制诚实 leader 广播的 `payload_json`（于是 `execution_data.block_hash()==H`，通过
   `execution_bridge.rs:523/761` 的信封-hash 校验），**只把 `execution_output` 换成伪造字节**，抢先直推给目标 follower。
3. follower：`should_skip(H)=false` → `inject(H, 伪造)`（`execution_bridge.rs:568-574`）→ `new_payload(H)`。
4. **reth 2.4.1 生产路径确认会拒绝伪造 bundle（无状态污染）**：`payload_validator.rs:780` 的 cache-hit 快路径
   只在 `n42_skip_state_root||n42_defer_state_root` 时触发，而 P1-3 已在生产启动硬拒这两个 bypass；故 cache-hit
   走标准路径，`prepare_n42_state_root_job(skip=false,defer=false)` 返回 `SynchronousStateRootJob`
   （`state_root_strategy/mod.rs:964`），其 `finish` 从伪造 bundle 的 state 真算 root，与头部诚实 `state_root` 不符
   → `payload_validator.rs:953` 返回 `ConsensusError::BodyStateRootDiff` → **`Invalid{validation_error:"…state root…"}`**。
5. 该 validation_error 是 **state-root-mismatch，不是 "block hash mismatch:"** → S5 的
   `is_declared_block_hash_mismatch`（`bad_block_cache.rs:187-192`）**放行** →
   `insert_if_invalid(H, "state root…")`（`execution_bridge.rs:816` 等）**成功把诚实 H 写入坏块缓存**。
6. 之后诚实块 H（正确 execution_output）到达 → `should_skip(H)=true` → 所有路径（eager/finalize/import/retry/
   **sync** `state_mgmt.rs:791`）丢弃。共识仍由诚实多数对 H 形成 CommitQC，但该 follower **无法执行已提交块 H、
   且无法通过 sync 自愈**，直到 512 项 LRU 挤出或**重启**（缓存不持久化）。

### 定级依据（为何 HIGH 而非 CRITICAL）

- reth **正确拒绝**伪造 bundle（`state_root != header.state_root()` → Invalid），**无状态污染/分叉**——排除 CRITICAL。
- 但坏块缓存的语义前提"reth 确定性拒绝了这个块=块本身坏"在 inject 路径下**不成立**：拒绝是被注入的伪造字节
  导致，而非块的确定性属性。S5 四轮修复只封了"声明 hash 不符"子路径，本子路径仍开。
- **可利用性**：单个拜占庭 peer 一次抢先直推即可精准拉黑一个诚实 follower 对某已提交块的执行与同步；
  **拜占庭 leader 更严重**——其为自己诚实提案的 H 广播 block_data 时植入伪造 execution_output，H 仍获 CommitQC，
  但所有走 compact-block 快路径的 follower 全部在 H 上卡死 → 链级 liveness 停摆，落在 BFT 容错预算内（单节点作恶）。

### 为何四轮 + 首三面审计前没抓到

现有回归测试（`mod.rs:5374-5410`）伪造的是 `payload_json`（block number 999）使 reth 返回 "block hash mismatch"，
正好被过滤器挡住；**没有建模"payload 诚实、只伪造 execution_output → state root mismatch"** 这一支。

### 修复建议

- **最小修复（推荐先做）**：inject 路径的 Invalid **不得**据以写坏块缓存。即：当本次 `new_payload` 前 inject 过
  compact `execution_output`、结果非 Valid 时，只 `evict(H)` 并当作**可重试**（不 `insert_if_invalid`）——
  因为该 verdict 可能反映被注入字节而非块本身。真正坏的块仍会在**非 inject 的全执行路径**上被 reth 确定性拒绝并正常入缓存。
  代价：带 compact output 的真坏块不进缓存、可能重试一次，有界可接受。
- **纵深防御（正解）**：给 `BlockDataBroadcast` 的 `execution_output`（连同 payload）加 leader 签名认证，
  使 peer 无法注入伪造 compact 字节。改动较大，可作 follow-up。
- **回归测试**：补"payload 诚实 + execution_output 伪造 → state-root-mismatch → H 不入坏块缓存、诚实 H 随后可 Valid 导入"。

---

## 🟡 LOW-1（CONFIRMED）：should_skip 早退不调用 discard_unvalidated_sidecar_diff

`execution_bridge.rs:729` 的 `should_skip` 早退跳过了 `discard_unvalidated_sidecar_diff`，sync 路径已把该 view 的
diff 暂存进 `pending_sidecar_diffs`（`state_mgmt.rs:684-698`），于是残留且拿不到 canonical 绑定，
`enqueue_confirmed_sidecar_state_diffs`（`consensus_loop.rs:1571-1576`）在该 view `break`。**净危害为零**——
能触发的块必是"已提交但不可执行"，执行本就卡在该 view，orphaned pending 与正常 missing 屏障等效。
建议 should_skip 早退前也调一次 discard 保持不变量一致。（注：HIGH-1 修复后此路径的触发面进一步收窄。）

---

## 三面干净结论（复核已通过）

**合并接缝（mod.rs warmup-floor × P1）**：真正正交。warmup 的 `leader_build_not_before` 门控与 rework 的
投票重发/catch-up/epoch 恢复代码区域不重叠；所有 leader build 触发路径都汇聚到唯一门控 `evaluate_leader_build_wait`
（floor 检查在 quorum 检查之前，不可绕过），rework 未新增/未修改该函数；两个构造点两组字段都初始化。无 CRITICAL/HIGH/MEDIUM。

**reth 2.4.1 / alloy 2.2.0 升级**：方向正确、无降级、承重依赖成立。**S5 字符串依赖在 2.4.1 下仍成立（YES）**——
alloy 2.2.0 `PayloadError::BlockHash` Display 逐字未变（`error.rs:60`）、reth `NewPayloadError::Eth` transparent 透传、
`on_new_payload_error` 仍走 `PayloadStatusEnum::from`（`mod.rs:3166`）；`take_payload_execution` API 与
`CachedPayloadData` tuple 契约两侧对齐；reth 2.4.1 集成对源码唯一改动是 n42 不引用的内部构造器参数；
n42-26 零业务代码适配即编过；Cargo.lock 与新基线一致，revm 41.0.0/alloy-evm 0.37.1/reth-primitives-traits 0.5.2 均升或平。
两个 LOW 长期健壮性建议：把 S5 的字符串匹配收敛为类型化 `is_block_hash_mismatch()` 判定（消除措辞漂移风险）；
exec cache 跨仓 tuple 契约列入升级检查清单。

**整合后其余面（S1/历史 QC/sync-2/持久化）**：
- S1 n-f quorum：所有 QC/CommitQC/TC/投票计数统一走 `ValidatorSet::quorum_size`，无 2f+1 硬编码残留，历史 QC 用当时集验证。
- prepared-lineage sync/2：信任边界无绕过，raw lineage 不完整时 fail-closed，cached output 仅链验证通过后消费
  （且即便伪造也被 reth state-root 校验挡下——与 HIGH-1 同一 reth 防线）。
- 快照/持久化：协议 v4、快照 v5、execution_validated_head 持久化三者独立命名空间不冲突，
  `advance_execution_validated_head` 原子配对更新 view+hash，恢复要求快照 hash==reth canonical head 否则 fail-closed。
- 一个 PLAUSIBLE 的 liveness 边角：`find_validator_set_by_len` 在"加一删一致两 epoch 同长"时可能按迭代序选错集
  → 合法块误拒（fail-safe，非安全问题），epoch 恢复用 staged-view 门控规避。

---

## 建议

1. **v0.5.1 首要**：修 HIGH-1（inject 路径 Invalid 不投毒坏块缓存 + 回归测试），可给 Codex。
2. 顺带：LOW-1 的 discard 一致性；S5 字符串匹配长期改类型化判定。
3. 已发布的 v0.5.0 无需回滚——HIGH-1 是 liveness/DoS 非安全/资金风险，且需拜占庭节点主动构造；修复走增量 v0.5.1。
