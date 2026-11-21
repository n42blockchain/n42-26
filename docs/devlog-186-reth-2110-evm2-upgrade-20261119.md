# Reth 2.11.0 升级（revm → evm2）

日期：2026-11-19 起。分支 `upgrade/reth-v2.11.0`（自 `feat/native-port` `1b51564`）。状态：**进行中**。
工作区 `.artifacts/reth-upgrade-2110/`：`reth-worktree`（上游 v2.11.0 `b9abbcc1b982755baa9021e6874e2e58936ca65f`
上的 `n42/v2.11.0` 分支）、`reth` 链接、`n42-26`（本分支）。构建产物放
`/data/n42-build/n42-26-reth211/target`；重型构建经 `gated.sh` 取 box claim 后运行。

## 要求（用户）

1. 参考 `../n42-rs`（主线已在 v2.11.0），依赖库 reth 升到 2.11.0。
2. 尽量复用，减少自己做的内容。
3. 手机 SDK（`n42-mobile`、`n42-mobile-ffi`）一并升级到 evm2。

## 版本

| 项 | 升级前 | 升级后 |
|---|---|---|
| Reth 上游 | v2.8.1 `d763342f` | v2.11.0 `b9abbcc1` |
| EVM | revm 43.0.3 + alloy-evm 0.39.0 + reth-revm | evm2 / evm2-inspectors / evm2-jit，git `alloy-rs/evm2@66687c18b34d3bdbb899f5eb7605070615c03475` |
| reth-core 类 crate | crates.io 0.8.2 | `[patch.crates-io]` → `paradigmxyz/reth-core@275ce4cf5befeb9f1e8acd935ff2882d2d25fc61` |
| alloy-trie / secp256k1 / enr / discv5 | 0.9.8 / 0.30 / 0.13 / git | 0.9.9 / 0.33 / 0.14 / 0.12 |

reth-core 仍钉 evm2 旧 rev，图里会出现两份 evm2；与上游、n42-rs 相同，用
`[patch."https://github.com/alloy-rs/evm2"]` 指向 `https://github.com/DaniPopes/evm2` 同一 rev 解决。

## 调研结论（三份只读调研，2026-11-19）

- reth 2.9 起没有 revm 的 `State`：块级缓存与 bundle 在 reth 执行器内部，`merge_transitions`/`take_bundle`
  不存在，变更以 evm2 的 `PendingState`/`AccountChanges` 流给出。驱动 `State` 的代码（graft、批量导入、
  `TrackingExecutor`）不能改名了事，n42-rs 也是重写而非翻译（其 `docs/RETH_2_9_0_UPGRADE.md`，1,755 行，
  含接口对照表与 F3–F6 记录）。
- 其余多为路径改名：bundle 类型 → `reth_trie_common::bundle_state`；`AccountInfo`/`Bytecode`/`CacheDB`/`SpecId`
  → evm2；`Database` 变为 `get_account/get_code_by_hash/get_storage/get_block_hash`（`&mut self`、引用参数）。
- n42-rs 2.9→2.11 几乎不需改 N42 代码（2.10 一处回调签名，2.11 `receipts` 字段私有 + secp256k1 0.33）。

### 复用分类

| 类别 | 内容 |
|---|---|
| 直接拿 | n42-rs `crates/n42/tx-types/src/evm.rs`（0x50 → `evm2::ethereum::TxEnvelope::Eip1559`）；twig-core、bmt-core 不依赖 EVM |
| 小改后拿 | `engine-types/src/parallel_common.rs` |
| 采用其设计与代码 | `n42_evm.rs`、`fast_handler.rs`、`direct_transfer.rs`、`batch_state.rs`、`graft_fold.rs`、`parallel_build.rs`（区块头换回 alloy `Header`，组装器换回 `EthBlockAssembler`，去掉 HotStuff/Glamsterdam 特有部分） |
| n42-rs 没有，自行迁移 | 随机数预编译 0x0302、restored-slots / SSTORE 脏槽、`ReadLogDatabase`、witness、state_diff、EOF 守卫、`n42-parallel-evm` |

待核对：(1) 脏槽规则——本仓库以 chain 94 块 13,561,251 为反例认为“写后改回”的槽也算脏，n42-rs
`qmdb-reth/src/changes.rs` 的注释相反；以本仓库的反例测试为准。(2) `partition_by_sender` 在收款人也是
发送人时本仓库合并分组，n42-rs 不合并而靠 `merge_batches` 拒绝。(3) revert 追加的存储合并语义。

### 协议面

- 手机 V2 流式验证按 EVM 读库顺序回放无键值流；节点打包与手机验证同步换 evm2 则自洽，新旧混用会失败
  → 流式包需要版本号。手机包、见证、状态差异本身不含 revm 类型。
- `CompactBlockExecution.bundle_state`（leader→follower 紧凑块，MessagePack/JSON）直接序列化
  `revm::database::states::BundleState`；须核对 `reth_trie_common::bundle_state::BundleState` 的 serde 是否
  逐字节一致，否则升版本。

## reth 补丁（v2.11.0 上的 `n42/v2.11.0`）

| 补丁 | v2.11.0 |
|---|---|
| reth-qmdb-state-reader | 干净 cherry-pick |
| reth-n42-perf | `evm/src/lib.rs` 冲突：保留上游 `witness`，加入 `payload_cache` 与两个开关 |
| reth-payload-transactions-root | 复用 2.9.0 评估时的解法（`execution_state`），干净应用 |
| reth-gov5-cached-state-root | 复用 2.9.0 评估时的解法，干净应用 |
| reth-engine-state-provider-factory | 干净 |
| reth-cached-execution-refusal | 干净 |
| reth-native-import-path | 保留 `uses_receipt_root`、`N42_CANON_NOTIFY_LATE`、payload 交易根复用；去掉 `open_parent`（随批量导入钩子） |
| reth-executed-block-inbox | 干净 |
| reth-import-batch、reth-import-batch-tail | **不迁移**：钩子挂在已删除的执行器上。改用 n42-rs 的做法——follower 在引擎外执行并校验，再经已执行块收件箱交给引擎 |

以上尚未编译验证。

## 阶段

1. **能编译、结果正确（性能路径关闭）**：workspace 依赖；tx-types；`N42EvmConfig`（取自 n42-rs `n42_evm.rs`）
   + 随机数预编译；脏槽跟踪；读日志库；witness/state_diff；`native_payload` 重新从上游 v2.11.0
   `default_ethereum_payload` 移植；exec_cache、qmdb_*、eof_guard、mobile_packet；`n42-mobile` 验证器；
   consensus-service 紧凑块；各 bin。`n42-parallel-evm` 的去留待定（仅 `execute_block_parallel` 开关与
   evm-bench 使用，n42-rs 无对应物）。
2. **性能路径**：搬 n42-rs 的 fast handler、direct transfer、batch_state、graft_fold、parallel_build、
   parallel_common；follower 导入走收件箱。
3. **验证与收尾**：串/并行逐字节对照、全量测试、E2E 1/3/4/5/8/12、七节点舰队、与 n42-rs/gov5 混合轮；
   `reth-source.lock`、补丁文件、apply/check 脚本、CI 固定点、CLAUDE.md、README（仍写六个补丁）、
   CLAUDE.md 中“n42-mobile 零 reth 依赖”的过时描述。

## 进度

- [x] 工作区与 v2.11.0 源；八个补丁迁移（未编译）
- [ ] 阶段 1
- [ ] 阶段 2
- [ ] 阶段 3

## 进度记录（2026-11-19，阶段 1 进行中）

### reth 侧（`n42/v2.11.0`，8 个补丁，未推送）

- `cargo check --tests`（reth-evm / engine-tree / ethereum-payload-builder / node-builder / provider /
  storage-api / consensus）通过：`logs/reth-check2.log`，rc=0。
- 只改了三行源码：`ethereum/payload/src/lib.rs` 的 `transactions_root` 取字段；`payload_validator.rs`
  去掉一处无用 `into()`；`state_root_strategy/mod.rs` 的 `state_provider_factory` 改 `const fn`。
- clippy：N42 代码无警告；`-D warnings` 整体不过是上游文件自身的警告（`rpc/src/debug.rs:1604`、
  `config/src/config.rs:559`）。未跑任何测试，`N42_CANON_NOTIFY_LATE` 与 executed-block inbox 未做运行时验证。
- 随批量导入钩子一起消失：`open_parent` 参数、`n42_import_stage_ms` 的 `collect` / `recovered_copy` /
  `merge_transitions` / `take_bundle` 四个阶段（读这些阶段的看板将无数据）。

### `n42-tx-types` / `n42-execution`

- check / test / clippy `-D warnings` 全过：`logs/exec-final.log`，rc=0；n42-execution 110 通过，
  n42-tx-types 35 通过、4 个原有 `#[ignore]`。
- `execution.rs` 直接采用 n42-rs 的 `evm.rs`（0x50 交易 = EIP-1559 的 `From` 实现）。
- `N42EvmConfig` 内包上游 `EthEvmConfig<ChainSpec, N42EvmFactory>`，环境与上下文计算委托上游；
  `N42EvmFactory` 委托 `RethEvmFactory`，加 0x0302 随机数预编译，`n42_tx_registry` 为快速转账处理器的接入口。
- 写后恢复槽位：每笔交易的变化取自 evm2 `TxStateChanges::account_changes()`；同一交易内写后恢复
  仍靠替换 SSTORE 指令观察（基于 evm2 自带 `sstore_with`，gas 不变）。**注意：evm2 JIT 不走该钩子**，
  本仓库未启用 jit 特性，若启用需另行处理。
- 旧性能路径 `fast_transfer` / `parallel_transfer` / `parallel_block` 当时移至
  `crates/n42-execution/legacy-revm/`，不参与编译；阶段 2 已用 n42-rs 对应实现替换，该目录随后删除。
- `n42-parallel-evm`、`bin/n42-evm-bench` 移出 workspace 搁置（revm 专用，n42-rs 无对应实现）。

### 行为差异（需在阶段 3 验证）

1. evm2 的 bundle 不含"只被加载未改动"的账户；遍历 `bundle.state` 的消费者需逐一核对。
2. 帧回滚的 SSTORE 仍记为恢复槽位（沿用旧规则，已用测试固定）。
3. 读日志流格式不变，但 evm2 在交易内的读顺序不能假定与 revm 相同：节点与手机必须同时切换，
   流数据包需带版本。
4. `AccountInfo` 无 `account_id`；`CompactBlockExecution` 的线上编码因此与旧版不保证兼容，
   新旧节点不可混跑。
5. gas 与状态清理未见差异，但尚未在真实区块上与 revm 对拍。

### 手机 SDK 与共识服务

- `n42-mobile/src/verifier.rs`：`StreamReplayDB` 改为实现 `evm2::evm::Database`，V1 的 `CachedReads`
  取自 `reth_evm::cached`；`n42-mobile-ffi` 无需改代码。
- `n42-consensus-service`：`CompactBlockExecution.bundle_state` 改为 `evm2::block::bundle_state::BundleState`。
- 编译验证进行中（`logs/mobile-check1.log`）。

### 待办

- `n42-node`、`bin/n42-node` 迁移（进行中）；阶段 2 性能路径；阶段 3 验证与
  `reth-source.lock` / `patches/` / 脚本 / CI / `CLAUDE.md` / `README.md` 更新。

## 进度记录（2026-11-19 续）：手机 SDK、共识服务、节点串行路径

### 结果

| 范围 | check | test | clippy `-D warnings` | 日志 |
|---|---|---|---|---|
| n42-mobile / n42-mobile-ffi | 通过 | 109 / 47 通过 | 干净 | `logs/mobile-test1.log` |
| n42-consensus-service | 通过 | 仅跑了 `compact_execution_codec`（1 通过 1 忽略） | 干净 | 同上 |
| n42-node / n42-node-bin | 通过 | lib 289 通过 11 忽略；`stream_v2_pipeline` 12、`restored_slots` 5 通过 | 干净 | `logs/node-final.log` |

`stream_v2_pipeline` 覆盖节点侧读日志采集 + 手机侧回放（含三个对抗用例），两端都在 evm2 上。
尚未跑 E2E、真实链数据回放和机群。

### 阶段 1 暂时退回串行的路径（开关仍可解析，启动时告警一次）

| 路径 | 开关 | 旧代码当时的暂存位置（阶段 2 后已删除） | 阶段 2 来源 |
|---|---|---|---|
| 并行转账出块 | `N42_PARALLEL_BUILD=1` | 原 `crates/n42-node/legacy-revm/parallel_payload.rs` | n42-rs `parallel_build` / `batch_state` / `graft_fold` / `parallel_common` |
| 转账快速路径 | `N42EvmFactory::with_fast_transfers` | 原 `crates/n42-execution/legacy-revm/fast_transfer.rs` | n42-rs `fast_handler` / `direct_transfer` |
| 跟随者并行导入批 | `N42_PARALLEL_IMPORT=1` | 原 `crates/n42-execution/legacy-revm/parallel_block.rs` | n42-rs `follower_import.rs` 思路 |

并行出块器独有、阶段 2 前缺失的行为：构建器内的 `N42_MAX_TXS_PER_BLOCK` / `N42_BUILD_TIME_BUDGET_MS`
限制、`N42_GRAFT_NO_CACHE` / `N42_GRAFT_STREAM` / `N42_TX_ROOT_EARLY`、H2-v4 收尾时把原生收据根写入区块头、
构建器直接给出的 QMDB 根（现由 normalization 自行推导）。因此阶段 1 结束时 TPS 低于升级前。

### 共识相关差异：被触碰但未变化的账户

revm 会把被触碰但未变化的账户放进 bundle（状态 `Changed`），`gov5_qmdb_operations` 据此为其写叶子；
evm2 的 bundle 不含这些账户。本仓库没有"未变账户不写叶子"的规则，所以不处理的话，
零值转账收款方、零小费 beneficiary、空操作内部调用这类区块的 QMDB 根会与升级前不同。
n42-rs 接受了这一差异（其链有对应规则），n42-26 不能照搬。
处理：在 `TrackingExecutor` 的每块登记里增加被触碰账户列表，喂给 QMDB 操作推导与 `eof_guard`（进行中）。

### 其他

- 引擎导入端到端夹具依赖 `EngineApiTreeState::new` 为 `pub`（原由 import-batch 补丁顺带公开），
  在 reth 侧补一个单独的小提交后恢复（进行中）。
- `n42-node` 的 dev 依赖 `secp256k1` 0.30 → 0.33，`bin/n42-node` 增加 `reth-trie-common`。

## 进度记录（2026-11-19 续二）：阶段 1 收尾与阶段 2a

### 阶段 1 的门

- 全 workspace `cargo check --all-targets`、`cargo clippy --all-targets -- -D warnings`：`logs/c3-ws-check-clippy.log`，rc=0。
- `cargo test --workspace --no-fail-fast`：1851 通过、0 失败、34 忽略（60 个测试二进制）：`logs/c4-ws-test.log`。
- E2E（阶段 1 的 release 二进制，以太坊配置，串行路径）：场景 1、3、4 通过（`logs/e2e-134.log`，
  21 节点区块哈希一致）；场景 5、8 通过（`logs/e2e-58.log`）。场景 12 未跑。

### 被触碰但未变化的账户

规则取自 revm-database 43.0.1 的 `apply_account_state` 等源码：被触碰、存在且非空、但无变化的账户，
revm 仍给一次 `change()` 转移，bundle 里状态为 `Changed`（无 nonce 无代码时 `InMemoryChange`），
`info == original_info`、存储为空。evm2 只丢了这一种情形。
实现：`TrackingExecutor` 在 `finish_*` / `into_state` 时把这些账户补进返回的 `BundleState`
（`restored_slots.rs` 的 `touched_unchanged` / `add_touched`），所有下游（QMDB 操作推导、exec cache、
状态根策略、回放工具、共享执行输出、`eof_guard`、`StateDiff`）无需改动。
与 revm 的剩余差异：不补空的 `AccountRevert`（reth 对它不写任何东西，未发现 n42 读者）。
新增 8 个测试固定：零值转账到已存在/不存在账户、零小费 beneficiary、空操作内部调用、
回滚交易的目标、只做写后恢复的合约、被两笔交易触碰只登记一次。
未用测试固定：区块前系统调用（EIP-4788 / EIP-2935）——按代码阅读不会产生此类账户。

### QMDB-only 的引擎串行导入

事实：`n42_state::wrap` 在 v2.8.1 与 v2.11.0 都只有 `BlockchainProvider::state_by_block_hash` 一个调用点，
而引擎串行执行的状态提供者来自 overlay 工厂，绕过该钩子。v2.8.1 上 `N42_QMDB_READS=only` 且关闭并行导入时，
缓存未命中的导入读的是 MDBX 明文状态（一直在写，见 devlog-147），所以没暴露；真正读 QMDB 的只有批量导入钩子。
修复（reth `adac5ff764`）：新增 `n42_state::execution_provider`，在引擎构造执行读提供者的五处
（validator 的 `make_state_provider`、三处 prewarm、txpool-prewarm worker）套用；未安装适配器时等价于原行为。

### 原生配置区块串行导入失败（移植补丁引入）

"无收据树缩短导入路径"补丁在 `!uses_receipt_root()` 时不启动收据根任务，而 v2.11.0 的串行执行器在发送失败时报错，
导致原生配置区块的串行引擎导入全部失败（"receipt root task closed"）；以太坊配置不受影响，E2E 冒烟未覆盖。
修复（reth `20bc910983`）：不使用收据根时跳过发送。恢复后的导入夹具三个变体全过。

### 转账快速路径

`crates/n42-execution/src/fast_transfer.rs` = n42-rs `fast_handler.rs` + `direct_transfer.rs` 的 `eip7708_transfer_log`，
经 `N42EvmFactory::n42_tx_registry` 注册，`N42_FAST_TRANSFER=1` 开启（默认关）。
改动：起始分叉由 Prague 改为 Cancun（本链默认分叉）；去掉 n42-rs 的批线程本地开关。
与旧 revm 快速路径的差别：只省第一帧，不替换校验与 journal，单笔节省更小（未测量）；
拒绝原因的分项计数没有了，只剩一个 `delegated`；计数是进程级原子量。
9 个等价性测试（Cancun / Prague / Osaka / Amsterdam、非法交易表、账户边界、混合区块与标准执行逐项相同）。

### reth 分支状态（`n42/v2.11.0`，本地，未推送）

8 个移植补丁 + `f0e68aa66f`（公开 `EngineApiTreeState::new`）+ `adac5ff764` + `20bc910983`，共 11 个提交。

### 待办

阶段 2b：并行转账出块（整体搬 n42-rs 的 `batch_state` / `graft_fold` / `direct_transfer` / `parallel_common` /
`parallel_build`，以当时暂存的 legacy-revm 文件为行为规范；这些文件在阶段 2b 完成后删除）、跟随者并行导入（n42-rs `follower_import` 思路 + 既有 executed-block inbox）。
之后：原生配置的 E2E / 机群对拍、`reth-source.lock` / `patches/` / 脚本 / CI / `CLAUDE.md` / `README.md`。

## 进度记录（2026-11-19 续三）：阶段 2b——并行出块与跟随者并行导入

### 方案

n42-rs 的代码能独立成立的整体搬，n42-26 规则不同或 n42-rs 依赖其独有基础设施的部分从旧 revm 代码移植。

| 文件（`crates/n42-execution/src/`） | 行数 | 来源 |
|---|---|---|
| `batch_state.rs` | 519 | 整体搬自 n42-rs，约 80 行改动 |
| `graft_fold.rs` | 631 | 整体搬，约 140 行改动 |
| `direct_transfer.rs` | 544 | 整体搬，约 23 行改动 |
| `parallel_common.rs` | 225 | `partition_by_sender`（并集规则）、线程池、分批移植自旧代码 |
| `parallel_build.rs` | 1408（代码与测试各约 700） | 裁剪后搬：去掉 n42-rs 独有的线程采样、scatter slot、输出分片、延迟执行账本、late receipts、plan-ahead |
| `crates/n42-node/src/parallel_payload.rs` | 1356 | 出块器：串行循环由 `native_payload.rs` 生成，前缀与收尾移植自旧代码，新胶水约 150 行 |

真正新写约 250 行：`BatchState::touch` / `take_touched`、`graft_fold::sequential` 与带检查的合并算术、
`TrackingExecutor::{set_pending_fold, has_pending_fold}`、`commit_run`、`batch_config`。

### n42-26 固定语义

- 写后恢复槽位算脏：不变（批次的交易哈希经 `commit_preexecuted_transfer` 进入恢复槽位登记）。
- 收款方同时是发送方：沿用并集规则，不采用 n42-rs 的拒绝。
- 批次注资账户上的串行存储 / CREATE：n42-rs 拒绝，这里在 fold 时按累加器规则重算状态与 revert（有测试）。
- 被触碰但未变化的账户：EVM 路径与直接转账路径都上报，与串行一致。
- 与串行逐项相同：测试比较整个 `BlockExecutionResult`、整个 `BundleState`（revert 按地址排序后）、区块与 QMDB 根。

### 开关

`N42_PARALLEL_BUILD`、`N42_MAX_TXS_PER_BLOCK`、`N42_BUILD_TIME_BUDGET_MS`、`N42_PARALLEL_BUILD_THREADS`、
`N42_GRAFT_NO_CACHE`、`N42_TX_ROOT_EARLY`、H2-v4 收尾（原生收据根入头、`store_built_gov5_roots`、
替换状态根时抑制广播执行副本）均恢复。新增 `N42_BATCH_FAST_TRANSFER`、`N42_BATCH_DIRECT_TRANSFER`（默认开）。
**`N42_GRAFT_STREAM=1` 仅解析并告警**：新设计在收尾时一次合并，没有流式嫁接。

### 与 revm 路径的行为差异

1. 批次直接读父状态，不带区块前系统调用的 overlay；若系统调用改到了批次动过的账户，缓存一半拒绝，整块退回串行。
2. 提交前的拒绝（划分、worker 打开失败、未建模变化、合并拒绝、缓存冲突）改为退回串行并计数
   （`n42_parallel_payload_refused_total{reason}`），旧版是出块错误；收据提交后的失败仍是出块错误。
3. 批内普通转账默认走直接路径，不经 EVM，直到该批第一笔不合格交易。

### 跟随者并行导入（`N42_PARALLEL_IMPORT=1`）

设计选择：不走"无校验插入已执行区块"（需要在适配层重做头、体、执行后与 QMDB 根校验），
而走引擎已有的缓存执行路径：`RethExecutionLayer::new_payload` 中 `FollowerImport::prepare`
恢复发送方 → `state_by_block_hash` 打开父状态 → `parallel_build::import_payload` →
`store_payload_execution` 按区块哈希存入 → `engine.new_payload` 取用并做全部校验。
代价：交易解码两次，引擎仍做自己的校验，节省少于无校验插入。n42-rs 的 `follower_import.rs`（约 4,700 行，
多为 APoS 与延迟执行机制）未搬。

- 缓存输出被拒：重提一次且不带缓存输出，以第二次结果为准（任何 `INVALID` 都重提，真无效区块被校验两次仍无效）。
- 缓存是单槽，至多持有一个区块的输出；首次提交后无论结果都取回残留项。
- `import_payload` 婉拒（空块、发送方恢复失败、父状态不可用、非纯转账开头、分叉不在 Cancun–Osaka、
  系统调用冲突等）时不存缓存、照常提交，计入 `n42_parallel_import_declined_total{reason}`。
- 不处理：自己出的块、已带执行输出的块（出块器输出、compact block 的 leader 输出）、委托执行层。
- 指标：`n42_parallel_import_batch_stage_ms` 的阶段标签变为 `partition` / `execute` / `collect` / `finish`
  （`keys`、`prestate` 消失）；并行导入的块在引擎自身指标里显示为缓存命中而非执行。
- 测试：`crates/n42-node/tests/follower_parallel_import.rs`（4,096 笔转账的原生 H2 区块，QMDB-only 状态，
  与串行相同）；真实 `BasicEngineValidator` 上两种故意错误的缓存输出被拒后区块仍通过。

### 门

- 全 workspace check：`logs/f2-final.log`；clippy `-D warnings` 与测试（1897 通过、0 失败、31 忽略）、
  release 构建 `n42-node` / `e2e-test` / `n42-stress`：`logs/f3-final.log`，均 rc=0。
- 两个 `legacy-revm/` 目录已删除。脚本未使用 reth v2.11.0 删除的命令行参数。

### 未做

- n42-rs 直接转账测试里的 0x50（ed25519）用例未搬（需要 dev 依赖）。
- 并行出块、并行导入、快速路径均无任何节点级或机群级运行，无性能数据。

## 进度记录（2026-11-19 续四）：基线固定点、补丁文件、脚本、CI 与文档

只动固定点与文档，未改 Rust 源码，未跑 cargo，未提交。

### 设计决策

- **补丁文件**：沿用既有格式——每个适配提交一份 `git diff --abbrev=9 父 提交`，无邮件头。
  与旧文件逐字节对照确认：旧文件即 9 位缩写的纯 diff。八个沿用补丁保留原文件名；
  删除 `reth-import-batch.patch`、`reth-import-batch-tail.patch`；新增三份。
- **应用顺序改为提交顺序**：旧脚本的顺序与提交顺序不同。每份补丁是对其父提交的 diff，
  按提交顺序应用必然成立，其他顺序需逐次验证（未验证旧顺序在新系列上是否仍可用）；
  `reth-source.lock` 的 `patch_series` 与 `apply-reth-patches.sh` 的 `names` 现在顺序一致。
- **`reth-source.lock`**：去掉 `revm`、`alloy-evm` 两项；evm2 与 reth-core 没有 crates.io 版本号，
  新增 `git_dependency_revisions` 记录两个 rev。`schema` 仍为 1（读它的 `check-reth-source.sh`
  只取上游提交、适配提交、补丁数与 tag）。
- **不改写 reth 历史**：`1eb96074a7`、`f97241fff2` 两个沿用提交的标题仍写 "Reth 2.7"，内容已是 v2.11.0 的适配，仅在此注明。

### 补丁系列（v2.11.0 `b9abbcc1b982755baa9021e6874e2e58936ca65f` 之上，按应用顺序）

| # | 补丁文件 | reth 提交 | 说明 |
|---|---|---|---|
| 1 | `reth-qmdb-state-reader` | `1eb96074a7ca0edb77b4cae76734e1a1ef289a08` | 沿用（标题仍写 Reth 2.7） |
| 2 | `reth-n42-perf` | `f97241fff2c5817841de9f24619c0421ec75852d` | 沿用（标题仍写 Reth 2.7） |
| 3 | `reth-payload-transactions-root` | `9ad4387b84f41c4e4bbedec4a267747b593cd778` | 沿用；现在含 reth 自身 `Cargo.lock` 的一行（`rayon`） |
| 4 | `reth-gov5-cached-state-root` | `fe01928711602bea959b942ef66480f5df3b98b1` | 沿用 |
| 5 | `reth-engine-state-provider-factory` | `7b58b9d49f5ee2edfd5cad3ca41fb69d283aad3c` | 沿用 |
| 6 | `reth-cached-execution-refusal` | `2b4ba0f7719868daf57edf9a3437258dc602235f` | 沿用 |
| 7 | `reth-native-import-path` | `1574758d1a4359ffb22a17d75f0343353d4c6648` | 沿用（去掉 `open_parent`） |
| 8 | `reth-executed-block-inbox` | `d8d631efe031dacaa1c09ea0ddc270e6d58e44b2` | 沿用 |
| 9 | `reth-engine-tree-state-constructor` | `f0e68aa66f18a6e7e43498aec810e78c8e4c89ae` | 新增：公开 `EngineApiTreeState::new` |
| 10 | `reth-engine-execution-state-adapter` | `adac5ff76462fe5b4e09ddf1b73c15d574428a15` | 新增：引擎执行读提供者走状态适配器 |
| 11 | `reth-receipt-root-task-guard` | `20bc910983b45d53f5f5d287dca92f226d5c5622` | 新增：未启动收据根任务时不发送收据 |

适配提交（十一补丁后）：`20bc910983b45d53f5f5d287dca92f226d5c5622`，树 `91ca846db2135d3691804809bcdb3f386a32e214`。

### 改动清单

- `patches/`：重生成 8 份、删除 2 份、新增 3 份。
- `reth-source.lock`、`scripts/apply-reth-patches.sh`（基线与适配提交、补丁名单与顺序）、
  `scripts/check-reth-source.sh`（结束语的版本号改为读 lock 的 `upstream_tag`，不再写死）。
- `scripts/test_apply_reth_patches.py`：补丁名单改为 11 项。该测试在本次之前已有 3 例失败
  （名单停在六补丁时期，脚本要求十个文件）；另有一处断言与脚本现状不符——整套已应用时脚本只输出
  一行 "already carries the full N42 patch series"，不再逐补丁输出 "already applied"，断言随之更新。
- `.github/workflows/{e2e,nightly,coverage,execution-spec-shards}.yml`：reth 检出点改为 v2.11.0 的提交
  （共 5 处）。四个 workflow 与 Dockerfile 都不构建 `n42-evm-bench`，无需改构建目标。
- `Cargo.toml`：reth 依赖段的注释（原写 2.7.0 与其上游提交）。
- `CLAUDE.md`、`README.md`、`DEVLOG.md`、`docs/00-Workspace-Map.md`、`docs/01-System-Architecture.md`、
  `docs/03-Crate-Reference.md`、`docs/04-Product-Lines-and-Performance.md`、
  `docs/modules/binaries-and-tests.md`、`docs/modules/data-exec-zk.md`：基线、依赖、搁置的两个 crate；
  `n42-mobile` "零 reth 依赖"的说法改为实际情况（依赖 `reth-evm`、`reth-primitives-traits`、
  `reth-ethereum-primitives`、`reth-chainspec`、`evm2`）。`docs/01` 里 `N42_PARALLEL_EVM` 开关的描述
  改为已删除（源码中已无该开关）。
- 本文件：五处把 `legacy-revm/` 写成现存路径的句子改为历史表述。

### 验证

| 检查 | 结果 |
|---|---|
| 干净 v2.11.0 检出（`git worktree add --detach … v2.11.0`）上按 CI 方式运行 `apply-reth-patches.sh` | rc=0；`git add -A && git write-tree` = `91ca846db2135d3691804809bcdb3f386a32e214`，与 `n42/v2.11.0` 的 `HEAD^{tree}` 相同（21 个文件，+580 / −98） |
| 同一检出上再次运行 | "Reth source already carries the full N42 patch series"，rc=0 |
| `check-reth-source.sh`（打过补丁的干净检出、`../reth-worktree`） | 均 "Reth source matches the v2.11.0 N42 patch lock."，rc=0 |
| `python3 scripts/test_apply_reth_patches.py` | 7 通过（改动前 4 通过 3 失败） |
| `python3 -m unittest discover -s scripts -p 'test_*.py'` | 102 通过（含 `test_reth_ci_baseline.py`：每个 workflow 的 reth 检出点等于 lock 的上游提交） |

Rust 版本：reth v2.11.0 声明 `rust-version = "1.96"`，本仓库 `rust-version = "1.97"`、
`rust-toolchain.toml` 与 CI、Dockerfile 均为 1.97.1，无冲突。

### 未验证 / 遗留

- CI workflow 与 Docker 镜像未实际运行。
- `docker/n42-node/Dockerfile` 不复制 `vendor/`，而根 `Cargo.toml` 的 `[patch]` 引用
  `vendor/dependency-repairs/` 下两个路径（升级前的 revm-inspectors 修复同样如此），
  `execution-spec-shards.yml` 的镜像构建预计在解析依赖时失败；属既有问题，本次未改。
- `docs/performance-records.md`、`docs/batch-transfer-fastlane-reproduction.md` 里引用 `n42-evm-bench`
  的历史测量与复现命令未动（历史记录；命令在搁置期间不可用）。

## 进度记录（2026-11-19 续五）：阶段 3 验证

### E2E（最终 release 二进制，含全部阶段 2 改动）

场景 1、3、4 与 5、8、12 全部通过：`.artifacts/reth-upgrade-2110/logs/e2e2-all.log`。
E2E 使用以太坊配置与串行路径，不覆盖原生 0x50 配置和并行路径。

### 机群（原生 0x50 路线，7 验证者本机机群，链 ID 941050，160 万笔 0x50 转账，`--broadcast --connections 8`）

脚本：`.artifacts/native-port-p0/roundAR.sh`、`roundAS.sh`（leg 辅助脚本为 `run-*-r211.sh` 副本，旧脚本未动）。
构建：`cargo build --profile profiling --features pprof -p n42-node-bin`，`n42-node` sha256
`1af91dd7e3d350d9741d7c487cf32c84fdc66556ab7a3afdfe8cc3719625b71a`。原始输出：`run118.out` … `run122.out`、`run*-tps.txt`、
`run*-metrics-node*.txt`、`run*-logs/`。TPS 均为满载窗口内的 wall-clock 值（`wall-tps.py`），不是整轮平均。

| 轮次 | 拓扑 | 校验 | wall-clock TPS（窗口） | 升级前同配置 |
|---|---|---|---|---|
| run118 | 单执行节点（`--execution-owner 0`） | PASS，895 块，7 个密钥同链 | 296,252（块 65..80，5.03 s） | run116 364,939 / run117 368,819 |
| run121 | 同上 | PASS，889 块 | 305,586（块 66..81，4.88 s） | 同上 |
| run120 | 全节点导入 | PASS，577 个共同块，7 个根/哈希一致 | 240,823（块 41..48，2.96 s） | run112 264,825 / run109 277,360 |
| run122 | 同上 | PASS，556 个共同块 | 233,852（块 42..48，2.69 s） | 同上 |
| run119 | 全节点导入，但漏了 `--broadcast` 与 `N42_LEADER_INSERT_EXECUTED` | PASS，385 个共同块 | 不可比（窗口只有 3 个块） | — |

结论：五轮正确性全部通过；并行出块、快速路径、跟随者并行导入均在真实节点上运行。
**TPS 低于升级前**：单执行节点约低 17–19%，全节点导入约低 10–15%（各两轮，未做更多重复）。

观察到的原因线索（未进一步定位）：

- leader 打包 118–122 ms（升级前 104–107 ms）；finish 43–49 ms（升级前约 53 ms）。
  run118 中 1,577,816 笔走并行批次，22,184 笔走串行快速路径；无拒绝、无退回串行。
- 跟随者并行导入每块约 193 ms（中位），其中 `collect` 61、`finish` 61、`execute` 10、`partition` 3。
  每节点有 16 个块因 `parent-state`（父状态按哈希不可取）被婉拒而串行执行；无重提。
- `N42_GRAFT_STREAM=1` 在新设计里不生效（见续三）。
- 转账快速路径只省第一帧（见续二）。

### 状态

- 升级功能完成：reth v2.11.0 + 11 个补丁（本地分支 `n42/v2.11.0`，HEAD `20bc910983`，未推送）；
  n42-26 分支 `upgrade/reth-v2.11.0` 全部改动未提交。
- 全 workspace check / clippy `-D warnings` / 测试（1897 通过）通过；补丁序列在干净 v2.11.0 上复现适配树。

### 后续

1. 性能回归定位：跟随者导入的 `collect` / `finish` 两段与 `parent-state` 婉拒；leader 打包多出的约 15 ms。
2. 未做：真实链 94 数据回放对拍 QMDB 根（`n42-qmdb-replay`）；CI 工作流与 Docker 镜像实际运行
   （`docker/n42-node/Dockerfile` 未拷贝 `vendor/`，镜像构建大概率在依赖解析处失败，升级前即如此）；
   手机流数据包的版本号（新旧 EVM 不可混跑，目前靠同时升级保证）。
3. 搁置的 `n42-parallel-evm` / `n42-evm-bench` 是否迁移或删除。

## 进度记录（2026-11-20）：leader 侧性能回归定位与修复

升级提交已推送：`origin/upgrade/reth-v2.11.0`，六个提交（`6dd9f6f` 起）。以下改动在其之上，尚未提交。

### 定位

同配置下 node0 满块（95,238 笔）各阶段中位数（`qgNb/logs/node0.log` 对 `qgSb/logs/node0.log`）：

| 阶段（ms） | run117（revm） | run121（evm2，修复前） |
|---|---|---|
| `packing_ms` | 104 | 128 |
| `evm_exec_ms` | 96 | 118.5 |
| `executor_finish_ms` | 4 | 53 |
| `root_wait_ms` | 26.5 | 28.5 |
| `N42_PAYLOAD_CACHE finish_ms` | 50 | 97.3 |
| `N42_PAYLOAD_CACHE cache_ms` | 11.3 | 0.2 |

每块净增约 60 ms。原因：批次合并做了两遍且都串行——最后一个批次结束后 `merge_batches`
（计入 `evm_exec_ms`，日志 `collect_ms` 44–48，旧版 3），收尾时 `fold_into` 再把每个账户从中间结构移出、
插入区块 map，并推入约 19 万条 revert 后排序。批次执行本身不慢于旧版（`groups_ms` 14–16 对 29–30）。

### 修改（`crates/n42-execution/src/{graft_fold,parallel_build,parallel_common}.rs`）

1. `MergedBatches` 直接采用区块 bundle 的形状（一个 `AddressMap<BundleAccount>` + 各批 revert 集 + 跨批账户小表），
   `fold_into` 把它整体移入，只把区块自己的少量账户并进去。
2. 逐批检查（存储、代码、销毁、每账户恰一条无存储 revert）移到批次线程（`CheckedBundle::new`）。
   新增拒绝 `FoldRefused::Reverts`。
3. 合并流式化：`execute_batches` 用 `pool.in_place_scope`，每个批次完成即在调用线程合并（`BatchMerger::add`）。
4. revert 在构建线程池上拼成一个已排序集合（`parallel_common::concat_sorted`），fold 不再排序。

日志行 "parallel transfer prefix executed"：`groups_ms` 现含流式合并，`collect_ms` 只剩尾部，新增 `merge_ms`。
bundle 内容不变，等价性测试未改且通过。

### 微基准（`leader_stages_of_a_full_block_of_transfers`，`#[ignore]`；95,238 笔直接转账、190,476 个账户、16 线程、release、本机，12 轮取第 2–11 轮中位数）

| 阶段（ms） | 修改前 | 修改后 |
|---|---|---|
| `execute_batches` | 100.2 | 51.1 |
| `commit_run` | 5.3 | 5.9 |
| `executor.finish()` | 86.3 | 9.2 |

### 机群（round AT，`roundAT.sh`，构建 sha256 前缀 `01a49a9461c41972`，原始输出 `run123.out`–`run125.out` 等）

| 轮次 | 拓扑 | 校验 | wall-clock TPS（窗口） | 打包 | `executor_finish` | finish 合计 |
|---|---|---|---|---|---|---|
| run123 | 单执行节点 | PASS，889 块 | 404,205（块 65..82，3.96 s） | 84.5 | 11 | 57.2 |
| run124 | 单执行节点 | PASS，879 块 | 404,397（块 66..83，3.96 s） | 86.5 | 10 | 56.5 |
| run125 | 全节点导入 | PASS，544 个共同块 | 256,499（块 43..45，1.11 s，仅 3 块，不稳） | — | — | — |

单执行节点拓扑高于升级前（364,939 / 368,819）。全节点导入仍低于升级前（264,825 / 277,360），
且窗口过短；跟随者每节点有 16 个块因 `parent-state` 婉拒而串行执行（定位中）。

### 待办

- `n42-execution` 测试套件间歇失败（`restored_slots_for(..).is_some()`）：疑为进程级 512 项登记表被淘汰测试挤出，确认并修复中；
  同时评估生产负载下登记表是否会淘汰仍在使用的条目。
- 跟随者 `parent-state` 婉拒与 `collect` / `finish` 两段各约 61 ms。
- 可选：剩余的串行入表（约 15–20 ms）移出关键路径。

## 进度记录（2026-11-20 续）：测试竞争与跟随者父状态

### `n42-execution` 测试间歇失败

原因（已证实，`logs/q2-repro.log`、`logs/rsdbg/quiet8.txt`）：淘汰测试 `the_registry_is_keyed_ordered_and_bounded`
向进程级 512 项恢复槽位登记表写入 520 项，挤掉其他测试刚写入、尚未读取的条目。
修复：`RestoredSlotsRegistry` 改为可实例化（`new(kept)` / `record` / `get`），进程级登记表仍是同一类型、上限 512，
`record_restored_slots` / `restored_slots_for` 行为不变；淘汰测试用自己的实例。套件连续 22 次全绿（`logs/q3-gates.log`）。
生产评估：每块登记 1–2 个键，读取紧跟执行（亚秒级），约 4 块/秒时 512 项覆盖一分钟以上，不会淘汰在用条目。
未做的加固：重复登记的键不前移；读取方 `unwrap_or_default()` 的未命中是静默的（只表现为根不一致），可加未命中计数。

### 跟随者 `parent-state` 婉拒

原因（run125 node3 日志）：`BlockchainProvider::state_by_block_hash` 只回答规范链内存块、pending 块或数据库；
引擎已校验但尚未成为规范头的块对它不可见（引擎校验器从自己的树状态读）。导入一旦在导入闸门排队，
`prepare(N+1)` 在 N 插入的瞬间开始，而 N 的 forkchoice 更新要 22–27 ms 后才处理；被婉拒的块串行执行更慢（385–477 ms 对 180–202 ms），
于是 forkchoice 更新越排越后（实测落后到数秒），之后每块都被婉拒。

修复（`crates/n42-node/src/el/follower_import.rs`）：`prepare` 每 1 ms 询问一次父状态，最多
`N42_PARALLEL_IMPORT_PARENT_WAIT_MS`（默认 60，`0` 恢复单次尝试）；只按父块哈希打开状态，不可能用错父块；超时照旧婉拒。
新增观测：`n42_parallel_import_batch_stage_ms` 的 `recover`、`parent_wait` 阶段，
`n42_parallel_import_parent_wait_ms{outcome=ready|timeout}`。

### 机群（round AU，`roundAU.sh`，原始输出 `run126.out`–`run128.out`、`run12[678]-metrics-node*.txt`）

| 轮次 | 拓扑 | 校验 | wall-clock TPS（窗口） | 升级前同配置 |
|---|---|---|---|---|
| run126 | 全节点导入 | PASS，568 个共同块 | 290,555（块 41..47，2.28 s） | run112 264,825 / run109 277,360 |
| run127 | 全节点导入 | PASS，563 个共同块 | 283,967（块 41..48，2.60 s） | 同上 |
| run128 | 单执行节点 | PASS，880 块 | 385,571（块 65..82，4.15 s） | run116 364,939 / run117 368,819 |

node3 的跟随者导入（run126 / run127）：`parent-state` 婉拒 12 / 3（修复前 16）；等待成功 6 / 16 次，
中位 0.1 / 19 ms；超时 12 / 3 次。每块准备中位 151 / 144 ms：`recover` 43 / 38、`execute` 46 / 44（含流式合并）、
`finish` 26 / 20、`collect` 6、`partition` 3–4。

结论：两种拓扑均不低于升级前。单执行节点三轮为 404,205 / 404,397 / 385,571；全节点导入两轮为 290,555 / 283,967。
窗口都只有几秒（7–18 个块），轮间波动约 5%。

### 仍可做

1. 父状态改从引擎树读取（与校验器同路），彻底去掉等待与超时婉拒；需要新增一个 reth 补丁。
2. 跟随者每块仍约 340 ms（准备约 147 + 引擎处理缓存输出约 190），高于约 250 ms 的出块节奏；
   较大的杠杆是引擎侧的 190 ms 与 `recover` 的约 40 ms。
3. leader 剩余的串行入表（约 15–20 ms）移出关键路径。

## 进度记录（2026-11-20 续二）：跟随者每块开销与 run129 失败

已推送：`e62cd30`（流式合并）、`203cbd4`（测试隔离）、`8f783f1`（父状态有界等待）、`019a095`（文档）。以下改动在其之上，尚未提交。

### 引擎侧约 190 ms 的构成（run127 node3，满块，缓存命中）

`validate_total` 约 159：`await_convert` 约 72（`standard_header` 65 + `ethereum_validate` 18）、`state_root` 约 69
（QMDB `commit` 约 45、key 约 5、derive 约 4.5、sort 约 1.6）、`pre_execute` 约 11、`post_execution` 2–3。
最大的可避免项是交易根：`convert_payload_to_block` 里串行的 `ordered_trie_root_encoded`（`payload-convert` 线程 99 个样本中 93 个）。
revm 版本里它与引擎内执行重叠；现在执行在 `prepare` 中完成，引擎只是干等。
第二项：缓存命中时校验器仍启动交易迭代与预热转换（约 190 个 `cpu` 线程样本），结果从未使用。

### 第一批改动（round AV 验证）

1. 交易根与执行并行（`follower_import.rs`）：`prepare` 入口起 `import-tx-root` 线程，输出存入后
   `store_payload_transactions_root`；`convert_payload_to_block` 仍对重建的头校验两个哈希，错误的根只会让区块哈希校验失败。
   新阶段 `tx_root_wait`。
2. 缓存命中时不转换交易（reth `5b4495f3fd`）。
3. `recover`（`evm_config.rs`）：从 `&[u8]` 解码，不再逐笔克隆 / 释放 `Bytes`（共享引用计数）；发送方来源计数本地累计、每个 payload 发布一次。
   `payload_recovery_of_a_full_block`（`#[ignore]`，95,238 笔真实 Ed25519 0x50 转账，14 轮中位）：共享缓冲 29.8 → 11.6 ms，各自缓冲 37.6 → 10.6 ms。
4. 父状态从引擎树读（reth `c71add1512`：`BlockchainProvider::executed_state_by_block_hash`），有界等待保留为兜底。

round AV（`roundAV.sh`，构建 `92a71646…`）：run130 全节点导入 309,697 PASS；run131 单执行节点 379,500 PASS；
**run129 全节点导入校验失败**（"a committed QC has no executed block after the FCU grace period"，260,350，仅块 42..47）。
已生效：准备中位 113 / 110 ms（`recover` 24、`execute` 43–46、`finish` 21–23、`tx_root_wait` 约 0）；
引擎处理缓存命中满块约 107–113 ms（原约 190）。

### run129 失败的因果（`qgVa/logs/node5.log`）

1. 共识层"ExecutionReady deadline expired"对已导入的第 44 块发起后台导入，在 `prepare` 中重新执行它。
2. `reth_evm::payload_cache` 只有一个执行输出槽；重复的 44 的输出覆盖了刚存入的 45 的输出，引擎串行执行 45（441 ms）。
3. 导入闸门在机群里未启用，`prepare(N+1)` 在区块数据到达时就开始，此时 N 仍在引擎里执行——既非规范头也不在引擎树，
   60 ms 等待超时后婉拒（46..59 共 14 次），每块串行 320–500 ms，高于约 250 ms 的节奏，自我强化。
4. `new_payload(N+1..)` 先入引擎队列，FCU(N) 排在其后：FCU 延迟 502 → 2178 ms，`Finalized` 落后峰值 5.3 s，
   超过校验脚本每节点每样本 2 s 的宽限（`committed_block` 每 50 ms 轮询 `eth_getBlockByHash`）。不是纯粹的收尾假象，也不是分叉。
   同类失败在升级前也出现过（run102–104、run113）。
5. 引擎树路径本身没坏，只是从未轮到：健康节点上 provider 先答，落后节点上父块尚未入树。

### 第二批改动（round AW 验证）

- 导入按父子顺序（`el/reth_engine.rs`、`import_gate.rs`）：配置了跟随者导入时闸门常开；一个块的导入等待本节点上父块正在进行的导入结束
  （`ImportGate::settled`），位置在发送方恢复之后、打开父状态之前；等待结束时父块已在树里。
  payload 只在父块导入返回后提交，引擎队列不再积压后代，FCU(N) 面对的是空闲引擎。
- 引擎已持有的块不再执行（婉拒原因 `known`）。
- 执行缓存保留最近 4 个输出（reth `4c1615dd27`）。
- 父状态婉拒改为 `n42::el` 上的 WARN。新直方图 `n42_engine_import_parent_wait_ms`。
- 测试：`follower_parallel_import` 夹具改为校验器与 provider 工厂共用 overlay manager（与节点一致），覆盖"已校验未规范"的父块读取、
  `prepare_after`、重复导入；`import_gate` 与 reth `payload_cache` 各一个单元测试。

round AW（`roundAW.sh`，构建 `1a9f1b48917dc53a`，原始输出 `run132.out`–`run135.out` 等）：

| 轮次 | 拓扑 | 校验 | wall-clock TPS（窗口） |
|---|---|---|---|
| run132 | 全节点导入 | PASS，565 个共同块 | 269,886（块 42..59，5.93 s） |
| run133 | 全节点导入 | PASS，557 个共同块 | 305,547（块 43..59，5.24 s） |
| run134 | 全节点导入 | PASS，546 个共同块 | 311,811（块 44..60，5.13 s） |
| run135 | 单执行节点 | PASS，881 块 | 398,604（块 65..82，4.01 s） |

三段全节点导入中 node0 / node3 / node5 的 `parent-state` 婉拒均为 0，`known` 婉拒 0–1，父状态全部来自 provider；
准备中位 103–138 ms。全节点导入的窗口现在覆盖全部 160 万笔（17–18 个块）。

### 未解释

run129 中 node5 的引擎在追平后静默约 2 s（FCU view=65 用时 1950 ms），猜测是持久化积压的 17 个满块，未验证。

### 仍可做

- QMDB bound candidate 用于跟随者输出（把约 50 ms 的 derive / sort / 树工作移出引擎，需要 store 里新的握手）。
- leader 剩余的串行入表（约 15–20 ms）。
- 引擎线程上的小项：`TreeState::remove_until` / `OverlayManager::remove_blocks`、`canon-notify` 克隆输出。

## 进度记录（2026-11-20 续三）：补丁系列增至十四个

`n42/v2.11.0` 在 `20bc910983` 之上新增三个提交；只动固定点与文档，未改 Rust 源码，未跑 cargo，未提交。

| # | 补丁文件 | reth 提交 | 改动文件 |
|---|---|---|---|
| 12 | `reth-cached-execution-skip-tx-conversion` | `5b4495f3fd7542839872c46afc09a68dc33fb1ca` | `engine/tree/src/tree/payload_validator.rs` |
| 13 | `reth-pre-canonical-block-state` | `c71add151225cf7c3c9c95a9937e9ef914c221c3` | `storage/provider/src/providers/blockchain_provider.rs`、`storage/storage-overlay/src/manager.rs` |
| 14 | `reth-payload-cache-recent-outputs` | `4c1615dd276381522b0252a6fc625db4b9c68a7f` | `evm/evm/src/payload_cache.rs` |

适配提交（十四补丁后）：`4c1615dd276381522b0252a6fc625db4b9c68a7f`，树 `d23613603928bffe602b1d3a781320579a288710`。

- 三个提交改的文件前面的补丁也改过（`payload_validator.rs`、`blockchain_provider.rs`、`payload_cache.rs`），
  但各补丁是对各自父提交的 diff，前十一份无需重生成：逐份与 `git diff --abbrev=9 父 提交` 比对，字节相同。
- 同步：`reth-source.lock`（`adapted_commit`、`patch_series` 14 项）、`scripts/apply-reth-patches.sh`
  （适配提交、名单）、`scripts/test_apply_reth_patches.py`（名单）、`CLAUDE.md`（十四个、适配提交）、
  `README.md`（fourteen）、`DEVLOG.md` 索引行。CI workflow 只固定上游提交，无需改动。

| 检查 | 结果 |
|---|---|
| 干净 v2.11.0 检出上运行 `apply-reth-patches.sh` | rc=0；树 `d23613603928bffe602b1d3a781320579a288710` = `n42/v2.11.0` 的 `HEAD^{tree}`（22 个文件，+731 / −100） |
| 再次运行 | "Reth source already carries the full N42 patch series"，rc=0 |
| `check-reth-source.sh`（打过补丁的干净检出、`../reth-worktree`） | 均 "Reth source matches the v2.11.0 N42 patch lock."，rc=0 |
| `python3 scripts/test_apply_reth_patches.py` / `python3 -m unittest discover -s scripts -p 'test_*.py'` | 7 通过 / 102 通过 |

未验证：CI workflow 与 Docker 镜像未实际运行。

## 进度记录（2026-11-20 续四）：leader 周期时间线与两处小改

已推送：`ba3af32`、`5143588`、`19d7ccf`、`83f577d`。以下两处改动在其之上，尚未提交。

### leader 周期（满块 95,238 笔，13 个稳定满块的中位数，起点为 "New payload job created"）

来源：`qgWd/logs/node0.log`（run135，单执行节点）、`qgWc/logs/node1.log`（run134，全节点导入）。
run135 的 pprof 时间戳未落在负载窗口内，未用于归因。

| 阶段（ms） | 单执行节点 | 全节点导入 | revm 时代（run109） |
|---|---|---|---|
| 池遍历、候选 | 约 6 | 约 10 | |
| 前缀合计 | 79 | 113 | |
| – 划分 / 批次（含流式合并）/ collect / 其余 | 3 / 35 / 4 / 约 9 | 4 / 48 / 6 / 约 12 | |
| – 提交（手续费与结果循环 + `commit_run`） | 10 | 16 | |
| – 把已执行转账移入区块 | 18 | 约 27 | |
| **打包完成** | **@88** | **@127** | 129 |
| executor finish / 收据根 / assemble / 等根 / 未计 | 10 / 1 / 9 / 28 / 约 8 | 11 / 1 / 13 / 31 / 约 9 | |
| **finish 合计** | **56** | **65** | 64 |
| cache | 0.2 | 0.3 | 19 |
| built → 校验启动（区块转 payload、规范化、改键、克隆） | 15 | 约 13 | 约 22 |
| 自有块 prepare（重建 13 + QMDB 提交 16.5–18.5，与 hashed state 并行） | 32 | 约 42 | |
| 引擎插入 | 14.4 | 16.5 | |
| FCU → 下一 job | 1.4 | 0.1 | 13 |
| **周期** | **211**（195–226） | **280**（248–312） | 约 295 |

结论：共识不在关键路径（`N42_BUILD_ON_SEAL` 下，下一 job 在自有块插入后即开始；N 的提交在 job N+1 开始后中位 123 ms 落地）；
阶段之间无空闲；全节点导入是同一流水线被同机六个跟随者拉长约 1.3 倍。

### 两处小改

1. `move_executed`（`crates/n42-node/src/parallel_payload.rs`）：候选全部执行时，95,238 × 368 字节的交易并行移入一次性预留的空间。
   微基准 `moving_a_full_block_of_executed_candidates`（`#[ignore]`）17.8 → 7.5 ms；日志新增 `moved_ms`。
2. 自有块插入（`crates/n42-node/src/el/own_block.rs`）：先在不含交易的 body 上推导并校验头，再与 QMDB 提交、hashed state 并行取 body。
   新指标阶段 `body`，`rebuild` 现只指头。

### 机群（round AX，`roundAX.sh`，构建 `d48726eda5c39468`，原始输出 `run136.out`–`run138.out` 等）

| 轮次 | 拓扑 | 校验 | wall-clock TPS（窗口） |
|---|---|---|---|
| run136 | 单执行节点 | PASS，883 块 | 415,375（块 66..82，3.85 s） |
| run137 | 单执行节点 | PASS，895 块 | 404,432（块 66..82，3.96 s） |
| run138 | 全节点导入 | PASS，557 个共同块 | 335,479（块 42..59，4.77 s） |

node0 中位数（run135 → run136 / run137）：`moved_ms` 约 18 → 3；前缀合计 77.5 → 67 / 68；`packing_ms` 87.5 → 72 / 75；
finish 56 → 55；周期 208.9 → 192.2 / 205.7。自有块 `prepare` p90 34.4 → 26.3 / 26.7（`rebuild` 13.0 → 1.9 / 1.6，`body` 14.4 / 18.0）。
无 "own block does not rebuild" / "QMDB candidate was not committed" 告警。

### 下一步候选（未做）

| 项 | 预计 | 说明 |
|---|---|---|
| QMDB 根任务与 executor finish 并行 | 约 10–15 ms | 改变操作推导的位置，不算小 |
| built → 校验启动复用帧字节 | 约 5–8 ms | 不再重编码 95k 笔交易 |
| 引擎插入时 leader 用不到的 bundle 拷贝 | 约 4–5 ms | `PayloadProcessor::on_inserted_executed_block`，需 reth 提交 |
| 提交循环并行化 | 约 4 ms | |
| 提前构建（build-ahead） | 打包约 72–88 ms 离开关键路径 | 见下 |

**提前构建设计要点**：N 的状态变化在其打包结束时已定；N+1 的批次在 N 的 finish 与衔接期间，以 N 的 `MergedBatches`
（已是 bundle 形状，需改为 `Arc` 共享）叠在 N 所用父 provider 之上执行；job N+1 采用其 `ParallelRun`，
`commit_run`、executor finish、QMDB candidate、组装照旧，因此收据、恢复槽位、被触碰账户、QMDB 操作由同一段代码产生。
仅限帧顺序构块（候选在 N 打包结束时即可确定）、本节点连任、N 为纯前缀块。
采用条件：父哈希为 N 的封存哈希且 N 由同一执行键的执行插入；区块环境（时间戳含 leader 的 bump、beneficiary、base fee、gas limit、
prevrandao、分叉）一致；候选列表一致；无提款或区块前系统调用改到批次账户。任一不满足即丢弃，仅损失 CPU。
预期：单执行节点周期约 150–160 ms（TPS 约 +30–35%，估算未实测）；全节点导入下跟随者约 220 ms/块会先成为瓶颈。
规模约 400–600 行（n42-execution、n42-node），无需 reth 改动。

### run129 中 node5 引擎静默约 2 s

info 级日志无法解释。已排除：重复导入（在静默开始后才提交）、backpressure（最大为 0）。
嫌疑：finalize 从 48 跳到 64 之后的持久化完成与树 / overlay 清理，或该 FCU 的规范化本身。
复现需 `RUST_LOG=info,engine::tree=debug,engine::persistence=debug,storage::overlay::manager=debug` 及覆盖追赶期的 pprof；
导入按序后造成它的积压不再形成，可能不复现。

## 进度记录（2026-11-20 续五）：leader 收尾与衔接的四处小改

AX 轮确认上一节两处改动后（run136/137 415k/404k TPS），继续做时间线表里便宜的几项。

### 设计决策

- **QMDB 操作提前派生**：操作表在 `executor.finish()` 之后才完全确定（串行尾部、多批共享账户、touched-unchanged 都在 fold 时定型），
  所以不能整体提前。能提前的是"只被一个批次改过"的账户：fold 把它们原样搬进 bundle。做法是在 `commit_run` 提交回执的同时派生这些账户的叶子，
  另起线程排序；块尾只重算 fold 报告点名的账户（共享账户、串行部分的全部账户、touched 账户）及 restored slot、Prague system caller，再并入。
  `prepare_bound_candidate`（29 ms）仍须等最终操作表，未动；要让它与 `executor.finish()` 并行，需要 fold 在状态半完成时把被点名账户的条目交给已启动的 root 任务
  （把 scope 提到 finish 之前、FoldReport 带条目而非地址），属于更大的重构，未做。
- **built → 验证启动**：测得 payload 转换 4.9 ms（并行编码），首次复制 95k 个 `Bytes` 2.4 ms（每个一次分配）、之后每次 0.9 ms、释放 0.6 ms。
  原归一化路径复制两遍、释放两遍、再逐字节比较 14 MB。改为按值归一化：native 路径且已有交易根时把交易表取出，在无交易的壳上派生头并做
  "执行输入不变"检查，再把同一张表放回。验证用的那份复制改为并行。复用 frame 字节未做：编码只占 4.9 ms，而按哈希在 FrameStore 里串行查 95k 次并不更快，
  且需要先证明 frame 原始字节与规范编码逐字节相同。
- **提交循环**：哈希 / 类型 / 小费改为并行读取，结果以惰性迭代器交给 `commit_run`，不再多搬一遍。
- **reth**：state cache 关闭且未与 payload builder 共享时，`on_inserted_executed_block` 不再把 bundle 抄进 execution cache（此时无人读取）。
  默认配置行为不变，需 `--engine.disable-state-cache` 才生效；代价是引擎自己执行区块（串行回退）时没有跨块缓存。

### 实施细节

- `n42-execution`：`MergedBatches::unshared()`、`refolded()`；`FoldReport` 与 `TrackingExecutor::report_fold()`；`commit_run_beside`。
- `n42-node`：`qmdb_state::EarlyOperations::{derive, sorted, finish}`；`parallel_payload.rs` 中的接线，debug 构建下 `debug_assert_eq!` 与原路径逐项比较。
- `n42-network`：`normalize_built_payload_for_gov5_h2`；`ExecutionOutputCache::rekey_gov5_built`；`execution_bridge.rs` 的 `copy_payload`。
- reth `n42/v2.11.0`：`a8583ac87e`（条件化缓存写入 + 测试）、`e037d26ce8`（`n42_insert_executed_stage_ms`）。
- 新指标：`n42_payload_builder_qmdb_operations_ms{source}`、`n42_leader_emit_stage_ms{stage}`、
  `n42_engine_boundary_duration_ms{phase="payload_convert"}`、`n42_insert_executed_stage_ms{stage}`。

### 测量（本机，release，190,476 账户 / 95,238 笔）

- 操作表：从 bundle 整体派生 8.98 ms；提前派生 6.55 ms（与 6.2 ms 的回执提交并行）+ 排序 5.82 ms（独立线程）；块尾 0.83 ms。
- payload 复制：首次 2.39 ms → 并行 1.09 ms。

### 遇到的问题

- 第一版块尾用稳定排序并入，4.4 ms；改为从尾部原地归并后 0.83 ms。
- 第一版把排序也放在 `commit_run_beside` 里，比回执提交长约 3.5 ms，会拖长 commit；拆成独立线程。
- `graft_fold` 新测试里原以为第二个共享账户存在，实际该夹具只产生一个共享账户，且它同时在串行部分里；
  "共享且不在串行部分"的账户由 `parallel_build` 的 shared-recipients 用例经不变式覆盖，但夹具未直接断言其共享。

### 状态与后续

四项均已实现并通过门禁；待车队确认。pprof 采集错过负载是 leg 脚本的时序问题（flood 启动 3 s 后才触发），节点侧无需修改。

## 进度记录（2026-11-20 续六）：AY 轮结果——撤回操作提前派生

AY 轮（run139–142）全部通过，但 finish 从 55 ms 变为 62–63 ms，周期未缩短。

- **撤回"QMDB 操作提前派生"（续五第一项）**：真实区块上 `n42_payload_builder_qmdb_operations_ms{source="ahead"}` 中位 11.5 ms，
  而旧路径在同样的区块上约 5.4 ms（root 任务 p90 35.8 ms − prepare 30.4 ms）；root 任务 p90 35.8 → 42.7 ms，prepare 不变（30.4 → 30.6）。
  原因有二：其一，车队区块每块约 10.2–10.7 万个操作，本机基准用的是 19 万账户，旧路径可省的上限本来只有约 5 ms；
  其二，pprof 显示块尾走的是"≥4096 个待重算账户 → 整表并行重排"分支，即 flood 的收款地址散列（`index * K % spread`）在一块内有上千个重复，
  落在不同批次即成为共享账户，全部要在块尾重算。块尾的 CPU 只有约 2 ms/块，其余是等待（排序线程与全局 rayon 池上的其他任务争用）。
  收益上限小、需要 fold 侧为共享账户提前定型才能做对，故整项撤回：`EarlyOperations`、`FoldReport`、`commit_run_beside`、
  `MergedBatches::{unshared, refolded}` 及其测试全部移除。
- **保留 built → 验证启动（第二项）**：built → spawn 12.6/13.1 → 5.3/6.1/5.3 ms，built → 广播 54.5/55 → 44–46 ms。
  `n42_leader_emit_stage_ms` 的中位数被空块淹没（99 块里 18 个满块），满块看 p90：`to_validation_spawn` 约 0.3 ms，
  即 `handle_built_payload` 内部已不是瓶颈，剩下的 5 ms 在它之前（resolve 唤醒与 payload 转换，后者 p90 1.5 ms）。
- **保留提交循环并行（第三项）**：`commit_ms` 12 → 10–11。
- **reth 条件化缓存写入（第四项）保留，默认不生效**：run142 加 `--engine.disable-state-cache` 后，满块 "Block added" 14.2 ms → 0.1 ms
  （日志单位变成 µs），built → 下一个 payload job 47.5 → 31.8 ms；但本块的 frame-ref 编码与验证完成反而晚了约 8 ms
  （built → validated 44–46 → 54.3，下一轮构建提前开始后与之争用），周期 188.6 ms 对 190.4 / 206.6，在噪声内。

## 进度记录（2026-11-20 续七）：补丁系列增至十六个

`n42/v2.11.0` 在 `4c1615dd27` 之上新增两个提交；只动固定点与文档，未改 Rust 源码，未跑 cargo，未提交。

| # | 补丁文件 | reth 提交 | 改动文件 |
|---|---|---|---|
| 15 | `reth-inserted-block-cache-skip` | `a8583ac87e61027eda77bfce626546561032b92d` | `engine/tree/src/tree/payload_processor/mod.rs` |
| 16 | `reth-insert-executed-stage-timers` | `e037d26ce8589c907cce170ee380375d78364b25` | `engine/tree/src/tree/payload_validator.rs` |

适配提交（十六补丁后）：`e037d26ce8589c907cce170ee380375d78364b25`，树 `dec72b6069aa497911f2dc86d0aee5780d5a900a`。

- 前十四份补丁无需重生成：十六份文件逐份与 `git diff --abbrev=9 父 提交` 比对，字节相同。
- 同步：`reth-source.lock`（`adapted_commit`、`patch_series` 16 项）、`scripts/apply-reth-patches.sh`
  （适配提交、名单）、`scripts/test_apply_reth_patches.py`（名单）、`CLAUDE.md`（十六个、适配提交）、
  `README.md`（sixteen）、`DEVLOG.md` 索引行。CI workflow 只固定上游提交，无需改动。

| 检查 | 结果 |
|---|---|
| 干净 v2.11.0 检出上运行 `apply-reth-patches.sh` | rc=0；树 `dec72b6069aa497911f2dc86d0aee5780d5a900a` = `n42/v2.11.0` 的 `HEAD^{tree}`（23 个文件，+780 / −102） |
| 再次运行 | "Reth source already carries the full N42 patch series"，rc=0 |
| `check-reth-source.sh`（打过补丁的干净检出、`../reth-worktree`） | 均 "Reth source matches the v2.11.0 N42 patch lock."，rc=0 |
| `python3 scripts/test_apply_reth_patches.py` / `python3 -m unittest discover -s scripts -p 'test_*.py'` | 7 通过 / 102 通过 |

未验证：CI workflow 与 Docker 镜像未实际运行。

## 进度记录（2026-11-20 续八）：round AZ 与收尾验证

round AZ（`roundAZ.sh`，操作提前推导已回退；原始输出 `run143.out`–`run145.out` 等）：

| 轮次 | 拓扑 | 校验 | wall-clock TPS（窗口） |
|---|---|---|---|
| run143 | 单执行节点 | PASS，885 块 | 442,226（仅 1,333,332 笔，块 65..78，3.02 s；窗口不完整，不与其他段直接比较） |
| run144 | 单执行节点 | PASS，884 块 | 417,140（块 65..81，3.84 s） |
| run145 | 全节点导入 | PASS，565 个共同块 | 363,644（块 43..59，4.40 s） |

node0 满块中位数（run136 / run137 → run143 / run144）：`commit_ms` 12 → 8 / 9；`packing_ms` 72 / 75 → 69 / 71；
finish 55.3 / 54.7 → 55.3 / 55.7；周期 192.2 / 205.7 → 187.0 / 188.5。

门：workspace check、clippy `-D warnings`、测试 1901 通过 / 0 失败 / 36 忽略、release 构建、E2E 1,3,4,5,8,12 全过
（`.artifacts/reth-upgrade-2110/logs/s2-final-gates.log`）。

## 进度记录（2026-11-21）：与 n42-rs 5.69M 的对照与方案调整

### n42-rs 的 5.69M 是什么

来源：n42-rs `main`（`284a3a0e3`）`docs/PLAN_6M_2026_11.md` 第 5 节 2026-11-21 条目，pair FTK：5.55 / 5.69M（5.69M 是迄今最好的一段）。
口径：fleet7 X22 轮的 window 1，即压测开始约 10 s 后的第一个 30 s 窗口（`scripts/fleet7-bench.sh` `WINDOW_SEC=30`，
`fleet7-measure.py` 按日志计窗口内封块的交易数 / 30）。拓扑 E=1：七个验证者密钥共用一个执行层（`N42_IMPORT_ONCE`，
文档自称 bench 拓扑），执行层可用 208 个 CPU；每块 200k 笔；压测器把每笔交易直接送到执行层本地 ingest（`F7_INGEST_ALL`），
单网关背书（不逐笔验签）；持久化每 8 块一次，账户历史关闭。同一主机（254 逻辑 CPU）。
它自己的其他拓扑：三节点独立执行 1.40–1.42M；七节点独立执行（E=7）对照约 68 万（一个窗口），每块约 14.7 万账户的真实形态 24–33 万（`docs/PLAN_E7_FLEET7.md` 第 1 节）。

### 我们的数字与可比性

| 拓扑 | n42-rs | n42-26（当前未提交构建） |
|---|---|---|
| 一个执行层、七个密钥 | 5.55 / 5.69M（30 s 窗口，200k 块） | 417,140（run144，单执行节点，95,238 块，约 4 s 突发窗口） |
| 七个执行层 | E=7 约 68 万；真实形态 24–33 万 | 335,479 / 336,737 / 363,644（全节点导入，约 4.5 s 突发窗口） |

窗口不同：我们一段只有 160 万笔，约 4 s 负载，不是持续 30 s；round BA（run146–148）补持续负载的 30 s / 20 s 窗口。

### 差距在哪里（一个执行层拓扑，每块）

| 项 | n42-rs | n42-26 | 说明 |
|---|---|---|---|
| 块大小 | 200k | 95k | n42-rs 自己测过：周期与块大小成正比，块大小本身不是杠杆 |
| 封块间隔 | 约 35 ms | 约 188 ms | 每千笔周期成本 0.175 ms 对约 1.97 ms，约 11 倍 |
| 执行 | par_exec 28.6 ms / 200k（32 线程） | 批次约 35 ms / 95k，打包合计约 70 ms | 每笔执行约 2.6 倍；n42-rs 有扁平账户表、读集预取、seeded 索引批读 |
| 状态根 | 与执行重叠（root ops 在执行批次内编码；root 门等父块） | 执行器收尾后才开始，等根 28 ms，收尾合计 55 ms | 延迟根在 n42-rs 是头部规则变化 |
| 封块后到下一次构块 | 4–5 ms（build on output / take sealed / 跳过自有 newPayload） | 约 60 ms（自有块准备 26、引擎插入 14、构建→校验 5 等） | 我们在自有块插入后才开始下一块 |
| 供给 | ingest 独立 runtime，64 连接，限速 9M | native-flood 8 连接，单节点约 80 万/s | 到 1M 级以上要先扩供给与重放集 |

结论：差距主要是结构性的——串行流水线（执行→根→插入→下一块）与每笔执行成本，而不是某个单项。

### 调整后的方案（按杠杆排序）

1. 测量对齐：持续负载 30 s 窗口（`window-tps30.py`），重放集按 TPS 放大；先于任何"接近 n42-rs"的说法。（进行中，round BA）
2. 在父块输出上构块（build on output）：下一块在本块封块时开始，读本块的内存执行输出；自有块插入、QMDB 提交、引擎工作与下一块并行。不改链规则。预计周期 188 → 约 130–147 ms。
3. 提前构建（build-ahead）：在本块打包结束时开始下一块的批次（仅帧顺序构块）。预计周期 → 约 max(打包, 收尾+衔接)。
4. 每笔执行成本：参照 n42-rs 的扁平账户表、读集预取。
5. 状态根离开封块路径：root ops 进入执行批次；延迟根与帧树交易根是头部 / 共识规则变化，需用户决定。
6. 之后再扩大块（200k）与供给（多连接、更大重放集）。
