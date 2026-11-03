# Reth v2.9.0 评估：evm2 替换 revm（暂停，记入待办）

日期：2026-11-03。分支 `upgrade/reth-v2.9.0`（自 main `5847275`）。状态：**暂停**。
共享测试机让给其他开发一段时间，本轮工作回到 Gov5 5M 性能主线；下列待办在测试机空闲后继续。

## 版本

| 项 | 当前基线 | 目标 |
|---|---|---|
| Reth 上游 | v2.8.1 `d763342fa185e4e90ee79dc65f6a5706168b0bcb` | v2.9.0 `c9a780f4f23d6752ca299b104c93b7f099673c80`（2026-10-29，上游最新 release） |
| EVM | revm 43.0.3 + alloy-evm 0.39.0 + reth-revm | evm2 / evm2-inspectors / evm2-jit，git rev `alloy-rs/evm2@f93318f120f6300599b21170d6c7bef81ed63d87`（未发版） |
| reth-core 类 crate | crates.io 0.8.2 | `[patch.crates-io]` 指向 `paradigmxyz/reth-core@275ce4cf5befeb9f1e8acd935ff2882d2d25fc61`（reth-codecs、reth-codecs-derive、reth-primitives-traits、reth-rpc-traits、reth-zstd-compressors） |
| MSRV | 1.95 | 1.96（本仓库 1.97.1 已满足） |

Alloy 2.5.0、alloy-primitives 1.7.3（reth 下限 1.6.1）、alloy-trie 0.9.8、alloy-hardforks 0.4.11 未移动。
上游另把 `zstd` 升到 0.14.3，rocksdb 改为显式 feature 集。

## 设计判断

- v2.9.0 不是常规升级。上游 `9d1638f328 refactor!: replace revm with evm2 (#25002)` 删除了
  `crates/revm`（reth-revm），`reth-evm` 的 `ConfigureEvm`、`BasicBlockExecutor`、`BlockBuilder`
  按 evm2 重写；`BundleState` 移到 `reth_trie_common::bundle_state`，`Database` 来自 `evm2::evm`。
- N42 自身直接使用 revm/alloy-evm/reth-revm 内部类型的文件约 40 个、合计约 3.4 万行
  （n42-parallel-evm、n42-execution、n42-node 的 qmdb_state_root/exec_cache/parallel_payload/
  mobile_packet/eof_guard、n42-consensus-service、n42-tx-types、n42-mobile、n42-evm-bench、n42-node bin）。
  迁移等于重写执行层，并需要串行/并行执行逐字节一致的对照验证。
- 手机侧：`n42-mobile/src/verifier.rs` 在手机上直接运行 revm 与 `reth_revm::cached::CachedReads`。
  换成 evm2 会改变手机 App 的 EVM 实现；若手机包/见证序列化了 revm 类型，还会成为协议变更，需单独评审。
- 混合客户端：n42-rs 仍在 v2.8.1（revm）。迁移后需重做与 n42-rs 的混合 soak。
- 结论：用户先选择“直接升到 v2.9.0”，随后因测试机让出、回到性能主线而暂停；以下为已完成部分和待办。

## 已完成

适配源：`.artifacts/reth-upgrade-290/reth-worktree`，分支 `n42/v2.9.0`（HEAD `f21dc148239a`，**未编译**）。
旧适配源仓库通过 alternates 指向含未提交改动的 `../reth`，在其中 fetch 新 tag 失败，
因此 v2.9.0 使用独立的全新 clone。

六个补丁中前四个已迁移，导出到 `patches/wip-reth-v2.9.0/`（子目录，不会被
`check-reth-source.sh` 的 `patches/reth-*.patch` 匹配，CI 不受影响）：

| 补丁 | 结果 |
|---|---|
| reth-qmdb-state-reader | 干净应用 |
| reth-n42-perf | `crates/evm/evm/src/lib.rs` 冲突：保留上游新增的 `witness` 模块，加入 `payload_cache` 与两个 state-root 开关；丢弃旧基线的 `test_utils` 与 `alloy_evm` 重导出 |
| reth-payload-transactions-root | payload builder 改用 v2.9.0 的 `BlockBuilderOutcome.execution_state` 构造 `BlockExecutionOutput`（旧版用 `db.take_bundle()`）；`rayon` 依赖重新加入 Cargo.toml |
| reth-gov5-cached-state-root | `payload_validator.rs`：保留缓存命中分支，去掉重复的 `ensure_ok!`，BAL 字段按上游改名为 `evm` |
| reth-import-batch | **未迁移**：钩子挂在 revm `State`/`BlockExecutorForEvm`/`executor_for_block` 上，v2.9.0 执行器改为 `execute_block_with_database` + `batch_database`，需按新执行器重新设计 |
| reth-engine-state-provider-factory | 未尝试（单独 `git apply --check` 无冲突） |

## 待办（测试机空闲后）

1. **非 reth 依赖刷新**（低风险，可单独合并）：保持 reth v2.8.1，`cargo update` 到 semver 兼容最新，
   评估可安全的大版本；跑 clippy `-D warnings` 与全量测试。重型构建须在测试机无 claim 时进行
   （cargo 构建会吃掉大页池，影响他人测量）。
2. 重新设计 reth-import-batch 补丁：在 v2.9.0 执行器上提供可选批量导入钩子，返回 `None` 时走串行路径。
3. 迁移 engine-state-provider-factory 补丁，适配源编译通过。
4. n42 自底向上迁移到 evm2：n42-tx-types（0x50 交易 TxEnv 映射）→ n42-parallel-evm（MV memory/调度器的 DB 接口）
   → n42-execution（fast/parallel transfer、read_log、restored_slots、precompile_random、executor factory）
   → n42-mobile → n42-node → n42-consensus-service → bins；workspace 加入同样的 reth-core `[patch.crates-io]`。
5. 手机端评审：确认手机包/见证中的 revm 类型序列化格式，决定是否需要协议版本号。
6. 验证：串行与并行执行逐字节一致的对照、全量测试、E2E 1/3/4/5/8/12、七节点 soak、与 n42-rs 的混合 soak。
7. 更新 `reth-source.lock`、`patches/reth-*.patch`、apply/check 脚本、CI 固定点与 CLAUDE.md。

## 测试机使用记录

- 2026-11-03 本地 01:25–01:28：main 合并验证（clippy + 五个 crate 测试）在 QS run 20 B4 腿的
  decay 阶段运行（测量窗口约 01:46 开始），01:33 clone reth v2.9.0；13:22 `cargo fetch` 在 run 23 B2 的
  funding 阶段。均不在测量窗口内，已在 handoff 告知。之后在测试机有 claim 期间不再做重型构建。
