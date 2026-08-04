# devlog-118: 增量审计(v0.5.x 收尾 4 提交)+ 全依赖升级(reth 20260804)

日期:2026-08-04
分支:`hardening/gov5-cross-port`

## 一、增量深度审计

范围:上次审计基线(`9018701`,interop 审计文档)之后的 4 个代码提交:

| 提交 | 内容 | 审计结论 |
|------|------|----------|
| `454e6ad` | ingest 批头上限 + 端口默认 loopback + admin token 常时比较 | ✅ 正确。上限检查(652 行)确实在 `Vec::with_capacity`(666 行)之前;`secret_eq` 的长度异或转 `u8` 潜在截断(长度差 256 的倍数时为 0)已被末尾 `a.len() == b.len()` 兜住 |
| `4b4dbfe` | 广播上限常量统一 + 超限块测量 | ✅ 正确。`encoded` 正是 block_direct(1480 行)与 gossip(1509 行)两条路径的同一字节串,测量点选在唯一能看到真实线上尺寸的位置 |
| `f5e4aba` | twig 上层树缓冲复用 + twig_count 导出 | ✅ 正确。`clear+resize` 保证 padding 叶子为 NULL_HASH;空 shard 分支正确复位(`up_cap=0`);n=1 边界(up_cap=1、fold 循环为空)成立 |
| `44a01a4` | 接收端 Reject 阈值统一 + 报告线校正 + 空批截断计数 | ✅ 正确。`MAX_BROADCAST_PAYLOAD_BYTES = 8192-256 = 7936 KiB` 高于 devlog-78 实测 p95(7374 KiB),不再误报 |

**发现并修复(1 项,minor)**:`handlers.rs` 的越界拒收测试仍硬编码
`8 * 1024 * 1024 + 1` — 正是 44a01a4 要消除的"第二份拷贝"漂移,漏在了测试里。
已改为 `MAX_GOSSIP_MESSAGE_SIZE + 1`(提交 `59583b9`)。

其他核查过但不改的点:
- sync 截断 while 循环每 pop 一块就对整个 response 重算 `serialized_size`,理论
  O(n²);但触发条件是 response 超 16 MiB,实际只需 pop 少数几块,不值得为它引入
  增量尺寸推导的脆弱性。
- `TwigStateSink::node_count()` 每块加一次 tree 锁,在 apply_diff 刚释放后,无争用。

## 二、reth fork 升级(chore/reth-upstream-20260804)

- 基线:`c533db8ba`(20260719)→ merge `paradigmxyz/main @ 92855d264`(75 提交),
  merge commit `82debcaff`,lockfile 刷新 `23316e3ff`。
- 冲突仅 2 处:`Cargo.lock`(取上游后 `cargo metadata` 重生成)、
  `crates/evm/evm/Cargo.toml` 的 std feature 列表(fork 加 `tracing/std`、上游加
  `fixed-cache/std`,两者都保留)。
- fork 定制(payload_cache、`n42_skip/defer_state_root`、
  `prepare_n42_state_root_job`、`N42DeferredStateRootJob`)与上游同文件改动
  (`payload_validator.rs`、`state_root_strategy/mod.rs`)无行级交叠,合并后
  `reth-evm`/`reth-engine-tree` 冒烟编译通过。
- rust-version 保持 fork 的 1.97(上游 1.95)。
- 上游值得注意的新内容:txpool_prewarm 模块、`fix(engine): defer persistence
  handoff during payload builds`、`feat(engine): build payloads on canonical
  ancestors above finality`、`reth-evm` 新增 `sender_recovery.rs`。

## 三、workspace 依赖升级

版本 pin 对齐新 reth 基线:

| 依赖 | 旧 | 新 |
|------|-----|-----|
| revm | 41.0.0 | 42.0.1 |
| alloy-evm | 0.37.1 | 0.38.0 |
| reth-primitives-traits | 0.5.2 | 0.6.0 |
| Alloy 全家桶 | 2.2.0 | 2.3.0 |
| lru | 0.16 | 0.18 |
| rand | 0.9 | 0.10 |
| toml | 0.9 | 1.1 |

另 `cargo update` 全量刷新 semver 兼容项(ark-* 0.5→0.6、blst 0.3.17、
clap 4.6.5 等)。bincode 刻意留在 1.3:sync/gossip 线格式建立在 bincode 1 上,
bincode 2 是 API + 格式双破坏,无收益。

**适配点(仅 2 处 API 破坏)**:

1. `BlobStore::insert` 参数 `BlobTransactionSidecarVariant` → `PooledBlobSidecar`
   (新增 cell availability 包装)。`blob_port.rs` 用 `.into()` 以
   `BlobCellAvailability::full()` 包装——正确,因为该处 RLP 解码自 leader 广播的
   完整 sidecar,不可能是稀疏 EIP-7594 子集。
2. rand 0.10 trait 重命名:`RngCore`→`Rng`(`fill_bytes`)、`Rng`→`RngExt`
   (`random`/`random_range`)。改 4 个文件的 use 行:twig-core `simd.rs`/
   `flat.rs`、jmt `sbmt_state_bench.rs`、n42-node-bin `keystore.rs`。

CI workflow(e2e/nightly/execution-spec-shards)的 reth ref 已更新为
`chore/reth-upstream-20260804`;CLAUDE.md 基线段落同步更新。

## 四、验证

- `cargo check --all-targets` ✅(唯一警告在 reth fork `fs-util` 上游代码,
  Windows cfg 分支未用 `OpenOptions`,不在 n42 门禁内)
- `cargo clippy --all-targets -- -D warnings` ✅
- `cargo test --workspace` 见任务记录(全绿后合入)

## 五、后续

- reth 分支 `chore/reth-upstream-20260804` 需 push 到 `n42blockchain/reth`,
  否则 CI checkout 会失败——**push n42-26 之前必须先 push reth**。
- E2E 1/3/4 + 5/8/12 由 CI 验证;本地 Windows 可跑 release 构建冒烟。
