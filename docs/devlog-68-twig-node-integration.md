# devlog-68 — Twig P6 持久化、移动验证、节点接线

## 背景

devlog-64 已完成 Rust 全-DRAM twig engine 核心、`TwigState` 的 `StateDiff` 桥接和
`ShardedTwigProof::verify_for_key`。本轮把 P6 从可嵌入状态推进到节点可运行状态：
持久化、mobile/FFI 验证、node/RPC 接线。

## Step 1: PersistentTwig

`n42-jmt::PersistentTwig` 复用 `PersistentSbmt` 的 crash-recovery 合约：
- snapshot：`bincode(TwigSnapshot)` + zstd，临时文件写入、fsync、rename、目录 fsync；
- WAL：每块 `[version u64 LE][len u32 LE][bincode(StateDiff)]`，WAL-ahead + `sync_data()`；
- recovery：先 load snapshot，再 replay 严格连续、版本更新的 WAL 记录；遇到 gap 拒绝恢复；
- `flush()` 同步写 snapshot 后截断 WAL，保证 snapshot/WAL 边界明确。

新增测试覆盖 unsnapshotted WAL replay、snapshot 后 WAL 截断、apply/flush/reopen、
genesis seed snapshot baseline 等恢复路径。

## Step 2: mobile/FFI twig proof verification

`n42-mobile` 直接依赖 zero-dep 的 `n42-twig-core`，不引入 `n42-jmt`/reth/mdbx：
- `verify_twig_state_proof`：只验证 proof 与 combined twig root 的内部一致性；
- `verify_twig_account_proof` / `verify_twig_storage_proof`：复用 `n42_bmt_core::account_key`
  / `storage_key` 派生 key，并调用 `ShardedTwigProof::verify_for_key` 做 #11 key+shard 绑定。

`n42-mobile-ffi` 新增 C ABI：
- `n42_verify_twig_state_proof`
- `n42_verify_twig_account_proof`
- `n42_verify_twig_storage_proof`

错误码沿用 SBMT：`0` valid，`-1` null/zero input，`1` bincode decode failed，`2` verify/key/shard/root
failed。FFI header/iOS header test 同步更新。

## Step 3: node/RPC wiring

Twig 作为独立 sidecar 接入，不破坏现有 SBMT：
- `N42_JMT=1` 仍启用 `PersistentSbmt`，RPC 仍是 `n42_jmtRoot` / `n42_jmtProof`；
- `N42_TWIG=1` 启用 `PersistentTwig`，snapshot 路径 `<data_dir>/twig.snapshot`；
- `N42_TWIG_SNAPSHOT_INTERVAL` 控制 twig snapshot 间隔，默认 1000。

启动 fresh genesis 时，`bin/n42-node` 用 `TwigState::seed_genesis_account` 从 reth chain spec 的
genesis alloc seed version 0，并 flush genesis snapshot。恢复时发布 snapshot/WAL 恢复后的 twig
root/version 到 `SharedConsensusState`。

orchestrator commit path 改为一次性从 `BlockDataBroadcast` 提取 `StateDiff`，再分别投递给启用的
SBMT/Twig 后台任务：
- SBMT 更新原有 `jmt_root` 和 `n42_jmt_latest_root`；
- Twig 更新新增 `twig_root` 和 `n42_twig_latest_root`。

RPC 新增：
- `n42_twigRoot`：返回 `{ version, root }`；
- `n42_twigProof(address, storageSlot?)`：返回 bincode(`ShardedTwigProof`) 的 `proofHex`、
  `shardIndex`、`keyHash`、`value`、`shardRoots`、`root`。

Twig 当前只提供 live-key inclusion proof；缺失 key 的 `n42_twigProof` 返回 JSON-RPC `-32001`。

## 验证

- `cargo test -p n42-twig-core -p n42-jmt`
- `cargo test -p n42-mobile -p n42-mobile-ffi`
- `cargo clippy -p n42-mobile -p n42-mobile-ffi --all-targets -- -D warnings`
- `cargo test -p n42-node test_twig_root_and_proof_roundtrip -- --nocapture`
- `cargo check -p n42-node-bin`
- `cargo clippy -p n42-node -p n42-node-bin --all-targets -- -D warnings`
- `cargo test -p n42-node`

`n42-node` 完整结果：207 unit tests passed；`comm_stress_benchmark` ignored；`stream_v2_pipeline`
6 tests passed；doctests 0。`../reth` 依赖仍有既有普通 warnings，不影响本地 crate clippy。

## 后续

P6 剩余是 E2E：
- fresh-genesis 多节点开启 `N42_TWIG=1` 后 root/height/blockhash 一致；
- 推进交易后 `n42_twigRoot` 跨节点一致；
- kill -9 + restart 后 version/root 一致，证明 `PersistentTwig` WAL replay 正确；
- 真实 `n42_twigProof` 经 mobile/FFI 正确 address 返回 0、错误 address 返回 2；
- 补内存/吞吐 benchmark 数据。
