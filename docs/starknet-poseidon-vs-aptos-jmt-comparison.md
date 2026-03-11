# Starknet Poseidon Merkle Tree vs Aptos Jellyfish Merkle Tree: 技术对比

## 1. 树结构 (Tree Structure)

| 特性 | Starknet Poseidon Patricia Trie | Aptos Jellyfish Merkle Tree (JMT) |
|------|--------------------------------|-----------------------------------|
| **树类型** | Binary Sparse Merkle-Patricia Trie | 16-ary (nibble-based) Sparse Merkle Tree |
| **最大深度** | 251 层 (高度 251, 匹配 Stark field 251 bit) | 256 bit key → 64 nibble 层 (每 nibble 4 bit, 但内部展开为 4 层二叉树 = 256 层逻辑深度) |
| **分支因子** | 2 (Binary: 每个节点最多左/右两个子节点) | 16 (每个 InternalNode 最多 16 个 children, 按 nibble 索引 0-15) |
| **节点类型** | 3 种: **Binary** (左右子 hash), **Edge** (压缩路径 + 子 hash), **Leaf** (值) | 3 种: **InternalNode** (16-child, 内部为 4 层二叉子树), **LeafNode** (account_key + value_hash), **Null/Placeholder** |
| **稀疏区域处理** | Edge 节点路径压缩 — 将连续单分支路径折叠为一条边 (length, path, child_hash), 空子树表示为 triplet (0,0,0) | 叶节点上浮 — 任何仅含 0 或 1 个叶的子树直接替换为 Placeholder 或 LeafNode, 避免存储空中间层 |
| **状态组织** | 两棵独立 trie: **Contract Trie** (高度 251, key=contract_address) + **Class Trie** (高度 251, key=class_hash); 每个合约内部还有一棵 **Storage Trie** (高度 251) | 单一 JMT 存储所有状态; key = SHA3-256(account_address), value = account state blob |

### Starknet 状态承诺公式

```
state_commitment = Poseidon("STARKNET_STATE_V0", contract_trie_root, class_trie_root)
```

Contract Trie 叶节点:
```
leaf = Pedersen(Pedersen(Pedersen(class_hash, storage_root), nonce), 0)
```

Class Trie 叶节点:
```
leaf = Poseidon("CONTRACT_CLASS_LEAF_V0", compiled_class_hash)
```

> **注意**: Starknet 混合使用 Poseidon 和 Pedersen — state_commitment 和 class trie 用 Poseidon, contract trie 叶节点仍用 Pedersen (截至 v0.13.x, 正在向全 Poseidon 迁移)。

### Aptos 状态承诺

```
state_root = JMT.root_hash()  // SHA3-256, 32 bytes
leaf_hash = SHA3-256(LEAF_DOMAIN_SEPARATOR || account_key || value_hash)
internal_hash = SHA3-256(INTERNAL_DOMAIN_SEPARATOR || left_child || right_child)
```

---

## 2. 密码学详情 (Cryptographic Details)

| 特性 | Starknet (Poseidon) | Aptos (SHA3-256 / Keccak) |
|------|---------------------|--------------------------|
| **Hash 函数** | Poseidon (基于 Hades 置换, 3 元素状态宽度) + Pedersen (椭圆曲线点乘) | SHA3-256 (NIST 标准 Keccak) |
| **Hash 输出大小** | 251 bit (~32 bytes, Stark field element, felt252) | 256 bit (32 bytes) |
| **素数域** | p = 2^251 + 17 * 2^192 + 1 | N/A (bitwise hash) |
| **安全等级** | ~126 bit (collision resistance for 251-bit output) | 128 bit (collision), 256 bit (preimage) |
| **S-box** | x^3 (cube, 适合 Stark field 特征) | Keccak sponge (bitwise) |
| **轮数** | 8 full rounds + ~83 partial rounds (state width 3, 具体参数由 CryptoExperts 提供) | 24 Keccak-f[1600] rounds (SHA3) |
| **Commitment 大小** | 32 bytes (1 felt252) | 32 bytes (SHA3-256 digest) |

### Hash 性能对比

| Hash 函数 | 原生 CPU 速度 (ns/hash) | ZK 约束数 (R1CS) | ZK 约束数 (STARK/AIR) | MB/s (native CPU) |
|-----------|----------------------|-----------------|---------------------|-------------------|
| **Poseidon (BN254 field, t=3)** | ~4,000-8,000 ns | **~250-350** | **~300-400** | ~4-8 MB/s |
| **Pedersen (StarkCurve)** | ~40,000-80,000 ns | ~750-1,500 | ~1,000-2,000 | ~0.4-0.8 MB/s |
| **SHA3-256 (Keccak)** | **~200-400 ns** | ~25,000-30,000 | ~150,000+ | **~500-800 MB/s** |
| **SHA-256** | ~100-300 ns (with HW accel) | ~25,000 | ~100,000+ | ~1,000+ MB/s (SHA-NI) |
| **Blake3** | ~50-100 ns | ~10,000-15,000 | ~50,000+ | ~2,000+ MB/s |
| **Poseidon2 (优化版)** | ~2,000-4,000 ns | **~150-200** | **~170-250** | ~8-16 MB/s |

> **关键洞察**: Poseidon 在原生 CPU 上比 SHA3 慢 **10-40x**, 但在 ZK 电路中快 **70-100x**。这是 Starknet 选择 Poseidon 的根本原因 — STARK 证明成本主导了系统总成本。

Aptos 优化的 `hash_sha3_256_two_x32b()` 函数可以在单次 Keccak-f[1600] 置换中完成两个 32-byte child hash 的合并 (总输入 96 bytes < rate 136 bytes), 极大优化了树内部节点 hash。

---

## 3. 数据大小 (Data Sizes)

| 指标 | Starknet | Aptos |
|------|----------|-------|
| **Hash 输出** | 32 bytes (251 bit felt) | 32 bytes (256 bit) |
| **单个节点磁盘大小** | Binary: ~64 bytes (2 x 32-byte hash); Edge: ~65-97 bytes (length u8 + path ≤32 bytes + child 32 bytes); Leaf: ~128 bytes (class_hash + storage_root + nonce + padding) | InternalNode: ~100-550 bytes (2 x u16 bitmap + up to 16 children, 每个 child = 32 byte hash + varint version + type); LeafNode: ~100 bytes (32 byte key + 32 byte value_hash + version) |
| **单 key 证明大小** | ~251 x 32 = **~8 KB** 最坏情况 (251 层 sibling hashes); 实际因 Edge 压缩约 **1-4 KB** | ~64 x 32 = **~2 KB** 最坏情况 (64 nibble 层); 实际因叶上浮约 **0.5-2 KB** |
| **批量证明 (per key)** | 共享前缀路径可摊薄, ~0.5-2 KB/key | 共享 nibble 前缀, ~0.3-1.5 KB/key |
| **状态数据库大小 (生产)** | Pathfinder 全节点: 估计 **100-200 GB** (含历史); 纯状态 trie 约 **30-60 GB** | 验证者节点推荐 **3 TB SSD** (含 ledger history); 纯状态 JMT 估计 **200-500 GB** |

---

## 4. 性能指标 (Performance Metrics)

| 指标 | Starknet | Aptos |
|------|----------|-------|
| **单 hash 计算** | Poseidon: ~4-8 us; Pedersen: ~40-80 us | SHA3-256: ~200-400 ns (单次); 优化双子合并: ~300 ns |
| **单叶插入/更新** | ~50-200 us (251 层 path, 但 Edge 压缩减少实际 hash 次数) | ~10-50 us (64 层 nibble path, 叶上浮减少层数) |
| **批量更新 (10K leaves)** | 估计 **500ms - 2s** (Poseidon CPU 开销大, 但只 rehash 变更路径) | 估计 **100-500ms** (SHA3 快, 16-ary 减少树深) |
| **批量更新 (100K leaves)** | 估计 **5-20s** | 估计 **1-5s** |
| **状态根计算 (per block)** | 块内 state diff 驱动, 典型块约 **100-500ms** (sequencer 端); 证明端不受此限 | 典型块约 **50-200ms** (取决于 state changes 数量); 支持并行 2-level 计算 |
| **单 key 读取延迟** | ~1-5ms (需遍历 Patricia 路径 + 存储 trie, 可能 2-3 次 trie lookup) | ~0.5-2ms (单次 JMT 遍历, RocksDB 点查) |
| **证明生成** | 单 key: ~1-10ms (遍历收集 siblings); STARK 全块证明: **数分钟到数十分钟** (GPU 加速后数秒) | 单 key: ~0.5-5ms (遍历收集 siblings) |
| **证明验证** | 单 key Merkle 验证: ~0.5-2ms; STARK 验证 (链上): ~200K-500K gas | 单 key Merkle 验证: ~0.1-0.5ms (SHA3 快) |

> **注意**: 以上估算基于已知的 hash 函数速度和树结构推导, 非直接 benchmark 数据。确切生产数据两个项目均未公开详细 breakdown。

---

## 5. 生产统计 (Production Statistics, 截至 2026-03)

| 指标 | Starknet | Aptos |
|------|----------|-------|
| **链上账户/合约数** | 估计 **~5-10M 账户**, **~50K-100K 合约** (L2, 仍在增长中) | 估计 **~50-200M 账户** (高活跃度 L1) |
| **总交易数** | 估计 **~500M-1B** (从 2023 年主网上线) | **~45.4 亿** (4,538,237,859 ledger versions, 截至 2026-03) |
| **Block height** | 估计 ~1-2M blocks | **658,098,357 blocks** |
| **TPS (实际)** | 日常 **~7-10 TPS** (L2BEAT 2026-03: 7.48 UOPS); 理论峰值远高 | 日常 **~30-160 TPS**; 峰值测试 **10,000+ TPS** |
| **出块时间** | ~30-60 秒 (L2 batch 模式) | ~0.5-1 秒 |
| **状态增长率** | 估计 **~5-15 GB/月** (L2 活跃度较低) | 估计 **~30-100 GB/月** (高吞吐 L1) |
| **TVL** | ~$549M (L2BEAT 2026-03) | ~$1-2B |

---

## 6. 代码引用 (Code References)

### Starknet Patricia + Poseidon

| 组件 | 仓库 & 路径 | 语言 |
|------|------------|------|
| **Patricia Trie 核心** | `starkware-libs/sequencer` → `crates/starknet_patricia/src/patricia_merkle_tree/` | Rust |
| 节点类型 | `node_data/inner_node.rs` (Binary, Edge nodes) | Rust |
| 类型定义 | `types.rs` (NodeIndex, SubTreeHeight=251, BITS=252) | Rust |
| 叶节点 | `node_data/leaf.rs` (SkeletonLeaf) | Rust |
| 更新逻辑 | `updated_skeleton_tree/tree.rs` (create, finalize layers) | Rust |
| 原始骨架 | `original_skeleton_tree/tree.rs` | Rust |
| **Poseidon 实现** | `CryptoExperts/poseidon` (C + x86_64 ASM, Montgomery form) | C/ASM |
| Poseidon 参数 | 同上, `parameters/` 目录 (field p = 2^251+17*2^192+1, state width 3/4/5/9) | - |
| Cairo 内置 | `starkware-libs/cairo` → `core::poseidon` | Cairo/Rust |
| Pedersen 参考 | `starkware-libs/cairo-lang` → `poseidon_hash.py`, `fast_pedersen_hash.py` | Python |

### Aptos Jellyfish Merkle Tree

| 组件 | 仓库 & 路径 | 语言 |
|------|------------|------|
| **JMT 核心** | `aptos-labs/aptos-core` → `storage/jellyfish-merkle/src/lib.rs` | Rust |
| 节点类型 | `storage/jellyfish-merkle/src/node_type/mod.rs` (InternalNode 16-child, LeafNode) | Rust |
| 证明定义 | `types/src/proof/definition.rs` (SparseMerkleProof, siblings vec) | Rust |
| **Hash 核心** | `crates/aptos-crypto/src/hash.rs` (SHA3-256 via tiny_keccak) | Rust |
| 优化 hash | 同上, `hash_sha3_256_two_x32b()` — 单次 Keccak permutation 合并两个 32-byte child | Rust |
| 原始论文 | Diem/Libra JMT Paper (2021-01-14.pdf) | - |

---

## 7. 设计哲学对比

### Starknet: ZK-First Design

- **核心权衡**: 牺牲原生 CPU 性能, 换取 ZK 证明效率
- Poseidon 在 STARK 中仅需 ~300 约束/hash, 而 SHA3 需 ~150,000+ 约束
- 整个系统设计围绕 "证明一切" — 状态转换、存储更新、合约执行都需进入 STARK 证明
- 二叉树 (depth 251) 配合 Edge 压缩, 在 ZK 电路中遍历成本可控
- **代价**: sequencer 端的 state root 计算比传统链慢得多

### Aptos: Raw Throughput Design

- **核心权衡**: 最大化原生 CPU 吞吐, 不考虑 ZK 可证明性
- SHA3-256 是目前最快的标准 hash 之一 (尤其有硬件加速时)
- 16-ary 树减少树深至 64 层, 同时每个 InternalNode 压缩 4 层二叉树为 1 个磁盘节点 → 减少 IOPS
- 叶上浮避免存储空层 → 实际树深远小于 64
- **代价**: 无法直接生成 ZK 证明; 如果要加 ZK, 需额外引入 ZK-friendly hash

---

## 8. 对 N42 的启示

| 考量 | 建议 |
|------|------|
| **如果 N42 不需要 ZK 证明** | Aptos JMT 模型更优: SHA3/Blake3 + 16-ary 树, 最大化 state root 计算速度 |
| **如果 N42 未来需要 ZK** | 考虑 Poseidon2 (比 Poseidon 快 2-3x native, 约束数少 70%) + binary trie |
| **状态根瓶颈** | N42 当前 state root 开销 ~0ms (compact block cache hit), 但未来规模化可能成为瓶颈; JMT 16-ary 结构天然减少 hash 层数 |
| **存储效率** | JMT 的 InternalNode (16-child 压缩) 减少磁盘 IOPS, 对 SSD 友好; Binary trie 需要更多 I/O 但单节点更小 |
| **证明大小** | JMT proof ~0.5-2 KB (16-ary 深度浅); Starknet proof ~1-4 KB (binary 深度深但有 edge 压缩) |
| **手机验证场景** | JMT proof 更小且验证更快 (SHA3 native fast), 适合手机轻节点; 如果要 ZK 轻验证则 Poseidon 更优 |

---

## 9. 数据可信度说明

以下数据来源可靠, 可直接引用:
- 树结构、节点类型、hash 函数: 直接来自源代码 (GitHub)
- Aptos 总交易数 (45.4 亿) 和 block height (6.58 亿): 来自 Aptos mainnet API
- Starknet TVL ($549M) 和日常 TPS (~7.5): 来自 L2BEAT
- Aptos 磁盘需求 (3 TB SSD): 来自官方文档
- Hash 函数参数 (Poseidon field, SHA3-256): 来自官方代码和规范

以下为估算值, 标记 "估计":
- 具体 batch update 时间 (无公开 benchmark)
- 状态数据库纯 trie 大小 (公开数据仅有总磁盘需求)
- Poseidon native CPU 速度 (基于论文和实现推算, 非标准化 benchmark)
- Starknet 账户/合约总数 (无官方公开 API 提供精确数字)
- 状态增长率 (根据活跃度和磁盘需求反推)
