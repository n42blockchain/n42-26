# Codex 留言：S5 坏块缓存 CRITICAL 修复（合并前唯一阻断项）

> 收件：OpenAI Codex
> 日期：2026-07-19
> 分支：`fix/gov5-sync-p1-rework-20260719`（在此分支上直接改，或新开 `fix/gov5-sync-s5-poison-fix` 从它拉出）
> 完整背景：**先读 `docs/codex-acceptance-gov5-sync-2026-07-19.md`** 的"第三轮验收"节（三轮验收清单，本项是唯一 CRITICAL）。
>
> **提交规范**：不要包含 "Claude" / "Codex" / "Co-Authored-By" 等字样。作者模板：
> ```
> GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
>   git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" -m "..."
> ```

## 一句话

返工分支质量很好、只差这一处就能合：**坏块缓存投毒 + 注入的 execution_output 失败不回收**，
组合成一个"单个 Byzantine peer 一次 sync 响应即可让恢复中节点对某真实块哈希永久 wedge"的活性攻击。

## 攻击链（为什么是 CRITICAL）

1. `import_and_notify`（`crates/n42-consensus-service/src/orchestrator/execution_bridge.rs:773-778`）与 eager 导入路径
   （同文件 `:568-572` 附近）在 `new_payload` **之前**把 peer 提供的 `broadcast.execution_output` 注入 reth
   payload cache，key = **发送方声明的** `broadcast.block_hash`。
2. 恶意 peer 构造：声明 hash = 某真实块 H、内容（含 execution_output）伪造。`95bba66` 的 lineage 预检只比对
   **声明字段之间**的一致性（envelope view/hash vs payload 声明 hash），伪造包能全过。
3. `new_payload` 在 well-formedness 阶段重算 header hash 不匹配 → 返回 `Invalid`。
4. 失败分支 `insert_if_invalid`（`:801` 及 eager 的 `:661`）把 **真实 H** 写进坏块缓存
   （`bad_block_cache.rs:88-99` 对**任何** `Invalid` 都写，不区分 hash-mismatch 型）。
5. 且注入的伪造 bundle **不回收**（三条失败分支 `Invalid`/`Syncing`/`Err` 均无 take，注入条目滞留 cache）。
6. 真实 H 随后到达 → 坏块缓存 `should_skip` 命中直接丢弃；即便绕过，cache-hit 取走伪造 bundle → state root
   不匹配 → 再次 Invalid。**节点对 H 永久 wedge，只有重启（缓存不持久化）能救**，破 f 容错活性。

## 修复（两处都要，缺一不可）

### Fix ① — 坏块缓存不接受 hash-mismatch 类 Invalid

`crates/n42-consensus-service/src/orchestrator/bad_block_cache.rs`，`insert_if_invalid`（:88）/
`insert_invalid_payload`（:102）：当 `validation_error` 属于"block hash mismatch / 声明哈希与重算不符"这类
**由发送方声明字段导致、而非区块本身确定性无效**的错误时，**不写缓存**（记 metric 即可）。
判据用 reth 返回的 `validation_error` 字符串匹配（reth 对此类返回 `PayloadError::BlockHash` / `latest_valid_hash == null`），
或在调用点区分。红线不变（任务书 S5）：只有**该 hash 所承诺的区块确定性执行失败**才可标 BAD。

### Fix ② — 注入的 execution_output 在非 Valid 时回收

`crates/n42-consensus-service/src/exec_cache.rs` 的 `trait ExecutionOutputCache` 目前只有 `inject` 和
`take_serialized`，**没有丢弃注入条目的方法**。新增一个 `fn evict(&self, hash: B256)`（node 侧
`RethExecutionOutputCache` 适配器实现为从 reth payload_cache 移除该 key），然后：

**凡是 `inject` 过、且 `new_payload` 结果不是 `Valid` 的路径，都要 `evict(hash)`**：
- `import_and_notify`：`:800-813` 的 `Invalid` 分支、`:786-799` 的 `Syncing/Accepted` 分支、`:815-818` 的 `Err` 分支。
- eager 导入：`:660-675` 的非 Valid 分支、`:676-` 的 `Err` 分支。
- retry_syncing 注入点（`:1029-1033`）与 leader_eager_import（`:1320`）对应的失败分支同理。

（只做 Fix ① 不够：坏块缓存不再毒化 H，但滞留的伪造 bundle 会让真实 H 每次 cache-hit 仍 Invalid。必须 ② 一起。）

## 验收标准（必须补测试）

1. **投毒测试**：mock EL，构造"声明 hash = H、内容不符"的 sync/lineage 输入 → 断言 H **不进**坏块缓存、
   注入条目被 evict；随后真实 H 到达能正常 `Valid` 导入、head 前进到 H。（这是首轮验收报告"missing test"
   里点名的困难情形，此前只测了"payload 字段 ≠ 声明"的简单情形。）
2. **确定性无效仍标 BAD**：一个真正 EVM-invalid / state-root-mismatch（非 hash-mismatch）的块，第二次到达
   仍被 `should_skip` 跳过、不重复 `new_payload`（保持 S5 原有正确行为，别改坏）。
3. `cargo test --workspace`、`cargo clippy --all-targets -- -D warnings` 全绿。
4. 在 `docs/` 增量记一条 devlog（S5 投毒修复 + 两处 fix + 攻击链），并更新 `DEVLOG.md` 索引一行。

## 顺带（本轮一起清掉的 MEDIUM/LOW，非阻断）

- `docs/devlog-96-view-proof-gates.md:99` "wire protocol remains version 3" 改为 4
  （实际 `crates/n42-primitives/src/consensus/messages.rs:251` `CONSENSUS_PROTOCOL_VERSION = 4`）。
- `collect_future_timeout`（`crates/n42-consensus/src/protocol/timeout.rs:17`）是共识行为变更，
  在 devlog-104 里逐条点名声明"共识断代变更"（目前只有 devlog-109 笼统兜底）。
- `95bba66` sidecar StateDiff staging（`state_mgmt.rs:617`）flush 时绑定校验 (hash, diff) 与该 view
  canonical 导入 hash 一致。
- lineage 响应按 16MB 帧预算截断（`state_mgmt.rs:404` + `n42-network/src/state_sync.rs`）；
  Twig 探针候选 storage_slots 加上限；bitfield v2 加"不可回退旧版读同一 datadir"降级警示；
  `mobileverify-gov5-comparison.md` 补进 DEVLOG 索引。

## 已经确认没问题的（不用再动）

S1-S4、BLS 批量（`b8e055b` 已修域绑定）、Twig staged flush、state-root 探针、网络恢复、epoch 恢复
（`6528f60`）、selfdestruct（跨交易 wipe 传播已修，"同交易 recreate"经 revm 41 源码复核不可达）、
prepared-lineage sync/2 的链验证骨架——均已通过验收。合并纪律：用 `fix/gov5-sync-p1-rework-20260719`
（已含 P0 全栈），**勿再合并独立的 S1-S5 草稿分支**。
