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

## 6. AM24：requestsHash 改为 EIP-7685 的 sha256("")（2026-11-24 追加）

**设计决策。** 标准 AM24（owner 决定，n42-rs main，冻结文本 `docs/MIXED_PRODUCTION_STANDARD_V1_RC.md` v1.0-rc2）：
Profile 1 的每个头（含创世）`requestsHash = sha256("") = 0xe3b0c442…b855`，空 trie 根 `0x56e81f17…b421`
改为**拒绝**。原 AM3 的空 trie 根写法作废。

**实施。**
- `n42-chainspec/src/profile1/header.rs`：新常量 `PROFILE1_REQUESTS_HASH`（sha256("")），从 `profile1` 模块导出。
- `profile1_genesis_header`：创世 `requests_hash = Some(PROFILE1_REQUESTS_HASH)`。
- `n42-consensus-service/src/profile1_header.rs`：B8 只接受 `PROFILE1_REQUESTS_HASH`，错误信息带 `(B8, AM24)`；
  V1 拒绝用例 `requestsHash empty-trie root` 的测试断言它由 B8 拒绝。
- 测试数据换成 n42-rs origin/main（f6c2fd6aa）带 SHA-256 清单的版本，逐个核对清单一致：
  V1 取自 `crates/n42/h2-consensus/testdata/profile1/`（README `c69c8fc2…`、genesis_8s `effc8f8a…`（未变）、
  header_profile_v1 `4d5d78bb…`）；V8 取自 `crates/n42/qmdb-reth/testdata/profile1/gov5/V8/`（allowed-keys
  `d4994e7b…`（未变）、README `7a0d47d2…`、14 个拒绝创世）。
- 新创世哈希 `0x188e9d930cfd43317416b0f65e8f98240ae686407ba806b854e98428c3b490dc`，在
  `genesis_8s_loads_to_the_v1_genesis_header` 与 `chainspec_parser` 测试里显式断言。

**问题。** n42-rs 清单里的 V8 只有 14 个拒绝用例（gov5 feat/profile1 015bd9f6），是此前 qs-scratch 副本
33 个的子集（14 个文件逐字节相同）。按要求以清单为准替换，另外 19 个（extra_data、nonzero_fork、profile_2、
three_validators 等）从 testdata 删除；`every_v8_refused_genesis_is_refused` 改为恰好 14 个。代码中对应的拒绝
规则未删，仍生效。qs-scratch 目录仍是旧向量，不再使用。

**状态。** `n42-chainspec` profile1_vectors 11 个、`profile1_header` 3 个、`chainspec_parser` 3 个测试通过。

## 7. 第 2 步：导入路径的头校验、AM2 块大小、AM1 执行集合与 V3/V4（2026-11-24 追加）

### 7.1 设计决策

- **头校验放在解码处，而不是 reth 引擎里。** Profile 1 头有 24 个 RLP 元素，`Gov5NativeHeader` 最多 23 个，
  旧解码器根本读不出来。新增 `n42_network::decode_profile1_block_rlp` / `decode_block_rlp_for(rlp, profile1)`，
  Profile 1 上用 `Profile1Header::decode`，解码失败是永久错误（`HeaderProfile`）。
  校验状态集中在 `Profile1ImportGate`（`n42-consensus-service/src/profile1_header.rs`）：记住已接纳的头（4096 个，
  父必须已接纳；祖先回溯深度 256），记住拒绝；拒绝原因带规则号，打 `n42_profile1_header_refused_total{path,rule}`
  指标和 `n42::profile1` 告警。未在 Profile 1 的链 `profile1 = None`，路径与原来一致。
- **Round-1 投票门**：H2-v4 参与者模式是导入后投票，投票以 `EngineOutput::SendToValidator(Vote)` 或广播发出；
  在 `handle_engine_output` 入口对 Vote 调 `check_vote`（即 `check_profile1_vote_time`），未到时间则不发。
- **AM2**：块大小唯一上界是 `hotstuff.maxBlockBytes`。`block_rlp_size_limit` 只在 Profile 1 时返回 maxBlockBytes，
  否则保持 Osaka 的 8 MiB；适配器在 Profile 1 时自己比较 `rlp_length`，并不再让 reth 内层做 8 MiB 检查。
- **AM1**：reth 的 `EthereumHardfork::Amsterdam` 从 Profile 1 链规格中移除（`profile1_chain_spec`），
  在 `N42EvmConfig` 里把 evm2 的 spec/version 提到 `AMSTERDAM`，并设 `slot_num`（= 块号，EIP-7843）。
  这样执行集合跟 n42-rs `GLAMSTERDAM_EXECUTION_PARITY.md` 一致，而只受链规格 Amsterdam 控制的东西保持关闭。
- **写集合规则（A9/D3/D5）**：V3 首次运行发现两类差异：每块多写 `0xff..fe` 系统调用者叶子（D3），
  `a9-no-op` 多写零值转账接收者的账户叶子（A9）。新增 `qmdb_state::gov5_block_operations`，Profile 1 下：
  只写前后值不同的键；有存储操作的账户保留账户操作（V3 README 开放点 1，gov5 现状）；不加 restored-slot 重写（D5）；
  不加系统调用者叶子（D3）。非 Profile 链仍是原 replay-v2 规则。根任务、构建器（native/parallel）、
  `exec_cache` 统一走这个函数。

### 7.2 Amsterdam 下各 EIP 的开关

| EIP | 由谁开 | Profile 1 上 |
|-----|--------|-------------|
| evm2 AMSTERDAM 版本：gas 表、EIP-8037、2^32-1 交易 gas 上限、EIP-2780、7623/7976、7981、8038、7708 转账日志、7778、7954 代码/initcode 上限、8246、8024、7843 SLOTNUM | evm env 的 spec/version | 开（AM1），由 `amsterdamTime` 提升 |
| EIP-7928 BAL | reth 链规格 Amsterdam（构建器 `with_bal_builder_if`、payload 校验、头校验） | 关：链规格去掉 Amsterdam |
| EIP-8282 builder deposit/exit 块末调用 | **evm2 按 env `spec_id.enables(AMSTERDAM)`**（`block/ethereum/execution.rs:422`），不看链规格 | **仍会执行**；两个合约地址无代码，调用成功且不改状态，V3 结果相等；要真正关掉需改 evm2，按要求停止并上报 |
| EIP-7997 | 未实现 | 无 |

### 7.3 实施细节（文件）

- `n42-network/src/gov5_block.rs`、`lib.rs`、`Cargo.toml`：Profile 1 解码。
- `n42-consensus-service`：`profile1_header.rs`（门、拒绝、日志）、`orchestrator/{mod,execution_bridge,range_sync,consensus_loop}.rs`、
  `observer.rs`、`orchestrator/tests/profile1.rs`。
- `bin/n42-node/src/main.rs`：`profile1_import_gate` 构建门并接入 observer 和 orchestrator；安装 `install_profile1_config`。
- `n42-chainspec/src/profile1/mod.rs`：`install_profile1_config`、`block_rlp_size_limit`、去掉 reth Amsterdam、
  `check_profile1_tx_type`（C1-C4）。
- `n42-consensus/src/adapter.rs`、`n42-node/src/{native_payload,parallel_payload}.rs`：AM2。
- `n42-execution/src/evm_config.rs`、`lib.rs`：AM1 提升。
- `n42-node/src/{qmdb_state,qmdb_state_root,exec_cache,parallel_payload}.rs`：写集合规则。
- `n42-node/tests/profile1_vectors.rs` + `n42-node/testdata/profile1/gov5/`（n42-rs origin/main 向量逐字节拷贝，SHA-256 与清单一致）。

### 7.4 结果

- 导入路径：follower 导入（`handle_block_data`、gov5 体 `process_h2_v4_gov5_block`）、sync 导入（`import_and_notify`）、
  catch-up（反向回溯与 range，`import_h2_v4_catchup_block`、range 解码）、observer（finalized catch-up、live import、
  `import_block_data`）都先过门；V1 全部 23 个导入拒绝在各路径都被拒，不调 new_payload，不投票。
- V3：genesis QMDB 根相等；`main.json` 7 块、`system_address_live.json` 2 块，写集合与根全部相等。
- V4：17 个用例裁决全部相等。阶段差别两处（裁决相同）：`type-4-create` gov5 在 includable 拒，本节点 alloy 解码即拒；
  `type-0x50` gov5 解码拒，本节点解码通过、由类型规则 C4 拒。
- clippy `--workspace --all-targets -D warnings` 通过；`cargo test --workspace` 2018 通过、0 失败。

### 7.5 未完成

1. Profile 1 块走 Engine API 时 payload 重建失败（`MissingRequests`：payload 不带 requestsHash/slot），引擎校验器
   `bind_gov5_h2_header` 也只认 23 元素头；导入门之后的 EL 绑定尚未做。reth 头校验对非 Amsterdam 的 `slot_number`
   也会报 `SlotNumberUnexpected`，Profile 1 头需绕过这条或由 N42 共识层替代。
2. C1-C4 类型规则只在向量测试里使用，尚未接入交易池与块导入（`import alone` 时 blob 交易会被执行接受）。
3. EIP-8282 块末调用需 evm2 改动才能关（见 7.2）。
4. 重启时门只在“零注册根重建的 24 元素头哈希 = 持久头哈希”时信任持久头。
5. V8 用 n42-rs 的 14 个拒绝用例（原 33 个中多余的 19 个已删）。
6. 出块、延迟执行（AM19）、V6 不在本步范围。
