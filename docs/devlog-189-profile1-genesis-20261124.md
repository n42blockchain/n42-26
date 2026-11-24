# devlog-189：混合生产标准 Profile 1 第一步——开关、创世与头布局（2026-11-24）

分支 `feat/profile1-genesis`（基于 origin/main 4831e77）。依据：n42-rs `docs/MIXED_PRODUCTION_STANDARD.md`
（v0.3 + AM1-AM23，修正案优先于正文）与 gov5 产出的向量 V1（头布局）、V8（创世键表与拒绝用例），
向量原样拷贝到 `crates/n42-chainspec/testdata/profile1/`。

## 1. 设计决策

- **开关只有一个**：创世 `config.n42Profile = 1`（P2）。没有该字段的创世一律走原路径，行为不变；
  字段存在但不是 JSON 数字 1（如 `2`、`"1"`、`1.0`）即拒绝（P4）。
- **在 `--chain` 解析处收口**：新增 `N42ChainSpecParser`（`bin/n42-node/src/chainspec_parser.rs`）取代
  `EthereumChainSpecParser`。带 `n42Profile` 的创世文件按原始 JSON 做封闭键表校验，并把 reth
  `ChainSpec` 的 `genesis_header` 替换为 Profile-1 头、用 Profile-1 哈希封存。这样 `init`、`node`、
  `db` 等所有子命令看到同一个标准创世哈希。考虑过只在 `node` 闭包里改 `chain.genesis_header`
  （gov5 interop 现有做法），但 `init` 不经过那里，会把错误哈希写进库。
- **封闭键表在原始 JSON 上校验**：alloy `ChainConfig` 反序列化后已知字段与未知字段混在不同位置、
  序列化回去还会补默认值，无法可靠判断"文件里写了什么"。因此 `config` 层键表只在解析器里对原始
  JSON 检查；`config.hotstuff` 整个对象 alloy 原样保留在 `extra_fields`，节点启动时可再次完整校验。
- **新头类型而非扩展 `Gov5NativeHeader`**：Profile 1 只有一种 24 元素布局（22 号为 `0x80` 空位、
  23 号 slotNumber、24 号 mobileRegistryRoot），而 `Gov5NativeHeader` 的旧布局把 mobile 根放在 23 号且
  所有尾部字段可选。合并会让两种布局在 23/24 元素处互相歧义；P1 要求旧布局在 Profile 1 上不可达，
  所以新建严格解码的 `Profile1Header`（缺字段、22 号非空、多余元素都报错）。
- **stateRoot 原因**：此前 n42-26 对 `genesis_8s.json` 得到 `0xac68dc35…`，是 reth `ChainSpec::from(genesis)`
  对 alloc 计算的以太坊 MPT 根；标准 A5/A6 要求 alloc 的 QMDB 根。n42-26 现有的
  `gov5_qmdb_genesis_tree`（gov5 `QMDBRootComputer.ComputeRoot` 的移植）本来就算得对，只是旧路径只在
  从 gov5 认证区间加载创世头时用于校验，从未用来构造创世头。现在把它挪到
  `n42_chainspec::profile1::qmdb_genesis_tree`（`n42-node` 原函数改为转调，单一实现），创世头直接用它，
  得到 `0x7eec60e5…`，与 V1 一致。测试里固定断言 reth 自身的头仍是 `0xac68dc35…`，作为原因的记录。
- **共识值只来自创世（P3）**：Profile 1 上 `ConsensusConfig` 由 `Profile1Config::consensus_config()`
  生成（验证者、f、超时、blockIntervalMs→slot、epochLength）；leader 规则为 `leaderTenure` +
  rotate-on-timeout 从 view 0 生效（A22），环境变量覆盖一律拒绝而非忽略。

## 2. 实施细节

| 位置 | 内容 |
|------|------|
| `crates/n42-chainspec/src/profile1/config.rs` | `Profile1Config`：V8 封闭键表常量（与 `allowed-keys.json` 由测试逐项比对）、分叉字段必须是 JSON 数字 0、`terminalTotalDifficultyPassed=true`、`consensus="hotstuff"`、`stateScheme="qmdb"`、tenure/epoch/超时/interval/gasLimit/maxBlockBytes ≥ 1、`maxTimeoutMs ≥ baseTimeoutMs`、`deferredExecutionDepth = 1`、`devBlockReward` 十进制字符串 + 整 gwei + ≤ u64::MAX gwei（AM8/AM12）、faucet 非零（AM16）、≥ 4 个验证者且地址/BLS 公钥不重复、committeePool 无 `enabled` |
| `crates/n42-chainspec/src/profile1/header.rs` | `Profile1Header` 24 元素编解码、`Profile1Extra`（`N42H‖view‖QC‖seal(96)`，≤ 4096）、`seal_hash`（AM4）、`seal`/`verify_seal`（H2-v4 POP DST） |
| `crates/n42-chainspec/src/profile1/mod.rs` | `profile1_genesis_header`（A6：ommers 0、requestsHash/withdrawalsRoot/tx/receipts 为空树根、22 号空位、slot 0、mobile 根 0）、A7 alloc 检查、创世顶层字段检查（extraData 空、gasLimit = hotstuff.gasLimit 等）、`profile1_chain_spec`、`install_n42_profile`/`n42_profile`（OnceLock，与 `install_leader_schedule` 同模式）、J4 禁用环境变量表与 `refuse_profile1_env` |
| `crates/n42-consensus/src/validator/selection.rs` | `LeaderSchedule::resolve_profile1`；`resolve_for_genesis` 在 Profile 1 上走它；`LeaderScheduleError::Profile1` |
| `crates/n42-consensus/src/committee_pool.rs` | Profile 1 上 committee pool 恒开（A19，无 `enabled`） |
| `crates/n42-consensus-service/src/profile1_header.rs` | `validate_profile1_header`（导入时：B1 extra/QC 解码/空 QC/视图/提交域法定人数/祖先、B2 封印、B3、B4 下界、B8、B9、B10、B13 上界、B14、A23）与 `check_profile1_vote_time`（B4 投票时上界）；QC 复用 `verify_h2_v4_sync_anchor` |
| `bin/n42-node/src/chainspec_parser.rs`, `main.rs` | `N42ChainSpecParser`；`install_genesis_profile`（解析、拒绝 J4 环境变量、安装 profile）；Profile 1 时共识配置取自创世 |

禁用的环境变量（Profile 1 上设置即拒绝启动）：`N42_CHAIN_ID`、`N42_INTEROP_GENESIS_HASH`、
`N42_CONSENSUS_CONFIG`、`N42_VALIDATOR_COUNT`、`N42_LEADER_TENURE`、`N42_HOTSTUFF_ROTATE_ON_TIMEOUT`、
`N42_BASE_TIMEOUT_MS`、`N42_MAX_TIMEOUT_MS`、`N42_BLOCK_INTERVAL_MS`、`N42_MAX_EMPTY_SKIPS`、
`N42_FRAME_BLOCKS`、`N42_FRAME_REF_BLOCKS`、`N42_SKIP_STATE_ROOT`、`N42_DEFER_STATE_ROOT`、
`N42_SKIP_TX_VERIFY`、`N42_DAILY_BASE_REWARD_GWEI`、`N42_MAX_REWARDS_PER_BLOCK`、
`N42_UNSTAKED_REWARD_RATIO`、`N42_SYNC_ANCHOR`，以及前缀 `N42_GOV5_*`（含 `N42_GOV5_LEGACY_SIGNING`、
`N42_GOV5_PRAGUE_TIME`、`N42_GOV5_QMDB_EXECUTION*`、`N42_GOV5_STATE_ROOT_TRUST`）和 `N42_REWARD_*`。

## 3. 测试与向量结果

- `n42-chainspec/tests/profile1_vectors.rs`：键表 = V8 `allowed-keys.json`；`genesis_8s.json` 的头逐字段、
  RLP、哈希均等于 V1（`0x6f733745…7e4f`）；V8 33 个拒绝用例全部拒绝；V8 基准创世（去掉
  `committeePool.enabled` 的那份）可加载；V1 区块 1、2 的 RLP 往返、hash、sealHash、封印验证
  （对应签名者通过、其他密钥失败、重签逐字节一致）；布局类拒绝（withdrawalsRoot 缺失、22 号非空）在解码时拒绝。
- `n42-consensus-service` `profile1_header`：V1 区块 1、2 通过；V1 全部 24 条 import/vote 拒绝均被拒；
  execution 类（体内奖励与 withdrawalsRoot 不符）不属头校验，另测区块 1、2 的 withdrawalsRoot 等于
  coinbase 与 faucet 各 1e18 的奖励根。
- `n42-consensus`：Profile 1 tenure + 从 view 0 起 rotate-on-timeout、覆盖被拒；Profile 1 pool 恒开。
- `bin/n42-node`：解析器对 V1 文件给出标准哈希，未知键被拒，`dev`/`mainnet`/无 profile 的创世哈希与
  `EthereumChainSpecParser` 相同。
- `n42-node init --chain .../V1/genesis_8s.json`（release，临时 datadir，已删除）打印 `Genesis block written hash=0x6f733745517957b421dcea37f6c879db89501aac0715f769fbc76ec54efe7e4f`，与 V1 一致。
- 门禁：`cargo clippy --workspace --all-targets -D warnings` 通过；`cargo test --workspace` 2004 通过、0 失败、38 忽略。

## 4. 未完成 / 未在 n42-26 生效的项

- `validate_profile1_header` 目前是库函数 + 向量测试，尚未接入节点的导入/投票路径（Profile 1 的出块、
  延迟执行、同步均不在本步范围）。B6 体内奖励检查、B13 延迟字段的值、AM19 投票门未做。
- reth 侧 hardfork 仍按创世字段推导（含 Amsterdam）；AM1 要求的"只启用 GLAMSTERDAM_EXECUTION_PARITY
  表中的执行变化"未做。
- AM2 的 8 MiB 构建/导入上限仍在；`maxBlockBytes` 只校验 ≥ 1，未接入（B15）。
- 不产出 Profile-1 封印头（出块不在本步范围）。

## 5. 后续计划

1. 把 `validate_profile1_header` 接入 Profile 1 的导入与 Round-1 投票前检查，并加 B6 体检查。
2. Profile-1 出块：`Profile1Header` 构建 + QC 嵌入 + `seal`。
3. Amsterdam 执行集合（AM1）、延迟执行 k=1（A10/B13）、AM2 去掉 8 MiB 上限。
4. 后续向量可参考 n42-rs 现成的测试骨架：`crates/n42/engine-types/src/profile1_vectors.rs`（V3/V4）与
   `crates/n42/h2-consensus/tests/profile1_v6.rs`（V6）；移植清单见 n42-rs `docs/PORT_PROFILE1_TO_N42_26.md`。
