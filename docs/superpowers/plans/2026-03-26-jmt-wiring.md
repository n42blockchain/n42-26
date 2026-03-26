# JMT 生产接入计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将已实现但未接入的 `ShardedJmt` 接入生产启动流程，使 JMT 在每次 block commit 时更新状态树，RPC 端点返回真实数据。

**Architecture:** 参照 ZK ProofScheduler 的条件启用模式（`N42_ZK_PROOF=1`），添加 `N42_JMT=1` 环境变量控制。`ShardedJmt` 实例通过 `Arc<Mutex<>>` 共享给 orchestrator（更新）和 RPC（查询）。所有 builder 方法和 consensus_loop 中的更新代码已存在，仅需在 main.rs 中接线。

**Tech Stack:** `n42-jmt::ShardedJmt`, `Arc<Mutex<>>`, `tokio::task::spawn_blocking`

---

## 现状分析

已存在但未调用的代码：

| 位置 | 代码 | 状态 |
|------|------|------|
| `orchestrator/mod.rs:329` | `jmt: Option<Arc<Mutex<ShardedJmt>>>` 字段 | ✅ 存在 |
| `orchestrator/mod.rs:638` | `with_jmt()` builder 方法 | ✅ 存在，未调用 |
| `orchestrator/consensus_loop.rs:366-408` | `apply_diff` 更新逻辑 | ✅ 存在，死代码 |
| `rpc.rs:234` | `jmt: Option<Arc<Mutex<ShardedJmt>>>` 字段 | ✅ 存在 |
| `rpc.rs:253` | `with_jmt()` builder 方法 | ✅ 存在，未调用 |
| `rpc.rs:556-625` | `jmt_root/jmt_proof/jmt_version` 实现 | ✅ 存在，永远返回 "not enabled" |
| `consensus_state.rs:170` | `jmt_root: ArcSwap<Option<(u64, B256)>>` | ✅ 存在，从不填充 |

**需要做的仅是 main.rs 中 ~15 行接线代码。**

---

## Task 1: 在 main.rs 中构造 ShardedJmt 并接线

**Files:**
- Modify: `bin/n42-node/src/main.rs` (添加 JMT 构造和接线)

- [ ] **Step 1: 在 main.rs 中添加 JMT 构造（参照 ZK scheduler 模式）**

在 `bin/n42-node/src/main.rs` 中，找到 ZK scheduler 构造块（约 line 535 `let zk_scheduler = if env_bool("N42_ZK_PROOF")`），在其**之前**添加：

```rust
// --- JMT state tree ---
let jmt = if env_bool("N42_JMT") {
    let shard_count = env_parse::<usize>("N42_JMT_SHARDS").unwrap_or(16);
    let jmt = Arc::new(std::sync::Mutex::new(
        n42_jmt::ShardedJmt::new(shard_count),
    ));
    info!(target: "n42::boot", shards = shard_count, "JMT state tree enabled");
    Some(jmt)
} else {
    None
};
```

注意：需要确认 `ShardedJmt::new()` 的签名。如果不接受 shard_count 参数，使用 `ShardedJmt::default()` 或 `ShardedJmt::new()`。

- [ ] **Step 2: 将 JMT 传给 RPC server**

在 `extend_rpc_modules` 闭包中（约 line 595-601），在 `if let Some(scheduler) = rpc_zk_scheduler` 之后添加：

```rust
if let Some(ref jmt) = rpc_jmt {
    rpc_server = rpc_server.with_jmt(Arc::clone(jmt));
}
```

需要在闭包之前 clone 引用：
```rust
let rpc_jmt = jmt.clone();
```

- [ ] **Step 3: 将 JMT 传给 orchestrator**

在 orchestrator 构造链中（约 line 1099-1100 `if let Some(scheduler) = zk_scheduler`），添加：

```rust
if let Some(ref jmt) = jmt {
    orchestrator = orchestrator.with_jmt(Arc::clone(jmt));
}
```

- [ ] **Step 4: 编译检查**

Run: `cargo check -p n42-node-bin --all-targets` (或 `cargo check --all-targets`)

- [ ] **Step 5: 提交**

```bash
git add bin/n42-node/src/main.rs
GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
  git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" \
  -m "feat(node): wire ShardedJmt into production startup (N42_JMT=1)"
```

---

## Task 2: 验证接线后死代码激活

- [ ] **Step 1: 启动节点并验证 JMT RPC**

```bash
N42_JMT=1 target/release/n42-node --dev
# 在另一个终端：
curl -X POST http://localhost:8545 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"n42_jmtVersion","params":[],"id":1}'
# 预期：返回版本号（不再是 "JMT not enabled"）
```

- [ ] **Step 2: 查看日志确认 JMT 更新**

每次 block commit 后应看到类似日志：
```
INFO n42::cl::jmt: JMT updated version=N root=0x...
```

---

## 不在此计划范围内

- **Parallel EVM 清理**：死代码清理是独立任务
- **Admin RPC 鉴权**：需要设计鉴权方案（admin token / 签名验证），独立任务
