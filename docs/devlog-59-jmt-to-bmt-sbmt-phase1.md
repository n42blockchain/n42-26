# devlog-59 — 决策:状态树 JMT → 自研 SBMT(第一阶段:内存核心 + 对拍)

## 决策

**n42 正式版状态树后端从 16 叉 JMT(`jmt 0.12`)切换到自研 Sparse Binary Merkle Tree(SBMT)。**

- 实现来源:**自研**(参考 AlDBaran `no_std` 库理念,Blake3 原生,完全可控)。
- 兼容策略:**新创世冷启动**(不迁移旧 JMT 状态,正式版从新创世用 SBMT)→ 省去迁移逻辑。
- 哈希:保持 **Blake3**(对齐 EIP-7864 默认 + SP1 zkVM 友好 + AlDBaran SMT)。

### 决策依据(承接 devlog-58 差距分析 + 外部调研)

- **方向已是主线**:以太坊 EIP-7864(统一 binary tree,默认 Blake3)、AlDBaran/Scroll 均走二叉 SMT。
- **ZK 友好**:二叉结构、定长 proof 路径,zkVM 程序分支更少;契合 n42 的 SP1 路线。
- **proof 更干净**:二叉路径,无 16 叉 branch 的兄弟冗余。

### AlDBaran(Pleiades/Hyades)评估结论(为何借鉴理念而非直接用)

- 硬件(论文 benchmark):AWS i7ie-metal-48xl,96 核/192 线程、1,536 GB RAM、16× NVMe RAID0;
  96 核 Pleiades-only **>48M ups**(峰 60M),+Hyades 历史化 **24M ups**(减半)。
- Rust 成熟度:**nightly,minimal-dep(仅 3 依赖)`no_std`,跑 x86_64/aarch64/RISC-V**,点名适合
  RISC-0 zkVM/TEE/kernel —— **理念与 n42 高度契合**。
- **硬伤**:research prototype(2025 论文),**无确认开源仓库**,不能直接集成。
- **判断**:把 Pleiades(异步内存 root)+ Hyades(独立 proof 服务)当**架构蓝本**,
  落地自研。48M ups 靠 96 核/1.5TB 堆出,远超 n42 需求(8s slot),价值在架构不在绝对吞吐。

## 第一阶段实现:`crates/n42-jmt/src/bmt.rs`

路径压缩 Sparse Binary Merkle Tree,内存核心。

### 设计要点

- **key**:256-bit(复用 `keys.rs` 的 `blake3(account/storage)` 32 字节定长 key)。
- **哈希(域分隔)**:empty=`32×0`;leaf=`blake3(0x00||key||value_hash)`;internal=`blake3(0x01||l||r)`。
- **压缩不变量**:任意含 0/1 leaf 的子树用 `Empty`/单 `Leaf` 表示(不留 internal 链)→
  leaf 挂在能唯一区分的最短前缀,深度 ~log₂N,**root 只依赖 key 集合,与插入/删除顺序无关**。
  `insert` 仅在碰撞时分裂;`remove` 把单 leaf 子树重新折叠上提。这是 JMT "0/1 leaf 子树" 规则的二叉版。
- **惰性 hash 缓存**:internal 节点 `Cell<Option<Hash>>`,更新重建路径时失效,未触及子树保留缓存 →
  `root_hash` 只重算触及路径,摊销 O(log N)。
- **proof**:inclusion **和** exclusion(空槽 + 异 key 占位两种排除情形),top-down sibling 列表;
  `verify(root, expected_value_hash)` bottom-up 折回 root,异 key 排除会校验前缀一致性。
- **API**:`insert`/`insert_hashed`/`remove`/`apply_batch`/`get`/`prove`/`root_hash`。

### 验证

- **9/9 单测通过**:`order_independent_root`(确定性)、`delete_restores_root` +
  `delete_matches_fresh_build`(折叠不变量 = 删后与重建一致)、inclusion/exclusion proof 验证、
  `tampered_proof_fails`(篡改 sibling 必败)、空树/单插/原地更新。
- `cargo clippy -p n42-jmt --all-targets -- -D warnings`:干净。

## 对拍:SBMT vs JMT(单棵树,同 key,同机,内存)

`benches/jmt_bench.rs` 新增 `bmt_vs_jmt` 组:相同 N 个 key,batch insert + root。

| leaves | SBMT(µs) | JMT 单树(µs) | SBMT 快 |
|-------:|---------:|-------------:|--------:|
| 100 | 60.9 | 205.7 | 3.38× |
| 1,000 | 766.0 | 4,190.2 | 5.47× |
| 10,000 | 9,248.6 | 68,587.2 | 7.42× |
| 50,000 | 63,564.8 | 475,219.4 | 7.48× |

**结论**:纯树引擎层面,自研 SBMT 比 `jmt 0.12` 单树快 **3.4–7.5×**,规模越大优势越大。
JMT 慢在 jmt 库每次 `put_value_set` 的 NodeBatch 构建/序列化/HashMap 开销;SBMT 是轻量
Box 递归树 + 惰性缓存。

**Caveat**:这是单树对单树。生产 JMT 走 16 分片并行(~10×),SBMT 尚未分片 —— 端到端要等
SBMT 接入 `ShardedJmt` 框架后再测。但引擎层结论清晰。

## 阶段状态与后续

第一阶段完成:✅ SBMT 内存核心 ✅ inclusion/exclusion proof ✅ 9 单测 ✅ 对拍 3.4–7.5×。

后续阶段(JMT→BMT 全面切换):
- **P1**:SBMT 接入 16 分片框架(`ShardedSbmt`,复用 `sharded.rs` 分片/合并逻辑)+ 与 ShardedJmt 端到端对拍。
- **P1**:SBMT 持久化 —— 复用 P0 `PersistentJmt` 路线(全内存 + 后台快照),崩溃恢复补 WAL。
- **P1**:proof 格式定稿 + **手机端(`n42-mobile` / FFI / Dart SDK)同步 SBMT 验证**。
- **P2**:共识/出块 state root 计算切到 SBMT(`n42-node` 编排器),走新创世。
- **P2**:Pleiades 式 **root 计算异步出共识关键路径**(对 8s slot 预算最直接收益)。
- **P3**:value-hash vs value-bytes 在 proof 中的取舍、节点剪枝、disk store。
