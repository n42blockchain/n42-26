# 开发日志-52: JMT 全面集成

> 日期：2026-03-11
> 阶段：n42-jmt crate 全面集成到节点运行时

---

## 概述

将已完成的 n42-jmt crate（ShardedJmt + Blake3 + 16-shard 并行）集成到节点的 5 个层面：
1. Orchestrator 共识循环集成
2. RPC 暴露 JMT root/proof/version
3. 快照持久化（值级序列化）
4. 移动端 Dart SDK
5. 性能基准（criterion）

## Task 1: Orchestrator 集成

### 设计决策

- **异步后台更新**：JMT apply_diff 不在共识关键路径上。在 `handle_block_committed()` 中提取 BundleState，`spawn_blocking` 后台执行 JMT 更新
- **数据提取时机**：在 `finalize_committed_block()` 之前从 `pending_block_data` 提取 BlockDataBroadcast 的 execution_output（finalize 会清除 pending_block_data）
- **ShardedJmt 生命周期**：`Option<Arc<Mutex<ShardedJmt>>>` 存储在 ConsensusOrchestrator，通过 `with_jmt()` builder 注入
- **容错**：如果 pending_block_data 中没有该块数据（Case C 竞态），跳过此次 JMT 更新并记录 debug 日志，不影响共识

### 修改文件

| 文件 | 修改 |
|------|------|
| `crates/n42-node/Cargo.toml` | 添加 `n42-jmt.workspace = true` |
| `crates/n42-node/src/orchestrator/mod.rs` | 添加 `jmt` 字段、`with_jmt()` builder、import |
| `crates/n42-node/src/orchestrator/consensus_loop.rs` | `handle_block_committed()` 中提取 StateDiff + spawn JMT 更新；添加 `extract_state_diff_for_jmt()` helper |

### 关键逻辑

```
handle_committed(view, hash, qc):
  ...
  head_block_hash = hash

  // JMT 后台更新 (新增)
  if jmt.is_some():
    diff = pending_block_data[hash] → bincode deserialize → zstd decompress
         → serde_json CompactBlockExecution → StateDiff::from_bundle_state
    if diff.is_some():
      spawn_blocking:
        tree.apply_diff(&diff) → log (version, root, elapsed_ms)
        state.update_jmt_root(version, root)
        if block_count % 100 == 0: tree.prune(200)

  // 原有流程继续
  staking_scan...
  finalize_committed_block...
```

## Task 2: JMT Root 暴露

### 设计决策

- **不修改 block header**（不动 reth），通过 RPC + metrics + SharedConsensusState 传递
- SharedConsensusState 添加 `jmt_root: ArcSwap<Option<(u64, B256)>>`，lock-free 读
- N42RpcServer 持有 `Option<Arc<Mutex<ShardedJmt>>>` 用于 proof 生成
- metrics gauge `n42_jmt_latest_root` 供 Prometheus 监控

### 新增 RPC 方法

| 方法 | 签名 | 说明 |
|------|------|------|
| `n42_jmtRoot` | `() → {version, root}` | 最新 JMT root 和版本号 |
| `n42_jmtProof` | `(address, storageSlot?) → JmtProofResponse` | 账户或存储槽的 Merkle 证明 |
| `n42_jmtVersion` | `() → u64` | 当前 JMT 版本（块数） |

### 修改文件

| 文件 | 修改 |
|------|------|
| `crates/n42-node/src/consensus_state.rs` | 添加 `jmt_root` 字段、`update_jmt_root()`、`load_jmt_root()` |
| `crates/n42-node/src/rpc.rs` | 添加 `JmtRootResponse`/`JmtProofResponse` 类型、3 个 RPC 方法实现、`with_jmt()` builder |

## Task 3: 快照持久化

### 设计决策

- **值级快照**而非 MDBX：jmt crate 的 Node/NodeKey 类型不支持 serde，无法直接序列化树结构
- 方案：导出所有 live key-value 对 + 版本号 → bincode + zstd 压缩 → 原子写入文件
- 恢复：从快照加载 → 按 shard 分组 → apply_batch 重建树 → 验证 root hash 一致
- 原子写入：先写 `.tmp` 文件再 rename，防止断电损坏
- 典型大小：10K 条目约几十 KB（zstd 压缩后）

### 新增文件

| 文件 | 说明 |
|------|------|
| `crates/n42-jmt/src/snapshot.rs` | `JmtSnapshot` 类型、`save_snapshot()`、`load_snapshot()`、`ShardedJmt::snapshot()`/`from_snapshot()` |
| `crates/n42-jmt/src/store.rs` | 添加 `dump_latest()` 方法、`latest_version` 追踪 |
| `crates/n42-jmt/src/sharded.rs` | 添加 `from_parts()`、`shards()` 内部方法 |

### 测试覆盖

- `snapshot_roundtrip`: 内存序列化/反序列化 root hash 一致性
- `snapshot_file_roundtrip`: 文件写入/加载 root hash 一致性
- `load_nonexistent_file`: 不存在的文件返回 None
- `snapshot_empty_tree`: 空树快照/恢复

## Task 4: 移动端 Dart SDK

### 设计决策

- **Pure Dart 实现**：仅需验证逻辑，不需要完整树
- 使用 `hashlib` package 提供 Blake3（纯 Dart，跨平台，无 FFI）
- SparseMerkleProof 验证逻辑：约 200 行
- 域分离 key hash 与 Rust 端一致：`blake3(b"n42:account:" || address)`

### 新建文件

| 文件 | 说明 |
|------|------|
| `n42appv2/packages/n42_jmt_verify/pubspec.yaml` | Package 配置 |
| `n42appv2/packages/n42_jmt_verify/lib/n42_jmt_verify.dart` | Library export |
| `n42appv2/packages/n42_jmt_verify/lib/src/blake3_hash.dart` | Blake3 hash 封装、accountKey、storageKey |
| `n42appv2/packages/n42_jmt_verify/lib/src/jmt_proof.dart` | JmtProof 数据结构 + verifyRoot() + verifyShardRouting() |
| `n42appv2/packages/n42_jmt_verify/lib/src/sparse_merkle_proof.dart` | SparseMerkleProof + SparseMerkleLeaf 验证逻辑 |
| `n42appv2/packages/n42_jmt_verify/test/jmt_verify_test.dart` | 9 个单元测试 |

## Task 5: 性能基准

### 新建文件

| 文件 | 说明 |
|------|------|
| `crates/n42-jmt/benches/jmt_bench.rs` | criterion 基准测试 |

### 测试场景

| Benchmark | 参数 | 说明 |
|-----------|------|------|
| apply_diff/accounts_only | 100/1K/10K/50K | 纯账户变更性能 |
| apply_diff/accts_slots | 1K×5/10K×2/5K×10 | 含存储槽变更 |
| root_hash | 1K/10K | 16-shard root 计算 |
| proof_generation | 10K 树 | 账户/存储证明生成 |
| proof_verify | 1K 树 | 证明验证 |
| incremental_update | 100/500/1K | 增量块更新 |

## 编译与测试结果

- `cargo check -p n42-node -p n42-jmt` ✅ 无错误
- `cargo clippy -p n42-node -p n42-jmt` ✅ 无新增 warning
- `cargo test -p n42-jmt` ✅ **41 tests passed**（原 37 + 新增 4 snapshot 测试）
- `cargo test -p n42-node` ✅ **171 passed, 2 pre-existing failures**（orchestrator 超时测试，与 JMT 无关）

## 后续计划

1. **节点初始化接入**：在 `NodeBuilder` 中创建 ShardedJmt 实例，注入 orchestrator 和 RPC
2. **启动恢复**：加载 JMT 快照 → replay 缺失块重建
3. **定期快照**：每 1000 块或 epoch 边界写入快照文件
4. **Dart 测试**：`dart test` 验证移动端 SDK
5. **集成测试**：7 节点测试网观察 `n42_jmt_sharded_update_ms` metrics
6. **RPC 端到端**：调用 `n42_jmtRoot` 和 `n42_jmtProof` 验证返回值
