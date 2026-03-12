# 日志-51: JMT + Blake3 Jellyfish Merkle Tree 集成

日期: 2026-03-11

## 设计决策

### 为什么选择 JMT + Blake3

| 方案 | 优势 | 劣势 |
|------|------|------|
| **MPT + Keccak (现状)** | EVM 兼容 | 慢、量子不安全、proof 大 |
| **Poseidon Merkle** | ZK 友好 | CPU 上慢、复杂度高 |
| **JMT + Blake3 (选定)** | 10x 更快哈希、proof 小、量子安全 | 需维护双树 |

### 架构：并行双树

```
N42 共识层                    reth 内部（不动）
┌─────────────┐              ┌─────────────┐
│ JMT + Blake3 │ ←并行维护→  │ MPT + Keccak │
│ (新增)       │              │ (skip_root)  │
└──────┬──────┘              └──────┬──────┘
       │                            │
   JMT root → block header     reth state → 内部管理
```

- reth 的 MPT state_root 通过 `n42_skip_state_root` 跳过
- JMT root 由 N42 共识层独立维护，写入 block header
- 两棵树完全独立，不影响 reth 内部逻辑

### 为什么用 Penumbra `jmt` crate（非 Aptos monorepo）

1. **独立 crate** (jmt v0.12.0, 245K downloads) vs 整个 monorepo
2. **可插拔 `SimpleHasher`** — 20 行实现 Blake3
3. **`TreeReader`/`TreeWriter`** 抽象 — 可换存储后端
4. **Apache-2.0** — 无许可证问题
5. **UpdateMerkleProof** — 增量证明功能，移动端友好

### 16-shard 并行设计

- 按 KeyHash 首字节高 4 位分到 16 个 shard
- 每个 shard 独立 `N42JmtTree` + `MemTreeStore`
- rayon 并行 `apply_batch`
- 组合根: `blake3(shard_0_root || ... || shard_15_root)`
- 空分片不 bump version（避免查询空树根）

## 实施细节

### 新 crate: `crates/n42-jmt/`

| 模块 | 行数 | 功能 |
|------|------|------|
| `hasher.rs` | ~25 | Blake3Hasher (SimpleHasher impl) |
| `keys.rs` | ~35 | 地址/存储 → KeyHash 编码，含 domain separator |
| `store.rs` | ~90 | MemTreeStore (TreeReader + TreeWriter impl) |
| `tree.rs` | ~200 | N42JmtTree 核心：apply_diff, get, proof |
| `sharded.rs` | ~180 | ShardedJmt: 16-shard 并行 wrapper |
| `proof.rs` | ~130 | JmtProof: 可序列化证明 + 移动端验证 |
| `metrics.rs` | ~35 | Prometheus 指标定义 |
| **总计** | **~695** | |

### Key 编码

- 账户: `blake3("n42:account:" || address_20_bytes)` → KeyHash (32 bytes)
- 存储: `blake3("n42:storage:" || address_20_bytes || slot_32_bytes)` → KeyHash
- Domain separator 防止跨类型碰撞

### 账户叶子值编码

`[balance_32_be][nonce_8_be][code_hash_32_or_zeros]` = 72 bytes 固定长度

### Prometheus 指标

| 指标 | 类型 | 说明 |
|------|------|------|
| `n42_jmt_update_ms` | histogram | 每次 apply_diff 耗时 |
| `n42_jmt_root_ms` | histogram | 根哈希计算耗时 |
| `n42_jmt_version` | gauge | 当前树版本 |
| `n42_jmt_leaf_count` | gauge | 每次更新的叶子数 |
| `n42_jmt_updates_total` | counter | 更新总次数 |
| `n42_jmt_proof_ms` | histogram | 证明生成耗时 |
| `n42_jmt_proof_size_bytes` | histogram | 证明大小 |
| `n42_jmt_proofs_generated_total` | counter | 生成证明总数 |
| `n42_jmt_verify_ms` | histogram | 验证耗时 |
| `n42_jmt_verify_success_total` | counter | 验证成功计数 |
| `n42_jmt_verify_failure_total` | counter | 验证失败计数 |
| `n42_jmt_sharded_update_ms` | histogram | 分片并行更新耗时 |
| `n42_jmt_shard_count` | gauge | 分片数 |
| `n42_jmt_sharded_total_keys` | gauge | 分片更新总 key 数 |

### 移动端证明结构 (JmtProof)

```rust
struct JmtProof {
    shard_index: u8,            // 0-15
    shard_roots: Vec<[u8; 32]>, // 16 个分片根 (512 bytes)
    proof_bytes: Vec<u8>,       // 序列化的 SparseMerkleProof
    key: [u8; 32],              // 被证明的 key
    value: Option<Vec<u8>>,     // 值 (None = 排除证明)
}
```

验证步骤:
1. `blake3(shard_roots)` == expected_root?
2. `shard_roots[shard_index]` 作为分片根
3. SparseMerkleProof.verify(shard_root, key, value)

### 依赖

- `jmt = "0.12"` — Penumbra JMT (via anyhow, borsh)
- `blake3 = "1"` — SIMD 加速哈希
- `rayon = "1.10"` — 并行迭代
- `parking_lot = "0.12"` — 高性能 RwLock
- `anyhow = "1"` — jmt crate 要求

## 测试

25 个单元测试全部通过:
- hasher: 确定性、增量哈希
- keys: 地址/存储 key 唯一性、域分离
- store: 空存储、读写、版本范围查询
- tree: 创建/修改/销毁账户、存储 slot、证明生成
- sharded: 基本操作、确定性、分布均匀性、版本递增
- proof: 包含证明、排除证明、错误根检测、大小合理性

## 后续计划

1. **集成到 orchestrator**: execution_bridge 中调用 `sharded_jmt.apply_diff()`
2. **JMT root 写入 block header**: 替换或补充 state_root 字段
3. **持久化存储**: MemTreeStore → reth DB (或 RocksDB)
4. **移动端 SDK**: JmtProof 的 Dart/Swift/Kotlin FFI
5. **性能基准**: 在 58K cap 压测中测量 JMT 更新延迟
