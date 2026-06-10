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
- ~~**P1**:SBMT 接入 16 分片框架~~ → **已完成,见下方第二阶段**。
- **P1**:SBMT 持久化 —— 复用 P0 `PersistentJmt` 路线(全内存 + 后台快照),崩溃恢复补 WAL。
- **P1**:proof 格式定稿 + **手机端(`n42-mobile` / FFI / Dart SDK)同步 SBMT 验证**。
- **P2**:共识/出块 state root 计算切到 SBMT(`n42-node` 编排器),走新创世。
- **P2**:Pleiades 式 **root 计算异步出共识关键路径**(对 8s slot 预算最直接收益)。
- **P3**:value-hash vs value-bytes 在 proof 中的取舍、节点剪枝、disk store。

---

## 第二阶段:ShardedSbmt(16 分片并行)+ 端到端对拍

`crates/n42-jmt/src/sharded_bmt.rs` — SBMT 的 16 分片版,对标 `ShardedJmt`。

### 设计

- 16 个独立 `Sbmt` + **per-shard KV map**(QMDB 式「统一 KV + Merkle」拆分):SBMT 存
  `key → blake3(value)` 承诺,KV map 存原始字节供读回(账户 `code_hash` 在余额-only `Modified`
  时需读回保留)。
- 分片:key 首 nibble(`key[0] >> 4`),与 `ShardedJmt` 一致;combined root =
  `blake3(shard_0_root || … || shard_15_root)`,形状与 ShardedJmt 完全相同 → 可直接对拍。
- `apply_diff(StateDiff)`:复用与 ShardedJmt 相同的账户处理(account/storage key、
  `encode_account_value`、code_hash 保留),rayon 16 片并行,每片同时更新 SBMT + KV。
- **5/5 单测**:`deterministic_root`、`distributes_across_shards`、`read_back_value`、
  `preserves_code_hash_on_modify`、`basic_apply`。

### 端到端对拍(ShardedSbmt vs ShardedJmt,同 `make_diff`,同机,内存)

| accounts | ShardedSBMT(µs) | ShardedJMT(µs) | SBMT 快 |
|---------:|----------------:|---------------:|--------:|
| 100 | 138.9 | 203.0 | 1.46× |
| 1,000 | 218.7 | 361.2 | 1.65× |
| 10,000 | 215.2 | 367.7 | 1.71× |
| 50,000 | 215.7 | 359.9 | 1.67× |

**结论与解读**:16 分片端到端 SBMT 比 JMT 快 **1.5–1.7×**,稳定净提升。

注意此处提速(1.5–1.7×)**小于单树对拍的 3.4–7.5×**,原因:
1. 16 分片把每棵树做小,树引擎差异被摊薄;
2. `prepare`(StateDiff 处理、分片、code_hash 读)是两边**共有的固定开销**,分片后占比变大;
3. ShardedSbmt 还多维护一层 per-shard KV map。

**Caveat(已验证为低估)**:上表的 `make_diff` 用 `Address::with_last_byte(i % 256)`,
N≥256 时实际只有 **256 个 distinct 账户**(每片 ~16 个),摊薄了引擎差异。

### distinct-key 验证(`apply_diff_distinct`,真实大 block)

改用 `make_diff_distinct`(N 个互异地址,每片树更深)重测:

| accounts | ShardedSBMT(µs) | ShardedJMT(µs) | SBMT 快 |
|---------:|----------------:|---------------:|--------:|
| 1,000 | 539.8 | 1,012.3 | 1.88× |
| 10,000 | 4,904.7 | 9,874.3 | 2.01× |
| 50,000 | 28,598.3 | 62,204.4 | **2.18×** |

证实碰撞版低估:真实 distinct 账户下端到端提速 **1.9–2.2×,且随规模增大**。三档梯度清晰:
**单树引擎 3.4–7.5× → 16 分片摊薄到 ~2× → 大 block 越明显**。这是换树后共识 state root 计算可
预期的真实净收益(尚不含 proof 简化 + zkVM 友好的额外价值)。

### 阶段状态

第二阶段完成:✅ ShardedSbmt 16 分片 ✅ 统一 KV+Merkle 读回 ✅ code_hash 保留 ✅ 5 单测
✅ 端到端对拍 1.5–1.7×。`cargo test -p n42-jmt` 共 **82 passed**,clippy 干净。

后续:distinct-key 大 diff 对拍 → SBMT 持久化(P0 PersistentJmt 路线)→ proof 打包 +
手机端验证 → 共识 state root 切换(新创世)。

---

## 第三阶段:SBMT 持久化(PersistentSbmt)

让换树方案具备上生产基础:全内存运行 + 快照持久化 + 崩溃恢复。

### 实现

- `ShardedSbmt::snapshot()` / `from_snapshot()`(`sharded_bmt.rs`):**复用通用 `JmtSnapshot`
  结构**(纯 KV dump),所以 SBMT 与 JMT 快照在磁盘上**格式互换**。from_snapshot 按分片重建
  并**校验 combined root** 一致。
- `PersistentSbmt`(`persistent.rs`):`PersistentJmt` 的 SBMT 对应物,同一模型 ——
  `apply_diff` 内存级(零节点 IO),每 `snapshot_interval` 版本触发**后台线程快照**
  (序列化+zstd+落盘不阻塞 apply),`open` 启动从快照恢复,`flush`/`flush_background`/
  `join_pending` 生命周期,`Drop` 时 best-effort join。复用现成 `save_snapshot`/`load_snapshot`。
- **同一 P0 局限**:崩溃恢复粒度 = 快照间隔,生产需补 StateDiff WAL(后续)。

### 验证

- 新增 **4 个测试**:`snapshot_roundtrip`、`from_snapshot_rejects_root_mismatch`(ShardedSbmt);
  `sbmt_apply_flush_reopen`、`sbmt_auto_snapshot_and_background_flush`(PersistentSbmt)。
- `cargo test -p n42-jmt` 共 **86 passed**,clippy `-D warnings` 干净。

### 阶段状态

第三阶段完成:✅ ShardedSbmt 快照/恢复(格式与 JMT 互换)✅ PersistentSbmt 全内存+后台快照
✅ 4 测试。至此 SBMT 自身已具备:引擎 + 16 分片并行 + 统一 KV+Merkle + inclusion/exclusion
proof + 持久化恢复。

后续(SBMT 外部集成,需方向确认):proof 打包 + 大小对比 → 手机端(`n42-mobile`)验证 →
共识 state root 切换(`n42-node`,新创世)→ Pleiades 式 root 异步出关键路径 → WAL 闭合崩溃恢复。

---

## 第四阶段:proof 打包 + 大小对比

`ShardedBmtProof`(`sharded_bmt.rs`)— SBMT 端到端 proof,对标 `JmtProof`:16 shard roots
(重算 combined root)+ in-shard `BmtProof` + 原始 value,**自洽验证**(仅需 block header 的
combined root,无需树访问,手机可调)。verify 校验:shard 数/索引 → combined root → value 与
value_hash 一致 → in-shard 二叉路径折回 shard root。

### proof 大小对比(5000 个 distinct 账户,同 key)

| | 总大小 | 认证路径(siblings) | 深度 |
|---|------:|-------------------:|-----:|
| **SBMT** | 1,135 B | **480 B** | 15 |
| JMT | 1,326 B | 709 B | — |

- 总大小小 **14%**;**认证路径小 32%**(480 vs 709)—— 这是真正的差异(两者 16 shard roots
  512B + value 相同)。验证决策依据「二叉 proof 比 16 叉 SparseMerkleProof 更干净」。
- **剩余优化空间**:两者都含 512B 的 16 shard-roots 冗余。对 shard roots 再建一棵小 merkle,
  proof 只带 log₂16=4 个兄弟(128B),可再省 384B —— 这是差距分析里的 proof 去冗余点,留作后续。

### 验证

- 新增 **2 测试**:`sharded_proof_inclusion_exclusion_tamper`、`proof_size_vs_jmt`。
- `cargo test -p n42-jmt`:全绿,clippy `-D warnings` 干净。

### 阶段状态

第四阶段完成:✅ ShardedBmtProof 打包 + 自洽 verify ✅ inclusion/exclusion/篡改测试
✅ proof 大小实测(认证路径比 JMT 小 32%)。

SBMT 现已具备生产所需全部 crate 内能力:引擎 + 16 分片 + 统一 KV+Merkle + proof(打包+验证+
更小)+ 持久化恢复。后续为外部集成(需方向确认):手机端 SBMT proof 验证(`n42-mobile`)→
共识 state root 切换(`n42-node`,新创世)→ proof shard-root 去冗余 → WAL。

---

## 第五阶段:proof shard-root 去冗余

把 combined root 从「16 shard root 拼接哈希」改成**深度-4 二叉 merkle 树**,proof 只带目标
shard root + 4 个兄弟(160B),不再带全部 16 roots(512B)。SBMT 走新创世,改 combined-root
算法无兼容负担。

### 实现(`sharded_bmt.rs`)

- `shard_tree_root` / `shard_tree_path` / `shard_tree_root_from_path`:16→8→4→2→1 的二叉 merkle。
- `ShardedSbmt::root_hash` 与 `apply_diff` 改用 `shard_tree_root`。
- `ShardedBmtProof` 字段:`shard_roots: Vec<Hash>`(512B)→ `shard_root: Hash` + `shard_path: Vec<Hash>`(4 个,128B)。
- verify 第一步改为从 `shard_root + path` 重算 combined root。

### proof 大小(5000 distinct 账户,同 key)

| | 总大小 | 变化 |
|---|------:|------|
| SBMT(去冗余前) | 1,135 B | — |
| **SBMT(去冗余后)** | **787 B** | **−348 B / −31%** |
| JMT | 1,326 B | — |

去冗余后 **SBMT proof 比 JMT 小 41%**(787 vs 1326)。对手机带宽敏感场景直接收益。

### 验证

- `cargo test -p n42-jmt`:全绿(tamper 测试改打 `shard_root`);clippy `-D warnings` 干净。

### 阶段状态

①proof shard-root 去冗余 ✅ 完成。下一步按既定顺序:②手机端 SBMT 验证(`n42-mobile`)→
③共识 state root 切换(`n42-node`,新创世)。

---

## 第六阶段:抽零依赖 core crate + 手机端 SBMT 验证(②)

让手机/FFI 能用纯 blake3 验证 SBMT proof,而不拖入 n42-jmt 的 reth/mdbx 重依赖。

### 新建 `n42-bmt-core`(零 reth/mdbx 依赖)

依赖仅 `blake3 + serde + thiserror`。承载**纯验证 + 引擎**:
- SBMT 引擎(`Sbmt`:insert/remove/apply_batch/get/prove/root_hash)
- `BmtProof`(in-shard inclusion/exclusion proof + verify)
- 深度-4 shard-root merkle(`shard_tree_root`/`shard_tree_path`/`shard_tree_root_from_path`)
- `ShardedBmtProof` + `verify`(接 `&[u8;32]` combined root)+ `BmtVerifyError`
- proof 类型 derive serde(网络传输 / 反序列化)

### n42-jmt 复用 core

- `bmt.rs` 从「本地实现」改为 **re-export `n42_bmt_core::*`**,保留 `n42_jmt::bmt::*` 路径。
- `sharded_bmt.rs` 删本地 shard-tree + `ShardedBmtProof`,改用 core;`ShardedSbmt`(分片 builder +
  KV + apply_diff + snapshot + prove)保留。引擎单一来源,无重复。

### n42-mobile 集成(②)

- 依赖 `n42-bmt-core`,新增 `state_proof::verify_state_proof(proof, state_root)`。
- 手机**纯 blake3 验证单账户** against block state root,**无需重执行 block、无需树访问**
  (此前手机只有 EVM 重执行一条验证路径,现新增轻验证路径)。
- 3 测试:正确 root 通过、错误 root 拒绝、篡改 value 拒绝。

### 验证

- `n42-bmt-core` 5 测试、`n42-jmt` 79 测试、`n42-mobile` state_proof 3 测试,全绿;clippy 干净。

### 阶段状态

②手机端 SBMT 验证 ✅ 完成(经由零依赖 `n42-bmt-core`)。剩 ③共识 state root 切换
(`n42-node`,新创世)— 改动面最大、碰共识关键路径,下一步进行。FFI/Dart SDK 暴露 `verify_state_proof`
作为 ② 的收尾小项。

---

## 第七阶段:n42-node 状态树切换 JMT→SBMT(③,直接替换)

### 关键发现:JMT/SBMT 与共识解耦

调研 n42-node 后确认:`ShardedJmt` **不是共识 state root**。真正的 state root 由 reth EVM 执行
算出、写入 block header、经 Engine API `new_payload` 校验。JMT/SBMT 是**异步后台维护的并行状态树**,
root 不进 header、不参与 BFT,唯一用途是 **RPC `jmtProof` 给手机钱包**。所以本次切换**不碰共识关键路径**。

### 改动(类型/方法适配,业务逻辑不变)

- `orchestrator/mod.rs`、`rpc.rs`、`bin/n42-node/main.rs`:字段/`with_jmt`/实例化
  `ShardedJmt`→`ShardedSbmt`(`N42_JMT=1` 启用,内存版,重启重建,与原 JMT 行为一致)。
- `consensus_loop.rs`:`apply_diff` 从 `match ... Ok/Err` 改为直接 `let (version, root) = tree.apply_diff(&diff)`
  (SBMT 无 fallible);**删除 `prune`**(SBMT 只保留最新态,无版本堆积,无需剪枝);去掉不再用的 `block_count`。
- `rpc.rs` `jmt_proof`:`build_proof`→`tree.prove(key.0)`,返回 **bincode 编码的 `ShardedBmtProof`**
  放入 `proof_hex`;手机用 `n42-bmt-core` 的 `verify_state_proof`(纯 blake3)验证。`root_hash` 改为
  infallible。

### 验证

- **n42-node + bin 编译通过**(本机 Windows,`cargo check`,1m59s)。除 pre-existing 的
  `OpenOptions` Windows-only cfg warning 外零警告。
- **E2E 待 CI/WSL2**:测试网(出块 + RPC proof 端到端)本机 Windows 跑不了,需 CI 验证
  `n42_jmtProof` 返回的 SBMT proof 能被手机 `verify_state_proof` 通过。

### 阶段状态

③ n42-node 切换 ✅ 完成(编译通过,E2E 待 CI)。**至此 JMT→SBMT 全链路打通**:
引擎(bmt) → 16 分片(ShardedSbmt) → 持久化(PersistentSbmt) → proof(ShardedBmtProof) →
去冗余(depth-4 shard merkle) → 零依赖 core(n42-bmt-core) → 手机验证(n42-mobile) →
共识节点(n42-node)。`ShardedJmt` 保留在 n42-jmt crate 仅作对拍基线。

后续:CI E2E 验证 → FFI/Dart SDK 暴露 verify_state_proof → 旧 JMT 代码清理(确认 SBMT 稳定后)。

---

## 第八阶段:FFI 暴露 SBMT proof 验证（手机真机接入）

补齐手机验证最后一公里：手机 app 能用纯 blake3 验证 SBMT 账户 proof，不重执行、不联网。

### 实现（`n42-mobile-ffi`）

- C FFI `n42_verify_state_proof(proof_data, proof_len, state_root)`：**无状态**（不需要
  `VerifierContext`），接 `bincode(ShardedBmtProof)`（= `n42_jmtProof` RPC 的 `proofHex` hex-decode）
  + 32 字节 combined root（= `n42_jmtRoot`）。返回 0 有效 / 1 解码失败 / 2 验证失败 / -1 参数错。
- **iOS**：`include/n42_mobile.h` 加 C 声明（Swift 直接调）。
- **Android**：`android.rs` 加 JNI wrapper `nativeVerifyStateProof(proof, stateRoot)`。
- 验证链路：手机调 `n42_jmtRoot` + `n42_jmtProof(addr)` → hex-decode proofHex → `n42_verify_state_proof`
  → 0 即账户状态被 block SBMT root 承诺，无需信任节点、无需重执行。

### 验证

- 新增 FFI 往返测试（valid=0 / wrong-root=2 / garbage=1 / null=-1）+ iOS header 断言含新函数。
- `cargo test -p n42-mobile-ffi` **44 passed**，clippy `-D warnings` 干净。

### 阶段状态

FFI 接入 ✅。SBMT 现已端到端可用：节点算 root + 出 proof（RPC）→ 手机 C/JNI/Swift 纯 blake3 验证。
剩余：PersistentSbmt 的 WAL（崩溃恢复闭合）+ reth-main 下实际 E2E（需 mac/CI）。

---

## 第九阶段:PersistentSbmt WAL（崩溃恢复闭合）

闭合前几阶段反复标注的 P0 局限：崩溃恢复粒度 == 快照间隔。

### 实现（`persistent.rs`）

- **WAL**（`<snapshot>.wal`，append-only）：每条 `[version u64 LE][len u32 LE][bincode(StateDiff)]`。
- `apply_diff`：内存 apply → **append WAL（per-block durable）** → 到 `snapshot_interval` 触发**同步**
  `flush`（快照 durable 后才截断 WAL，保证顺序安全）。
- `open`：load snapshot(V0) → **replay WAL 中 version > V0 的记录** → 打开 WAL append handle。
  崩溃时写一半的尾记录（截断/损坏）被丢弃。
- `flush`：snapshot + save **成功后** `truncate_wal`（快照已覆盖到该 version，WAL 可清）。
- `flush_background` 保留但**不截断 WAL**（快照未 durable），WAL checkpoint 只走同步 `flush`。

### 效果

崩溃恢复 = 快照 + WAL replay → **已提交的块一个都不丢**，即使从未快照。

### 验证

- 新增 2 测试：`sbmt_wal_recovers_unsnapshotted_blocks`（interval=MAX 不快照，纯 WAL 恢复 3 个块）、
  `sbmt_wal_truncated_after_snapshot`（快照后 WAL 文件长度=0）。
- `cargo test -p n42-jmt` **81 passed**，clippy `-D warnings` 干净。

### 阶段状态

WAL ✅。**至此 SBMT 新树完整实现闭环**：引擎 → 16 分片 → 统一 KV+Merkle → proof（打包+去冗余，比 JMT
小 41%）→ 持久化（快照 + WAL 崩溃恢复）→ 零依赖 core → 手机验证（core + FFI C/JNI/Swift）→
节点切换（ShardedSbmt + genesis seeding + leader drain）→ reth main 端到端 E2E 通过（mac）。

注：生产可选增强 — WAL 用 `fsync`（当前 `flush` 到 OS buffer，进程崩溃安全、OS 崩溃可能丢 buffer）。

---

## 第十阶段:节点接入 PersistentSbmt(修复审计 #2,重启不丢状态)

深度审计(code-review)发现关键 gap:节点用**内存 `ShardedSbmt`**,不持久化,每次启动**无条件 seed
genesis** —— 重启后 SBMT=genesis-only(v0)而链在 block N,RPC proof 对 stale root。即前九阶段做的
`PersistentSbmt`/WAL **根本没接进节点**。本阶段接上。

### 改动

- `PersistentSbmt::inner_mut()`:暴露可变 inner,仅供 genesis seed(v0,绕过 WAL,genesis 可重导出)。
- `bin/main.rs`:`ShardedSbmt::new()` → `PersistentSbmt::open(<data_dir>/sbmt.snapshot, interval)`
  (`N42_SBMT_SNAPSHOT_INTERVAL`,默认 1000)。**genesis 仅当 `version()==0`**(无快照恢复)才 seed;
  否则发布从 snapshot+WAL 恢复的 root。
- `orchestrator/mod.rs` + `rpc.rs`:字段/`with_jmt` 类型 `ShardedSbmt`→`PersistentSbmt`;RPC proof 走
  `tree.inner().prove()`。
- `consensus_loop.rs`:`apply_diff` 现返回 `Result`(WAL/快照 IO 可能失败)→ `match`,在 spawn_blocking
  后台执行(WAL append per-block + 每 interval 快照,不阻塞共识)。

### 效果

节点重启:`open` 从 `sbmt.snapshot` + WAL replay 恢复到崩溃前 version,genesis 不重 seed,root 与链一致。
SBMT 真正具备生产持久性。

### 验证

- `cargo check -p n42-node-bin` 通过;`cargo test`:n42-jmt 81 + n42-node 206 全过;clippy 干净
  (唯一 warning 是 reth fork 的 `reth-fs-util`,pre-existing)。
- **E2E 待 mac**:重启后 `n42_jmtVersion`/`jmtRoot` 应恢复到重启前值(本机 Windows 跑不了测试网)。

### 阶段状态

审计 #2 ✅ 修复。SBMT 持久化端到端贯通:引擎→分片→proof→`PersistentSbmt`(快照+WAL)→
**节点集成(重启恢复)**→手机验证(FFI)。剩余审计项:#5 leader drain finalization(共识,需 mac E2E)、
#7 fsync(可选)、#10 cleanup 去重(重构)。

---

## 第十一阶段:WAL/快照 fsync(审计 #7,Windows 端)

审计 #7:`append_wal` 只 `flush()` 到 OS buffer、`save_snapshot` 只 atomic rename 不 fsync,文档
"durable per block / no committed block lost on crash" 对 OS/电源崩溃过强。本阶段补 fsync(Windows
端代码 + 单测;重启崩溃 E2E 由 mac/codex 负责)。

### 改动

- `persistent.rs append_wal`:去掉对 `File` 无效的 `flush()`,改 `self.wal.sync_data()`(fsync 数据)。
  每块 fsync;SBMT apply 在 spawn_blocking 后台(非共识关键路径),成本可接受。
- `snapshot.rs save_snapshot`:`std::fs::write` → `File::create + write_all + sync_all`(fsync temp),
  rename 后 `#[cfg(unix)]` fsync 父目录使 rename 本身 durable(Windows 无 dir-fsync,rename 已 crash-safe)。

### 效果

WAL 记录与快照现对 **OS/电源崩溃** durable,不只是进程崩溃。`PersistentJmt` 的快照也受益(共享 `save_snapshot`)。

### 验证

- `cargo test -p n42-jmt` **81 passed**,clippy 干净。
- **E2E 待 mac(codex)**:跑块 → kill → 重启 → 确认恢复;以及拔电/OS 崩溃场景。
