# Codex 留言：HIGH-1 坏块缓存 compact-output 投毒修复（v0.5.1 唯一阻断项）

> 收件：OpenAI Codex
> 日期：2026-07-21
> 背景全文：**先读 `docs/audit-v0.5.0-post-merge-2026-07-21.md`**（HIGH-1 一节含完整攻击链、触发门槛、修法与定级依据）。
> 分支：从 main（含 v0.5.0）拉 `fix/high1-compact-output-poison`。
>
> **提交规范**：不要包含 "Claude" / "Codex" / "Co-Authored-By" 等字样。作者模板：
> ```
> GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
>   git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" -m "..."
> ```

## 一句话

S5 的坏块缓存过滤只堵了"声明 hash 不符"一条投毒路径。拜占庭节点可保留诚实 payload（hash 正确、过信封校验）、
**只伪造 compact-block 的 `execution_output` 缓存字节** → reth 用伪造 bundle 算出 state root 不符 →
返回 `Invalid{"state root mismatch"}`（不是 "block hash mismatch"）→ S5 过滤器放行 → 诚实块 H 被拉黑 →
follower 无法执行/同步已提交块 H，直到重启。

## 已坐实的事实（读代码/reth 源码证实，不必重查）

1. `BlockDataBroadcast`（`crates/n42-consensus-service/src/orchestrator/mod.rs:111-130`）**无签名**；
   `execution_output` 是独立字段、不被 block_hash 覆盖。
2. **触发门槛**：直推路径有验证者鉴权（`n42-network/src/service.rs:1390`），但 **gossip 路径无鉴权**
   （`service.rs:1115-1128`，`block_announce_topic`）——任意 mesh peer 的 block_data 被 `handle_block_data` 无来源校验处理
   （`mod.rs:2498` 拿到 `source` 只打日志丢弃）。去重是 first-wins hash（`pending_block_data`）。故拜占庭 leader 天然赢竞争、
   gossip 里的拜占庭节点抢先亦可。
3. **reth 2.4.1 生产路径正确拒绝伪造 bundle（无状态污染，故 HIGH 非 CRITICAL）**：cache-hit 快路径
   （`reth/crates/engine/tree/src/tree/payload_validator.rs:780`）只在 `N42_SKIP/DEFER_STATE_ROOT` 时触发，
   而 P1-3 已在生产启动硬拒；故走标准路径 `SynchronousStateRootJob`（`state_root_strategy/mod.rs:964`）真算 root，
   与头部不符 → `payload_validator.rs:953` 返回 `BodyStateRootDiff` → `Invalid{validation_error:"…state root…"}`。
4. S5 过滤器 `is_declared_block_hash_mismatch`（`bad_block_cache.rs:187-192`）只认 `"block hash mismatch:"` 前缀，
   放行 state-root-mismatch → `insert_if_invalid(H, …)` 把诚实 H 写入缓存 → 之后所有路径（含 sync `state_mgmt.rs:791`）`should_skip` 丢弃。

## 修复（做这个，别用更简单的版本）

**inject 路径拿到非 Valid → `evict(H)` → 不带任何注入缓存地重跑一次 `new_payload`（强制全执行）→ 用这次干净裁决决定导入与是否入坏块缓存。**

- 投毒（payload 诚实、output 伪造）→ 干净全执行算出正确 root = **Valid** → 不缓存，且顺带把 H 正常导入；
- 真坏块（payload 本身坏）→ 干净重跑仍 Invalid → 正常入坏块缓存。

诚实 payload_json 已过 hash 校验，全执行裁决与被注入字节无关——既堵投毒、又保住 S5 的 DoS 上界、还避免误杀。
代价：失败路径多一次全执行（罕见/攻击场景，可接受）。

**改动点**（所有"先 inject 后 new_payload"的失败分支）：
- `crates/n42-consensus-service/src/orchestrator/execution_bridge.rs`：`import_and_notify` 的 Invalid 臂（`:816` 附近）、
  eager 导入（`:660-668`）、syncing retry（`:1145`）、leader eager（`:1356`）；
- `crates/n42-consensus-service/src/orchestrator/consensus_loop.rs`：committed 导入（`:1017`）；
- observer 对应路径（`observer.rs`）。
建议抽一个公共 helper：`fn resubmit_clean_and_decide(hash, execution_data) -> verdict`（evict → new_payload 无注入 → 返回裁决），
各失败分支在"本次注入过 compact output"时改调它，非注入路径维持原逻辑。

> 🔴 **不要**用"inject 路径 Invalid 就直接跳过 `insert_if_invalid`"这个更简单的版本：总带 compact output 到达的真坏块
> 在 follower 上不会自然落到非 inject 全执行路径，于是永不缓存 → 重开 S5 本要堵的每次到达都 new_payload 的 DoS。
> 必须靠干净重跑区分投毒与真坏块。

## 验收标准

1. **投毒回归测试**：mock EL，构造"payload 诚实 + execution_output 伪造"的 block_data → 首次 new_payload（带注入）返回
   state-root-mismatch → 断言 evict + 干净重跑 Valid → **H 不入坏块缓存**、被正常导入、head 前进到 H。
   与现有 `mod.rs:5374-5410`（伪造 payload_json → block hash mismatch）明确区分。
2. **真坏块仍缓存**：payload 本身无效（干净重跑也 Invalid）→ H 入坏块缓存、第二次到达被 `should_skip` 跳过、不重复 new_payload。
3. `cargo test --workspace` + `cargo clippy --all-targets -- -D warnings` 全绿（`../reth` 需在 `chore/reth-upstream-20260719`）。
4. 补 devlog + `DEVLOG.md` 索引；顺带修 **LOW-1**（`execution_bridge.rs:729` 的 `should_skip` 早退前也调一次
   `discard_unvalidated_sidecar_diff`，保持 sidecar 不变量一致）。

## 纵深防御（可作独立 follow-up，本轮不必）

给 `BlockDataBroadcast` 的 `execution_output`（连同 payload）加 leader 签名认证，从源头杜绝 gossip 路径注入伪造 compact 字节。

## 已确认无需再动

合并接缝（warmup×P1 正交）、reth 2.4.1 升级（S5 字符串依赖在 2.4.1 仍成立、exec cache API 对齐）、
S1 n-f quorum / sync-2 信任边界 / 快照持久化整合——首轮四审 + 本轮三面审计均通过。合并用本新分支，别动独立草稿分支。
