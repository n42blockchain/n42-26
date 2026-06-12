# 任务：twig 引擎 P6 节点接线 + E2E（交接 codex/mac）

Rust twig 引擎核心已完成并对 gov5 字节验证（见 `docs/devlog-64`，P1–P5 + P6 StateDiff 桥接）。
剩余是节点级接线 + E2E —— 需 testnet + node bin（Windows 跑不了/原生不编），交给 mac。

## 已就绪（Windows 已实现并测）

- `n42-twig-core`：`ShardedTwig` 全-DRAM twig 引擎 + `ShardedTwigProof::verify_for_key`（#11 绑定）+
  `snapshot`/`from_snapshot`。14 测试，对 gov5 Go QMDB 三处 root 字节一致。
- `n42-jmt::TwigState`（`src/twig.rs`）：`apply_diff(StateDiff)->(version,root)`、`seed_genesis_account`、
  `prove(key)`、`snapshot`/`from_snapshot`。

## 当前状态（codex/mac）

- Step 1 `PersistentTwig`：已完成并提交。文件 snapshot + WAL、fsync、恢复 replay、snapshot 后
  WAL 截断测试均已覆盖。
- Step 2 mobile/FFI twig proof verification：已完成并提交。`n42-mobile`/C ABI 均带 #11
  key+shard 绑定验证。
- Step 3 节点接线：已完成本地实现。`N42_TWIG=1` 启用 `PersistentTwig`，fresh genesis seed，
  orchestrator commit path 更新 twig root，RPC `n42_twigRoot`/`n42_twigProof` 返回
  bincode `ShardedTwigProof`。
- Step 4 E2E：待执行。需要 fresh data dir/`--clean` testnet 验跨节点 root、一致性、崩溃恢复、
  真实 RPC proof + mobile/FFI 绑定验证。

## 你要做的（按依赖顺序）

### 1. PersistentTwig（文件 snapshot + WAL，崩溃恢复）
**状态：已完成。**

复用 `crates/n42-jmt/src/persistent.rs` 的 PersistentSbmt 机制做一个 `PersistentTwig`：
- 包 `TwigState` + 周期 snapshot（bincode `TwigSnapshot` 落盘，fsync）+ WAL（每块 ops，fsync，
  WAL-ahead）+ open 时 replay。
- `apply_diff` 先 append WAL 再改内存；snapshot 后截断 WAL；replay 连续性校验。
- 参考 PersistentSbmt 的 fsync/snapshot/WAL 既有实现（audit #7 已硬化）。

### 2. mobile/FFI twig proof 验证（带 #11 绑定）
**状态：已完成。**

- `n42-mobile`：`verify_twig_account_proof(proof, root, address)` / `verify_twig_storage_proof(...)`，
  内部派生 key（`n42_bmt_core::account_key`/`storage_key`）+ `ShardedTwigProof::verify_for_key`。
- `n42-mobile-ffi`：`n42_verify_twig_account_proof(proof_data, len, state_root, address_20)` 等
  C ABI（镜像现有 `n42_verify_account_proof`）。
- 注意 `n42-twig-core` 是 zero-dep，可直接进 mobile（无 reth/mdbx）。

### 3. 节点接线
**状态：已完成本地实现，等待 E2E。**

- `orchestrator/mod.rs`：新增 `Arc<Mutex<PersistentTwig>>`（与现有 `jmt: Arc<Mutex<PersistentSbmt>>`
  并列，或用 env/flag 切换 SBMT vs twig）。
- `bin/n42-node/src/main.rs`：fresh genesis 用 `TwigState::seed_genesis_account` seed；
  `N42_TWIG=1` 选 twig 引擎；`N42_TWIG_SNAPSHOT_INTERVAL`。
- RPC：`n42_twigRoot` / `n42_twigProof`（镜像 `n42_jmtRoot`/`n42_jmtProof`，返回 bincode
  `ShardedTwigProof`）。
- **fresh genesis 必须**：twig root 结构与 SBMT 不同，旧 data_dir 不兼容，`--clean` 或新 data_dir。

### 4. E2E（对齐之前 #7/#11/路径压缩的标准）
**状态：pending。**

- **Step 0**：`git -C ../reth log -1 --oneline` 必须 reth main 合并版（alloy-evm 0.36），不降级 deps。
- fresh-genesis 多节点（≥4）出块，root/height/blockhash 一致；tx 推进后仍一致。
- 崩溃恢复：冷启→推进→`kill -9`→重启→version/root 一致（PersistentTwig WAL replay）。
- 真实 RPC proof：`n42_twigProof` 取证明，`n42_verify_twig_account_proof` 正确 address→0、
  错 address→2（#11）；多 shard 覆盖。
- benchmark：内存（目标 ~100 B/entry，对比 SBMT ~400）、吞吐（目标 ≥ C2 Go 7.99M；Rust 无 GC 应更高）。

## 验证标准（对齐 C2）
全 suite + race + commitment 全绿、root 在 twig 方案内字节恒等（同输入同 root，跨节点一致）、
崩溃恢复正确、#11 绑定有效。报告：测试结果、E2E 数据、内存/吞吐数。

## 注意
- 现有 SBMT（PR #1）是稳定回退基线，twig 走独立 crate + env/flag，不破坏 SBMT。
- twig 是 consensus-breaking（root 结构变），定位为新链 fresh-genesis 基线。
