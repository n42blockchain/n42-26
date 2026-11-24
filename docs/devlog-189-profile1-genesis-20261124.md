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

## 8. 第 3 步：Profile-1 块完整走通 reth 执行（2026-11-24 追加）

本步头里仍是**本块自己的**执行结果（未做延迟执行）。延迟执行按
`deferred-execution-design.md` 后续做；本步新增的出块封印函数是唯一写四个执行字段
（stateRoot、receiptsRoot、logsBloom、gasUsed）的地方，延迟执行只需改这一处，不受阻碍。

### 8.1 设计决策

- **不新增 `N42HeaderProfile` 变体。** Profile-1 节点用 `Gov5H2` 头配置；在 Gov5H2 的各分支里用
  “链 genesis 声明了 `n42Profile` 且头带 `slotNumber`”识别 Profile-1 头（旧 gov5 布局都没有 slotNumber）。
  这样复用现有 gov5 原生头的全部机制（原生头登记表、按哈希绑定、gov5 奖励根、委员会证据链），改动最小。
- **Engine API 载荷。** Profile-1 块的载荷是 V3 载荷，sidecar 带 Prague 的 requestsHash（AM24，等于 alloy 的
  `EMPTY_REQUESTS_HASH`）；载荷里没有 slotNumber 和 mobileRegistryRoot。引擎校验器
  `bind_gov5_h2_header` 在其它布局都不匹配时调用 `bind_profile1_header`：按标准补齐 difficulty=0、
  ommersHash=0、slotNumber=number、withdrawalsRoot=gov5 奖励根，mobileRegistryRoot 取登记表里该哈希的头
  （没有则为 0），只有 24 元素编码的哈希等于载荷哈希才接受。哈希相等即证明每个字段都是头里的。
  `MissingRequests` 由 `replay_import::build_profile1_execution_data` 解决（旧的 v1 输入检查不再用于 Profile-1 头）。
- **与 reth 头检查的冲突，全部在 N42Consensus 里处理，不改 reth：**
  - `SlotNumberUnexpected`：reth 的 Amsterdam 已从链规格移除，而头带 slotNumber。
    `normalize_header` 在交给内层 Ethereum 规则的临时副本里去掉 slotNumber；slotNumber 由块哈希认证，并由导入门检查（= number）。
  - 时间戳：reth 要求子块严格晚于父块，Profile 1（B4）允许相等。`validate_header_against_parent` 先检查 `time ≥ parent.time`，
    相等时把临时副本加 1 秒；内层其余规则（gasLimit、baseFee、同一分叉的 blob 参数）都不读时间戳。
  - requestsHash：Prague 下 reth 比较执行产生的 requests 哈希；空请求的哈希就是 sha256("")，与 AM24 一致，无需改动。
  - BAL：链规格没有 Amsterdam，执行不建 BAL，头里也没有 BAL 哈希，一致。
- **C1-C4。**
  - 交易池：Profile 1 上 `no_eip4844()`（拒 blob）；0x50 因 Profile 1 没有 `altSigTx` 本来就不收；0x05 及以上的类型解码就失败。
  - 块导入：`N42Consensus::validate_block_pre_execution` 在 Profile 1 链上逐笔检查类型（`check_profile1_tx_type`），
    所有导入路径都经过这里。

### 8.2 实施细节

- `n42-consensus`：`remembered_profile1_header(hash)`（登记表中按哈希取 24 元素头并复核哈希）；adapter 的 Profile-1 处理和 C1-C4 检查。
- `n42-network/gov5_block.rs`：
  - `seal_built_payload_for_profile1`：出块封印。只改不进入执行的头字段，prevRandao 不为 0 或 requestsHash 不对时拒绝；
    嵌入最后的 CommitQC（须为祖先且 view 更低），用 leader 的 BLS 密钥封印（B2），登记原生头，返回新哈希的载荷。
  - `encode_profile1_block_rlp`、`encode_block_wire`；`encode_gov5_block_rlp` 遇到 Profile-1 哈希时用登记的 24 元素头编码 gov5 线格式。
  - `header_qc_wire` 的祖先回溯同时认 Profile-1 头。
- `n42-consensus-service`：
  - `replay_import::build_profile1_execution_data`。
  - `Profile1ImportGate::with_seal_key` / `seal_key`。
  - `handle_built_payload` 在 Profile 1 上走封印分支；出块的 rekey 仍由 `gov5_normalization_preserves_execution_input` 守护。
  - Profile 1 的 payload attributes 中 prevRandao 为 0（B3）。
- `n42-node`：`engine_validator::bind_profile1_header`；`pool.rs` 的 C2。
- `bin/n42-node/src/main.rs`（V6 合并前提）：
  - Profile 1 自动成为 H2-v4 参与者，用 Gov5H2 头配置；H2-v4 身份为 genesis 的链 id 与哈希。
  - QMDB 执行基从 genesis alloc 建（`load_profile1_execution_base`，根须等于 Profile-1 genesis 头）。
  - A8 奖励来源由 genesis 构建（`Gov5WithdrawalSource::from_genesis`）。
  - 导入门带本节点封印密钥；genesis 与持久头的原生编码写入登记表。
  - 以上都不读任何 N42_GOV5_* 环境变量。
- `PROFILE1_FORBIDDEN_ENV` 增加 `N42_COLOCATED_VOTE`。

### 8.3 测试

- `crates/n42-node/tests/profile1_import.rs`：
  - `a_profile1_chain_imports_through_reth_and_becomes_canonical`：genesis_8s（加 V3 的出资账户）→ 区块 1（V3 plain-transfer 交易）
    → 区块 2（V3 contract-create 交易）。每块按 leader 的方式产生：在引擎的父状态上执行，算 QMDB 根和原生收据根，
    用 V1 测试密钥里该 view 的 leader 封印。之后依次经过：gov5 线编码（与 `encode_gov5_block_rlp` 字节一致）、
    Profile-1 解码、导入门、`build_gov5_gossip_execution_data`（与 leader 载荷一致）、reth 的 `BasicEngineValidator`
    （N42EngineValidator + N42Consensus(Gov5H2) + QMDB 状态根策略）。结果：
    - 两块都是 Valid，QMDB store 记录的根等于独立计算的根；
    - QMDB 根被篡改的同一块被拒（BodyStateRootDiff），且不提交任何东西；
    - 每块按 FCU 的做法成为 canonical head，最后持久化（`save_blocks`）后按 Profile-1 哈希能读回头，头里 slot = number。
  - `profile1_block_import_refuses_transaction_types_outside_c1_c4`：V4 的 blob（C2）与 0x50（C4）在执行前被拒，其余合法类型通过类型检查。
- `orchestrator/tests/profile1.rs` 的 catch-up 测试改为：V1 区块 1、2 通过门后进入 new_payload（以前停在 MissingRequests）。

- `profile1_header::tests::own_sealed_parent_is_admitted_only_under_the_own_key`：leader 自己封印的父块只在本节点密钥验封通过时进入门。

### 8.4 本地四节点出块（真实 n42-node 进程）

用 V1 的 `genesis_8s.json` 与 `header_profile_v1.json` 的四把 BLS 测试密钥起四个 debug 构建的 n42-node
（本机回环，`N42_ALLOW_DETERMINISTIC_P2P=1`，不设任何 Profile 1 禁用的变量）。
四个验证者 quorum 为 3，单节点无法定案，因此直接跑四节点。

**遇到的问题及解决（按出现顺序）：**

1. **委员会证据根取不到父块。** `committee_parent_beacon_root` 只按 gov5 布局解码父块，
   而 Profile-1 genesis 与块是 24 元素头。改为取不到时用 `remembered_profile1_header`
   （number 与 receipts_root 和 follower 的 `verify_parent_link` 读的是同一份）。
2. **A23 gasLimit。** reth 构建器按 1/1024 步长把 gasLimit 拉向 `--builder.gaslimit` 默认值
   （480000000 → 479531251），follower 按 A23 拒块。Profile 1 上构建器的期望 gasLimit 固定为 genesis gasLimit（payload.rs）。
3. **serve cache 认不出 Profile-1 块**（“refusing to retain malformed local Gov5 block”）。`attributable_hash` 同时接受 24 元素头；
   新增 `decode_wire_block_rlp`（gov5 或 Profile-1），serve cache 的三处校验改用它。
4. **V1 genesis 时间戳在未来。** `genesis_8s.json` 的 timestamp 为 1795564800（2026-11-25 00:00 UTC），
   B4 正确拒绝了超前于本地时钟的投票。本地运行用一份把 timestamp 改为当前时间的副本（只改变 genesis 哈希，密钥与验证者不变）。
5. **leader 自己的块不在自己的门里。** 门只认导入路径上的头，leader 自己封印的块从不经过导入路径，
   所以它的子块报 `parent_unknown`，leader 也不能为自己的块投 Round-1 票。新增 `Profile1ImportGate::admit_own_sealed`：
   从原生头登记表取出该头，**只有封印用本节点密钥验证通过**才按普通规则走一遍 admit；`admit` 遇到未知父块时、
   `profile1_vote_allowed` 投票前各调用一次。
6. **没有 8 秒 slot 节奏。** 编排器的 slot 时间只读 `N42_BLOCK_INTERVAL_MS`，该变量在 Profile 1 上被禁止，
   于是编排器不按 slot 出块，一次连出约 4 块，时间戳逐块 +1 超前，触发 B4，视图超时 24 秒。
   改为 Profile 1 上取已安装的 `Profile1Config::block_interval_ms`。

**结果（修复后）：**

- 四节点按 8 秒 slot 出块，leader 轮换（tenure 1），每块 finalized = latest；运行到 15 块无任何拒绝或错误。
- 头字段：mixHash 0、gasLimit 480000000、extraData 259 字节（N42H + view + CommitQC + 封印；区块 1 无 QC 为 108 字节）、
  2 笔 A8 奖励提款、parentBeaconRoot 为委员会证据根、requestsHash = sha256("")。
- 用 V3 的出资账户和 10 笔预签名交易（chain id 1144）：全部进块（区块 3-12，status 1，合约创建在 `0x9ed5…`），
  四节点块哈希与状态根一致。

### 8.5 阶段完成状态

| 项 | 状态 |
|---|---|
| 24 元素头绑定到 reth 载荷与引擎路径 | 完成（`bind_profile1_header`） |
| MissingRequests 重建 | 完成（`build_profile1_execution_data`） |
| SlotNumberUnexpected、等时间戳 | 完成，在 N42Consensus 内处理，未改 reth |
| 进程内导入测试 genesis → 1 → 2（Valid、canonical、持久化） | 完成 |
| leader 出块封印、本节点验证者接受 | 完成，四节点真实进程出块并定案 |
| C1-C4 进交易池和块导入 | 完成 |
| V6 前提：H2-v4 参与者由 profile 推出；`N42_COLOCATED_VOTE` 禁用 | 完成 |

### 8.6 后续计划 / 受阻

- 延迟执行（`deferred-execution-design.md`）：唯一写执行字段的地方是 `seal_built_payload_for_profile1`，从这里改。
- mobileRegistryRoot 目前恒为 0（登记表没有该头时绑定也按 0）。
- EIP-8282 不改 evm2/reth，由用户决定。
- V1 `genesis_8s.json` 的时间戳在 2026-11-25 00:00 UTC 之前无法出块（B4），这是测试数据本身的属性。
- V6 合并后需复核：`VoteRules::installed()` 下 Round 1 等 `BlockImportedWithParent`，leader 自己的块需要发出该事件。
