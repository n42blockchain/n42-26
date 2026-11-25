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
## V6：共识消息（投票、QC、TC、NewView、签名域）

向量来源：n42-rs `origin/main`（AM24 之后重新生成的版本）
`crates/n42/h2-consensus/testdata/profile1/gov5/V6/`，逐字节复制到
`crates/n42-chainspec/testdata/profile1/V6/`；SHA-256 与 n42-rs `gov5/README.md` 清单一致
（`consensus_messages_v6.json` `9267eb25…504a`，`README.md` `5d558cb0…af38`），测试里固定校验。
没有使用 `/data/blockchain/wr-logs/qs-scratch/profile1/vectors`（旧版）。V6 的链身份是
chain id 1144、genesis `0x188e9d93…90dc`（AM24 之后的 V1）；本分支 V1 目录仍是旧哈希 `0x6f73…7e4f`，
V6 自带身份，不依赖它。

### 设计决策

- 线格式/签名域不用改：n42-26 的 `n42-network::{h2_wire,h2_v4,h2_bridge}` 与 n42-rs `n42-h2-wire`
  同源（只差 snappy 走 `snappy_pool`，输出逐字节相同），`n42-primitives` 的 H2-v4 域（E10：
  `"N42H2V4" || phase || chainId LE || genesisHash || view LE || payload`）、POP DST 与 gov5 相同。
- 差异都在引擎的投票门上。移植 n42-rs 的 `VoteRules` 思路（`vote_rules.rs`），只保留 n42-26 有的规则：
  `VoteRules::TODAY`（默认，行为不变）/`VoteRules::PROFILE1`。引擎构造时由
  `n42_chainspec::profile1::n42_profile()`（第一步装入的进程级 profile）决定，测试用 `set_vote_rules`。
  这样无需改编排器即可在 Profile 1 节点上生效。
- 事件对应关系：n42-rs 有 `header_known` + `BlockChecked`（已检查）+ `BlockImported`（已执行）三级；
  n42-26 的 H2 路径只有一级 `BlockImportedWithParent`（reth 对该 payload 返回 Valid 后发出，即本节点已执行），
  比 n42-rs 的“检查”更强。所以 Round 1 与 Round 2 都以它为准；裸 `BlockImported`（只缓存了块数据，
  或 `N42_COLOCATED_VOTE` 路线）在 Profile 1 上不算证据。考虑过把 n42-rs 的 checked/executed 两级整体移植，
  但 n42-26 没有延迟执行（A10，步骤 3b），现在引入 `BlockChecked` 无人发送，放到 3b 再做。
- A14：Profile 1 上只能用 H2-v4 链绑定签名。Native/Gov5Legacy 的代码保留给其他链；Profile 1 引擎若停在
  这两个 profile，`process_event`、`process_authenticated_message`、`on_timeout` 一律拒绝
  （`SigningProfileUnreachable`），不签名也不验签。
- 伪造 QC 的视图跳转：n42-26 原来把“携带验不过的 QC 的未来视图消息”缓存起来（等到该视图再拒），
  n42-rs 直接拒。Profile 1 上改为直接返回 QC 错误（集合已知时），其他链不变。

### 实施细节

- 新文件 `crates/n42-consensus/src/vote_rules.rs`：`VoteRules { profile1 }`，`installed()`、
  `round1_needs_execution`、`unknown_parent_abstains`、`commit_vote_needs_own_execution`、
  `signing_profile_allowed`；`lib.rs` 导出 `VoteRules`。
- `state_machine.rs`：字段 `vote_rules`、`held_prepare_qc`；`vote_rules()`/`set_vote_rules()`；
  `check_signing_profile()`（A14）；`try_qc_view_jump` 在 Profile 1 上对伪造 QC 返回错误；
  `advance_to_view` 清掉过期的 held PrepareQC；`propose_add_validator`/`propose_remove_validator`
  在 Profile 1 上拒绝（AM5，原来会在 leader 出块时报 `H2V4ValidatorChangesUnsupported` 卡住每个 leader 视图）。
- `proposal.rs`：Round 1 在 Profile 1 上总是执行门控；`on_block_imported` 在 Profile 1 上忽略不带父哈希的导入；
  `extends_justify` 在 Profile 1 上对未知父块（或零父块）弃权（n42-rs audit A-2），其他链仍放行；
  `process_prepare_qc` 在 Profile 1 上先更新锁，再在本节点未执行该块时把 PrepareQC 挂起，
  `on_block_imported` 执行到该块时补发 CommitVote（移植 n42-rs 的 held PrepareQC 机制）。
- `timeout.rs`：`on_timeout` 先做 A14 检查。`error.rs`：新增 `SigningProfileUnreachable`。
- 测试 `crates/n42-consensus-service/tests/profile1_v6.rs`（移植自 n42-rs `tests/profile1_v6.rs`，改名对应：
  `wire_bridge::to_engine/to_wire` → `consensus_from_h2_v4/consensus_to_h2_v4`，
  `header_known+BlockChecked+BlockImported` → `BlockImportedWithParent`，`set_leader_tenure` →
  `set_leader_schedule{tenure 1, rotate 从 view 0}`）。放在 consensus-service 是因为它同时依赖
  chainspec、consensus、network，不用改 Cargo.toml。

### 结果

8 条消息（proposal_v2、prepare_vote_v2、prepare_qc_v2、commit_vote_v2、decide_v2、timeout_v10800、
newview_tc_epoch_boundary、proposal_after_tc）：签名原像、签名（用向量测试密钥重签逐字节相同、验签通过）、
SSZ wire（解码/重编码）、H2-v4 信封、snappy gossip 全部相等；经 `h2_bridge` 转成引擎类型再转回仍是同一信封；
引擎自己的 `ConsensusSigningProfile::*_message` 原像也相等；所有 QC/TC 在 H2-v4 域下对创世集合验证通过。
尺寸：n = 4/100/512 的 QC、Decide、NewView、信封、gossip 尺寸相等，gov5 的 100 成员 Decide 与 NewView
（TC + highQC + leader 签名）对其 100 把公钥验证通过。

19 个判定用例全部与 gov5 一致。改动前（`VoteRules::TODAY`）有 4 组不一致：
- `round1_gate`：裸 `BlockImported`（只有块数据）就投 Round 1——H2 编排器只在 colocated 路线发它，但引擎本身不拦；
- `round1_refused_block`：同上，未执行的块也会得票；
- `round2_gate`：收到 PrepareQC 就发 CommitVote，不等本节点执行（A13/AM19 不符）；
- `proposal_justify_foreign_set`：引擎在 view 1 收到 view 2 提案，justify QC 伪造时被静默缓存（返回 Ok），而非拒绝。
改动后全部一致。`decide_wrong_width` 的拒绝点不同：n42-rs 在 wire→engine 桥按集合宽度拒，n42-26 的桥不知道集合大小，
由引擎 `collect_signer_keys` 拒（“signer bitmap length 5 does not match authorized validator set length 4”），结论相同。
错误文本：wrong_proposer、vote_domain_signature、foreign_set 类、tc_foreign_set 与 gov5 文本一致或同义；
`tc_wrong_next_view` 为 “NewView for view 10802 carries a TC for view 10800, which leads to view 10801”。

另加两条 Profile 1 测试：A14（Native/Gov5Legacy 引擎拒绝事件与超时）、AM5（增删验证者被拒）。

### 门禁

`cargo clippy --workspace --all-targets -- -D warnings` 通过；`cargo test --workspace` 2019 通过、0 失败、38 忽略
（第一步为 2004；新增 V6 测试 14 条、`vote_rules` 单测 1 条）。

### 仍需节点/编排器改动（本步未做）

- **Profile 1 节点还没有走 H2-v4 参与者路径**：`bin/n42-node/src/main.rs` 只在 `N42_GOV5_H2_PARTICIPANT=1`
  时调用 `with_h2_v4_participant(identity)`，而 `N42_GOV5_*` 在 Profile 1 上是禁用环境变量。加了 A14 检查后，
  Profile 1 节点若仍是 Native 签名会在第一个事件就拒绝（故意 fail closed）。需要在 Profile 1 上以
  `(chain id, 创世哈希)` 自动启用 H2-v4 参与者（这与另一 worktree 的头导入接线重叠，留给那边或下一步）。
- `N42_COLOCATED_VOTE` 在 Profile 1 上应列入禁用环境变量（引擎已不把它的裸导入当证据）。
- E16 投票传输、G5 64 KiB 上限、gov5 的 `high_tc` 捎带、n42-rs 的 checked/executed 两级（延迟执行 3b）未做。

## 9. 延迟执行（深度 1）：设计 PR 1-4（2026-11-24 追加）

依据 `.artifacts/profile1-reply/deferred-execution-design.md`。区块 N 的头携带 N−1 的执行结果，共五个字段
（A10 + AM25）：stateRoot（QMDB 根）、receiptsRoot（B7 原生收据根）、logsBloom、gasUsed（执行结果的
`gas_used`，即 EIP-8037 的 `max(regular, state)`，不是收据累计）、requestsHash（N−1 的 EIP-7685 请求哈希，
无请求为 sha256("")）。区块 1 携带 genesis 自身的字段。Round 1 与 CommitVote 的执行门（Phase A）沿用 V6，
Phase B、可包含性（includability）、E14 标签、E12 停机不在本次范围。

### 9.1 设计决策

- **PR 1：`deferred_depth.rs` 逐字移植**自 n42-rs `h2-consensus`（origin/main e2a890f77），只在模块文档末尾加了出处说明；
  六个原测试一并带入。`VoteRules` 只加 `deferred_depth()`（Profile 1 为 `Some(1)`）。
- **PR 2：执行结果登记表放在 `n42-consensus`，不在设计写的 `n42-node`。** 原因是依赖方向：`n42-node` 依赖
  `n42-consensus-service`，而 leader 的封印在 `n42-network`、出块在 `n42-consensus-service`，都够不到 `n42-node`；
  `n42-consensus` 在三者之下（与原生头登记表同处）。
  - 核心是不含全局状态的 `ExecutedFieldsRegistry`（便于测试），外加一个进程级实例供调用方使用。
  - 持久化：`executed_fields.log`，与 QMDB 分支文件同目录。40 字节文件头（魔数 + genesis 哈希，防止串链），
    之后是定长 400 字节记录（392 字节正文 + blake3 前 8 字节校验）。每次 `insert` 追加并 `sync_data` 后才返回，
    因此早于该块的第一次 FCU。
  - 打开时，在第一条不完整或校验失败的记录处截断（崩溃时写了半条）。超过 4096 条压缩到最新 1024 条（临时文件 + rename）。
  - 同一哈希再次登记相同结果时不做任何事；结果不同则拒绝并保留第一条（`Conflict`）。这条规则让 leader 登记的结果与
    本节点引擎导入时算出的结果互相校验。
  - 没有移植 n42-rs 的 `fields_from_descendants`（设计 3.8：投票节点不从别人的头取结果）。
- **PR 3：不改 reth。** 设计 §4 的两项都核实过：
  1. 状态根任务返回的 `ProviderError` 不是 validation error，`cached_refusal` 不会把它记成无效块；
  2. 唯一不经验证插入的路径是 executed-block inbox，只用于本节点自己的块，且只在 `N42_LEADER_INSERT_EXECUTED=1`
     时启用。该路径只在头里的根是本块自己的根时才提交 QMDB，Profile 1 上永远不成立，会回落到 newPayload。
     这个开关与 `N42_QMDB_ADOPT_BUILT` 现在都列入 Profile 1 禁用环境变量。cache-skip 与 native-import 补丁不跳过验证。
- **AM25 改变了静态规则。** requestsHash 不再要求等于 sha256("")：导入门（B8）、replay 构造、leader 封印都只要求存在，
  值由延迟检查比对（与 n42-rs 一致）。V1 的两条 requestsHash 拒绝用例因此不再由导入门拒绝，而是由延迟检查拒绝；
  相关测试改为说明这一点（门的拒绝数由 23 改为 21）。

### 9.2 实施细节

- **QMDB 状态根任务**（`Gov5QmdbStateRootJob::finish_deferred`，Profile-1 头且链 genesis 声明 profile 时启用）：
  1. 取 registry(parent)，没有则返回 `ProviderError`（可重试，reth 不记忆）；
  2. 用 `deferred_depth::expected_fields` 求期望值，与头携带的五个字段比较；不一致时不提交任何东西，记录是哪个字段不同，
     并返回一个不等于头的根，reth 走 BodyStateRootDiff 判 Invalid；
  3. 一致时用新方法 `compute_and_commit_own` 按本块操作算出的根提交（`commit_operations` 的期望根改为 `Option`；
     无期望根时不做根比较，也不走以头的根标记的重叠读视图）；
  4. 把本块自己的五个字段（QMDB 根、原生收据根、bloom、`result.gas_used`、`result.requests.requests_hash()`）
     登记到本块哈希下，返回头的根。
  adopt-built 路径在 Profile 1 上不走（开关被禁用）。
- **`N42Consensus::validate_block_post_execution`** 的 Profile-1 分支：交给内层以太坊规则的头副本中，receiptsRoot、
  logsBloom、gasUsed、requestsHash 换成本块自己的值，内层对这几个字段的自我比较变成恒真，其余检查照常。
- **启动**：Profile 1 节点安装 `executed_fields.log` 并登记 genesis 自身字段；共识头高于 0 而日志里没有其结果则拒绝启动。
- **PR 4：leader 出块。**
  - `seal_built_payload_for_profile1(execution, view, qc, carried, registry_root, key)` 写入父块的五个字段，
    同时改载荷：V1 字段和 sidecar 的 requests hash。
  - `profile1_built_own_fields` 从构建器载荷读出本块自己的 bloom、gas、requests hash，加上 QMDB 根和原生收据根。
  - `handle_built_payload`：
    - 父块结果最多等 2 秒（10 ms 轮询）。构建器只能在已执行的父块上出块，正常情况下结果已登记；等不到则放弃本次出块，
      不猜也不填零。
    - 封印后立即把本块自己的结果登记到封印哈希下（子块的出块会在这里找）。
    - rekey 用放宽后的守卫 `profile1_seal_preserves_execution_input`：不比较 gasUsed、logsBloom、requests hash，
      它们不是 EVM 输入。

### 9.3 测试

- PR 1：`deferred_depth` 的六个移植测试；`vote_rules` 测试覆盖 `deferred_depth()`。
- PR 2：`executed_fields` 的 8 个测试：插入、读取与冲突；内存淘汰；五个字段的差异命名；重启后读回；
  半条记录和翻转字节时截断；他链日志与非日志文件被拒绝；压缩；V1 genesis 条目与跨线程等待。
- PR 3：`crates/n42-node/tests/profile1_deferred.rs`（gov5 的 V5，已复制到 `testdata/profile1/gov5/V5/`），经 reth
  引擎（含委员会池）：
  - (c) genesis 结果未登记时区块 1 得到 provider error，不提交、不记为无效；登记后同一块 Valid。
  - (a) V5 六个块全部 Valid，本节点登记的结果与 gov5 的 `executed` 完全相同（state/receipts/bloom/gas）。
  - (b) 区块 2 五个携带字段任改一个并重新封印，都是 Invalid（BodyStateRootDiff）；V5 三条 `deferred` 拒绝用例 Invalid；
    `import` 用例被导入门拒绝。
  - (d) 区块 1、3 是 state-gas 块：登记的 gasUsed = max(regular, state)，不等于收据累计（293760、183600）。
  - (e) 兄弟链：6'（区块 6 换时间）与 6 都携带 result(5)，但执行结果不同；7' 在 6' 上携带 result(6) 被拒，
    携带 result(6') 为 Valid。
- PR 4：`profile1_import.rs` 改为三块（plain-transfer、contract-create、storage-set）。每块：
  - 封印写入父块结果，leader 在封印哈希下登记自己的结果；
  - 守卫对封印为真，对改了时间戳的载荷为假；
  - 引擎导入时的结果与 leader 登记的相同（不同就会冲突而导入失败）；
  - 头携带父块结果。

### 9.4 四节点运行（`/tmp/p1-run/run4.sh`，genesis_now.json 时间戳更新并加入 V3 出资账户）

- 四个节点按 8 秒出块。检查脚本 `/tmp/p1-run/check_deferred.py` 对每个块验证：四个节点哈希一致，且区块 N 携带的
  五个字段等于每个节点 `executed_fields.log` 里为 N−1 登记的结果。结果：区块 1-18 全部一致，0 处不符。
- 第 6-15 块各含一笔 V3 交易（10 笔全部 status 1）；区块 7 携带 183600（区块 6 转账到新账户的 state gas），
  区块 8 携带 293760（区块 7 建合约），与 V5 的同类数值相同。
- 日志：ERROR 0、拒绝 0、缺父块结果 0；leader 自己的导入命中缓存执行（cache hit 4-5 次/节点）。
- 重启节点 0：读回 21 条结果，通过头检查，重新加入；链继续到 26 块，0 处不符。

### 9.5 后续

- PR 5：Round-1 门 Phase A 的剩余部分（BlockChecked/BlockRejected、B4 进门、已知父块、持有的 CommitVote）。
- PR 6：可包含性与 Phase B。
- PR 7：E14 标签（目前 latest = safe = finalized）、日志与 QMDB WAL 之间崩溃的恢复、E12 停机。
- PR 8：延迟链上的同步与观察者。
- 设计 3.10：手机端 receiptsRoot/stateRoot 的语义已变（N 的值在 N+1 的头里），Profile 1 上需关闭手机收据验证或改为对 N+1 证明。
- V5 的 AM25 请求块（一个提款请求和一个合并请求）gov5 发布后加入 PR 3 测试。

## 10. 延迟执行：PR 7（E14、E12、重启）、PR 6（可包含性与 Phase B）、PR 8（同步与观察者）（2026-11-24 追加）

顺序按协调要求：先 PR 7（较便宜的正确性项），再 PR 6，最后 PR 8。PR 5 视为已由 V6 Phase A 与 PR 4 完成。

### 10.1 PR 7：E14 标签、E12 停机、重启边界

**设计决策**

- **E14**（`n42-consensus-service/src/fcu_tags.rs`）：Profile 1 上每个 FCU 为 head = 已提交块 N，safe = N−1，
  finalized = reth 报告已持久化的块中不高于 safe 的那个（`canonical_in_memory_state().get_persisted_num_hash()`）。
  - finalized 必须在 safe 的链上：沿父链接回走验证（上限 `MAX_FINALIZED_WALK = 1024`），持久化块在别的分支上时不断言。
  - 持久化块高于 safe 时，只在它是 safe 的后代时取 safe；finalized 永远不会等于 head。
  - 无法从本节点知识确定的标签发零哈希（Engine API 的"不更新"），不猜。genesis 是自己的 safe/finalized。
  - 父链接来自原生头登记表（`remembered_profile1_header`），否则读 reth 的头。
  - 非 Profile 1 保持 (N,N,N)。`forkchoice_state(tags, head, committed)` 区分"head"和"已提交块"：流水线定案可能使 head 越过已提交块，标签按已提交块算。
  - 检查"finalized == head"的假设：所有 FCU 点（orchestrator 定案、重试、后台导入、构建、导入、catch-up，observer 3 处，
    replay plan）都改为走同一函数。委托 EL（`N42_EL_OWNER_RPC`）把 finalized 当作已提交块读取，与延迟标签不兼容，
    因此该变量列入 Profile 1 禁用环境变量。
- **E12**（`halt.rs`）：已提交块（有 CommitQC，或被共识提交、或位于严格提交锚之下的 range 块）出现以下任一情况即停机：
  1. 头携带的字段 ≠ 本节点对父块的结果；
  2. 本节点导入时记录了携带字段不符（QMDB 任务 `record_carried_mismatch`）；
  3. 同一高度提交了两个不同块（`CommittedHeights`，SYNC §3.3 冲突提交证据）。
  停机行为：证据写入共识数据目录下 `profile1_halt.json`（临时文件 + rename + 目录 fsync），gauge `n42_profile1_halted = 1`；
  orchestrator 主循环进入 `idle_halted()`，丢弃一切共识消息、区块与引擎输出（不导入、不投票、不出块），每分钟打一条 ERROR。
  启动时若该文件存在则以停机状态启动。
  - **恢复只能由运维操作**：停节点；保存并移走 `<N42_DATA_DIR>/profile1_halt.json`；查明原因（本节点执行错误则从可信快照/对等节点重建
    datadir，网络分叉则按 SYNC 处理）；再启动。
- **重启（设计 3.8）**：
  - `require_head_result(number, hash)`：共识头高于 0 而 `executed_fields.log` 没有其结果时拒绝启动（`RestartError::HeadResultMissing`，
    提示从对等节点重导或从备份恢复）。orchestrator 与 observer（PR 8 补上）启动时都检查。
  - QMDB WAL 已提交而结果日志未写入时崩溃：重启后同一块再导入，`compute_and_commit_own` 对相同操作幂等，补登结果；操作不同则 ConflictingBlock。

**测试**：`fcu_tags` 5 个；`halt` 4 个；`executed_fields::a_restart_needs_the_heads_own_result`；
`qmdb_state_root::deferred_commit_after_a_crash_before_the_result_was_filed`；orchestrator 进程内故障注入
`e12_a_committed_block_with_wrong_carried_fields_halts`、`e12_a_committed_block_the_import_refused_halts`、
`e12_a_conflicting_commit_halts`、`e12_needs_a_halt_latch`（无停机锁存时不停机），以及 `e14_commit_forkchoice_tags`。

**四节点**：区块 1-18 0 处不符，四节点标签均为 latest=18 safe=17 finalized=17；重启节点 2 后到 23 块，标签 23/22/22。

### 10.2 PR 6：可包含性与 Phase B

**设计决策**

- **gov5 规则逐字移植**（`n42-consensus-service/src/deferred_check.rs`，只读参考 gov5 `deferred_includable.go` 等）：
  `intrinsic_gas`（含 Amsterdam 的 EIP-2780：TX_BASE 12000、冷账户 3000、value 6000、create 12000、访问列表 2900+20×64 / 2000+32×64、授权 7816）、
  `deferred_tx_plan`（无状态规则、按发送者分组、gas limit 之和不超块上限）、`check_sender_states`（nonce 连续、最坏费用 ≤ 余额 + 先前转入、发送者无代码）。
  gov5 的测试用例作为表驱动测试带入。
- **`ExecutionLayer::deferred_check`**（默认 `Unsupported`）。`n42-node/src/el/deferred.rs` 的 `DeferredChecker` 只读不执行，按顺序：
  已执行过 → Checked；头未进门 → Retry；载荷父块与头不符 → Refused；父块结果未登记 → Retry；携带字段与本节点结果不符 → Refused；
  解码与恢复签名（rayon）；plan；在父块后状态上读发送者。指标 `n42_deferred_check_total{outcome}`、`n42_blocks_refused_total{reason}`。
- **Phase B**：新事件 `ConsensusEvent::BlockChecked{block_hash,parent_hash}` 只释放 Round 1（仍要求父块 = justify 块，AM19）；
  `BlockRefused(hash)` 永久扣下该块的 Round 1，即使之后导入成功也不投（gov5 在检查上投票）。CommitVote 仍要求 `BlockImportedWithParent`（本节点执行）。
  follower 的 eager import 任务在 Profile 1 且未停机时同时起 `deferred_check` 与 new_payload，导入与检查重叠。
- **陷阱：Decide 早于本节点导入**：Round 1 在检查上投出，法定人数可能在本节点导入完成前提交。现有 `finish_eager_import_completion`
  把提交挂起（`enqueue_async_commit` + `mark_async_commit_execution_ready`），超时处理见到 eager import 在途时只等待、不重复导入；导入 Valid 后再定案。
- **构建器可包含性过滤**（设计 3.3）：`IncludabilityFilter` 用与检查相同的规则逐笔判断（`admit` 不改状态，构建器执行成功后 `commit`）；
  在 `default_n42_payload` 中 Profile 1 启用，被拒交易 `mark_invalid`（连带后续 nonce），计数 `n42_builder_not_includable_total`。
  Profile 1 上并行构建器回落到顺序构建器（并行路径不做逐笔过滤）。
- **Phase A/B 取舍**：Retry/Unsupported 回落 Phase A（导入后投票），所以检查不可判定时不会少投或错投，只是慢；
  Refused 扣下 Round 1，但如果法定人数仍形成 PrepareQC，本节点执行成功后照常投 CommitVote（与 gov5 一致，避免活性问题）。
  Phase B 的收益在 leader 自己的块和父块结果已就绪时；检查本身不执行，只有签名恢复与少量账户读。

**测试**：`deferred_check` 6 个（含 `builder_filter_matches_the_check`：过滤器接纳的集合恰好通过检查）；
`profile1_v6.rs` 新增 4 个 Phase B 引擎测试（检查释放 Round 1 且字节等于 gov5 的 prepare_vote_v2，但不释放 CommitVote；
AM19 与检查早于提案；被拒块无 Round 1；非 Profile 1 无效果）；orchestrator `phase_b_a_decide_before_the_import_is_queued`、
`phase_b_check_outcomes_reach_the_engine`；`profile1_deferred.rs` 中 V5 每块在导入前检查为 Checked，携带错误 requestsHash 时检查拒绝。

**四节点**：区块 1-20 0 处不符，10 笔交易全部上链，标签 20/19/19；ERROR 0、REFUSED 0；日志中可见 "checked vote: deferred check passed before the import"（各节点 4-11 次）。

### 10.3 PR 8：延迟链上的 range 同步与观察者

- range 导入失败的块若不是仅松散锚定（即在严格提交锚之下，已提交），在 Profile 1 上走 `profile1_check_committed`：有携带字段不符证据则停机（E12），否则照旧停止本轮。
- 观察者启动时同样 `require_head_result`（从重启头开始必须有自己的结果）。
- **range 服务端修复（四节点运行发现）**：Profile 1 节点无法提供 `bodies_by_range`，请求方 range 客户端卡死：
  1. `gov5_rpc` 服务端对要发出的块用 `decode_gov5_block_rlp` 自检，只认 gov5 原生头布局，Profile 1 的 24 元素头一律
     "invalid gov5 block RLP"（code 2）。改为 `decode_wire_block_rlp`（两种布局都读），by-hash 同样。
  2. `main.rs` 的 `gov5_canonical_block_reader` 先从 reth 头 `Gov5NativeHeader::recover`，Profile 1 块 reth 头不哈希到块哈希，恢复失败。
     改为已记住 Profile-1 头时跳过恢复，`encode_gov5_block_rlp` 本身已按记住的 Profile-1 头编码。
  - 新测试 `gov5_rpc::range_and_hash_serve_profile1_blocks`。
  - 限制：Profile-1 头只存在进程内登记表（8192 条），服务端重启后无法提供重启前的块；持久化 Profile-1 头是后续项。
  - 另一个原有问题（未改）：range 客户端在没有任何回复可用时，`anchored_total` 随实时区块增长被当作"进展"，
    `stall_timeout` 永不触发，且客户端激活后反向遍历被跳过，节点停在原高度（本次在修复前观察到 23 分钟无进展）。
- **20 块延迟链 range 测试**（`profile1_deferred.rs` (f)）：在 V5 + 6' + 7' 之上再出 20 块（8-27，每块携带父块结果，leader 轮换并封印）；
  第二个节点用自己的 reth 树与自己的 QMDB 存储，从 genesis 按高度顺序导入整段 27 块（导入门 → Phase B 检查 → 引擎），
  每个子块携带的 stateRoot 都等于第二个节点自己算出的父块根；重新打开 `executed_fields.log` 含全部 27 块结果，`require_head_result(27)` 成立；
  追平节点的 E14 标签为 safe=26、finalized=持久化块。
  - 限制：登记表是进程级的，第二个节点的检查对已登记块直接返回 Checked；真正的独立性由它自己的 QMDB 根与引擎导入（结果不同会冲突）保证。
- **四节点重启**：停节点 3（高度 6），三节点继续到 30，重启节点 3：从重启头 6 开始经反向祖先遍历追平 7-30（24 块，gap < 32 未触发 range 客户端），
  0 ERROR，四节点到 31 块 0 处不符，标签 31/30/30。
- **四节点 range 客户端**（修复后，新链）：停节点 3（高度 5），三节点到 45，重启节点 3：`range catch-up started head=5 target=45`，
  3 秒后 `stopped reason=CaughtUp released=39 faults=0 conflicts=0`，24 秒内追平；到 59 块 0 处不符，四节点标签一致，ERROR/REFUSED/HALT 均为 0。
  交易：节点 3 停机期间同一发送者的 nonce 缺口使后续交易等待，节点 3 恢复后区块 49-51 上链（区块 51 一块 5 笔同发送者交易，构建器过滤器正常接纳），10 笔全部 status 1。

### 10.4 状态与后续

- 门禁：`cargo clippy --workspace --all-targets -D warnings` 通过；`cargo test --workspace` 2080 通过、0 失败、38 忽略。
- EIP-8282 创世 alloc 拒绝（方案 C）已按协调撤销：预部署地址可由 Nick 方法在创世后部署，创世规则无法保证调用为空操作；由用户在 evm2 侧决定。
- 后续：设计 3.10 手机端语义；V5 的 AM25 请求块；observer 的 E12（目前只有 orchestrator 停机）。

## 11. EIP-8282 在原生链上关闭

分支 `feat/evm2-no-8282-native`（基于 1512a00）。owner 批准的方案 A：vendor 并修补 evm2。

### 11.1 问题

- evm2 rev 66687c18 的 `post_execution_system_call_state_changes`（upstream `execution.rs:382-450`，条件在 :422）
  只要 `spec_id.enables(AMSTERDAM)` 就在块尾调用两个 EIP-8282 预部署地址（builder deposit `0x0000BFF4…8282`、
  builder exit `0x000064D6…8282`），从不看链配置。
- Profile 1 从 `amsterdamTime` 把 evm2 spec 提到 AMSTERDAM（AM1），同时去掉 reth 的 Amsterdam 硬分叉，所以 n42-26
  在每个 Profile-1 块都发出这两个调用。n42-rs 自 2026-11-06 也用同一 evm2 rev，同样会调用（其
  `docs/EVM2_PORT_PLAN.md` S1 计划加门）。gov5 在原生链上不调用（`UsesEIP8282BuilderRequests` = Glamsterdam 且非
  `IsN42Native`；标准 D2）。
- 地址目前无代码，调用是空操作；但预部署可用 Nick 方法由任何人部署（原生链接受 pre-155 交易），之后调用会改状态、
  产生 0x03/0x04 请求（或合约 revert 让所有块无效），与 gov5 分叉。

### 11.2 设计决策

- 开关放在 evm2 的 `EthBlockExecutor`（一个字段 + `set_eip8282_builder_requests`），默认 `true` 即上游行为，
  `start_block` 保留该值；`apply_post_execution_changes`（`finish` 也走它）把它传给新函数
  `post_execution_system_call_state_changes_with_builder_requests`。原函数签名不变、内部传 `true`。
- 没有改 `BlockExecutionContext` / `EthBlockRules`：前者被 reth 测试按字面量构造，加字段会破坏 reth 编译；后者
  `derive(Default)` 得到 `false`，默认值会与上游相反，且 reth 在 big-block 分段时每段重建 rules。
- 不用 reth 的 `set_spec_id(OSAKA)` 绕过（n42-rs 计划的路线）：它同时改变 deposit 请求扫描与系统调用的 spec，
  语义比单独开关宽；vendor 的成本是升级 evm2 时重放一个小 diff。
- 谓词：`eip8282_builder_requests_active(genesis) = !n42_chainspec::native::is_n42_native(genesis)`，即 gov5 的
  `IsN42Native`（consensus 为 hotstuff/apos，或 config 有 hotstuff/apos 段，或声明了 n42Profile），与 n42-rs
  `crates/chainspec/src/qmdb.rs` `is_n42_native` 同一规则。最初写的是“链配置有 reth Amsterdam 才调用”（模仿
  alloy-evm），在三方共享的链上结果相同，但按协调方更正（n42-rs 已在 evm2 上，并按原生性门控）改为原生性谓词，
  与 gov5、标准和 n42-rs S1 对齐。非原生（以太坊兼容）链只要 spec 到 Amsterdam 就照常调用。
- 放置点：`TrackingBlockExecutorFactory::create_executor`。`N42EvmConfig` 的所有执行器（引擎导入、payload builder、
  并行构建、手机验证器）都从这里创建；grep 确认没有绕过它直接用内层 `EthEvmConfig` / `EthBlockExecutorFactory` 执行的路径。

### 11.3 实施细节

- `vendor/dependency-repairs/evm2-66687c18/`：只 vendor `crates/evm2`（src、build.rs、README 实体拷贝——上游是指向根
  README 的符号链接、LICENSE*、根目录 LICENSES/），`Cargo.toml` 按 `cargo package` 方式展平（去掉 dev-deps、tests、
  examples、lints；`std` 特性去掉只属于 dev-dep 的 `ark-std/std`），末尾 `[workspace]` 使其不属于本 workspace。
  `N42-PATCH.diff`（两个文件）、`N42-PROVENANCE.md`（来源、改动、原因、升级重放步骤）。
- workspace `Cargo.toml` `[patch."https://github.com/alloy-rs/evm2"]`：`evm2` 改为 path；evm2-macros / evm2-jit 仍指向
  DaniPopes 同 rev。`Cargo.lock` 只剩一个 `evm2`（无 source 行），`cargo tree -i evm2` 只显示 path 包；reth-core 的旧
  pin（f93318f）同样被折叠进来。
- `n42-chainspec::native::is_n42_native`、`n42_execution::eip8282_builder_requests_active`（n42-execution 新增
  `alloy-genesis` 依赖）。
- `docker/n42-node/Dockerfile` 增加 `COPY n42-26/vendor/`：此前镜像构建已缺 evm2-inspectors 等 vendored 修复，
  evm2 本体 vendor 后不复制就无法构建。CI workflow 直接检出整个仓库，vendor 随仓库可用；`reth-source.lock`、
  `check-reth-source.sh` 只管 reth 源，不涉及 evm2。

### 11.4 测试（`crates/n42-node/tests/profile1_vectors.rs`）

预部署按部署后的样子放进状态：alloy-eips 代码、nonce 1、excess 槽 = 2^256-1（inhibitor），并各排一个待处理请求
（count 1、tail 1、条目槽）。用 V3/main 前两个块（空块、普通转账）。

- `profile1_amsterdam_makes_no_eip8282_calls`：从创世起就有代码。两个地址不进 bundle，无 0x03/0x04 请求，
  每块 QMDB 写集、根、requests 与地址为空时相同。
- `profile1_mid_chain_eip8282_deploy_makes_no_calls`：块 k 后才出现代码（模拟 Nick 方法 CREATE），块 k+1 同上相等。
- `non_native_amsterdam_chain_keeps_the_eip8282_calls`：去掉 n42Profile/consensus/hotstuff 并加回 reth Amsterdam，
  两个调用照常发生：excess 槽从 MAX 变 0，产生 0x03 和 0x04 请求（上游默认行为，防止过度门控）。
- 反向验证：把开关强制为 `true` 时两个原生链测试失败（地址被改），恢复后通过。
- `n42-chainspec` `n42_native_follows_gov5` 覆盖谓词。

### 11.5 后续

- n42-rs S1/S2 可对照：谓词 `crates/n42-chainspec/src/native.rs` `is_n42_native`，门
  `crates/n42-execution/src/evm_config.rs` `eip8282_builder_requests_active`，设置点
  `crates/n42-execution/src/restored_slots.rs` `create_executor`。
- 升级 evm2 时按 `N42-PROVENANCE.md` 重放；若上游提供开关（例如 `EthBlockRules` 上的 builder_requests），删除 vendor。
- gov5 发布含 8282 代码与排队请求的 V5 用例（n42-rs S8）后加入本仓库。

## 12. Profile 1 跟随者（AM6 入口形式：验证型非验证者）（2026-11-24 追加）

分支 `feat/profile1-follower`（基于 main 5d82240）。owner 已授权 n42-26 以 follower、validator 或 producer 身份加入混合轮次；
本节实现 follower：不持有验证者密钥，不投票、不发 NewView/timeout、不提案，导入验证者产出的链。

### 12.1 设计决策

- **选 (b)：参与者模式下无验证者索引、永不签名**，不选 (a)（在 Profile 1 上开启 `N42_OBSERVER_MODE`）。
  - 观察者（`observer.rs`）是另一条导入路径：有 Profile-1 头门和启动时的 `require_head_result`（PR 8），但没有 Phase B 的
    延迟检查、没有 E12 停机（10.4 已列为后续项）、没有 range 客户端（它用自己的 gov5 finalized catch-up），QC 检查也是它自己的
    `verify_sync_block_qc`。走 (a) 等于把这些再在观察者里实现一遍，follower 检查的东西也不再与验证者完全相同。
  - (b) 复用验证者的编排器（`ConsensusOrchestrator`）与引擎：同一个导入门（含 B2 封印）、Phase B 延迟检查、引擎导入
    （QMDB 根任务比对五个携带字段）、`executed_fields.log`、E14 标签、E12 停机锁存、range 客户端与反向祖先遍历；
    Proposal/PrepareQC/Decide/NewView 由同一个 `ConsensusEngine` 按 genesis 集合（E13）验证。
  - 引擎本来就有非成员守卫：`is_local_validator_active_for_view` 按本地公钥是否在该视图集合中决定能否投票/超时；
    `is_leader_for_view` 也按公钥。follower 的索引取 `n`（集合之外），因此永远不是 leader。
- **三道不发送守卫**：
  1. follower 不加载验证者密钥，用随机 BLS 密钥（不在 genesis 集合中），引擎因此不签任何东西；
  2. `handle_engine_output` 在 follower 模式下丢弃一切 `BroadcastMessage`/`SendToValidator`（计数
     `n42_follower_engine_output_dropped_total{kind}`，WARN；正常运行应为 0）；
  3. 网络端口包一层 `FollowerNetwork`（`net_port.rs`）：投票、提交投票、超时、NewView、提案、QC、Decide 的 gossip/直连，
     区块公告、gov5 区块推送、直连区块、blob sidecar 一律吞掉（计数 `n42_follower_publish_refused_total{kind}`），
     只放行拉取类请求（by-hash、range、sync）、sync 应答、交易转发；`set_validator_context` 为空操作（不承担 Rotor 中继角色）。
  另外 `with_follower()` 置 `leader_disabled`，出块入口直接返回。
- **入口开关 `N42_FOLLOWER_MODE=1`**（显式开启，不因密钥不在集合中自动降级）：只在 Profile-1 创世上允许；与
  `N42_OBSERVER_MODE` 互斥；设置了 `N42_VALIDATOR_KEY` 或 `N42_KEYSTORE_PATH` 时拒绝启动。拼错密钥的验证者仍然按原规则拒绝启动
  （错误信息提示 follower 开关），不会悄悄变成 follower。
- **网络**：swarm 用不带验证者索引的 interop swarm（`build_interop_observer_swarm`，Identify 不声明 `v{index}`），
  网络服务仍是 H2 参与者模式（H2 消息走共识通道，供引擎处理）。libp2p 身份取 `N42_P2P_KEY`，否则随机，绝不用验证者的确定性身份。
  不做自动连接（端口推算只对验证者成立），只拨 `N42_TRUSTED_PEERS`。
- **range 锚定强制（§3）**：四节点运行中发现 Profile 1 上 range 客户端默认是 `observe`（`N42_SYNC_ANCHOR` 又被 AM10 禁用，
  无法改成 `enforce`），而标准要求“Anchors enforced，没有 observe 模式”。`with_profile1_gate` 现在把 range 客户端改为
  `Enforce`（`RangeCatchup::enforce_anchors`，`Off` 保持不变）。这同时作用于验证者。
- **AM10**：禁用环境变量规则不变。注意 `N42_GOV5_TCP_PORT` 属于 `N42_GOV5_` 前缀，Profile 1 上被拒绝；TCP 监听端口即
  `N42_CONSENSUS_PORT`（QUIC 与 TCP 同号）。
- 顺带：引擎 `sync_local_validator_index` 对“本就不在集合中”的节点（索引 ≥ n）把每视图一次的
  "local key not in current validator set (sync)" 降为 debug。

### 12.2 实施细节（文件）

- `bin/n42-node/src/main.rs`：`resolve_follower_mode`（含单测 `follower_mode_is_profile1_keyless_and_not_an_observer`）；
  follower 的随机 BLS 密钥、索引 `n`、无手续费地址；p2p 身份；无索引 swarm；跳过自动连接；导入门不带封印密钥；
  `orchestrator.with_follower()`。
- `crates/n42-consensus-service/src/net_port.rs`：`FollowerNetwork`、`consensus_message_kind`。
- `orchestrator/mod.rs`：字段 `follower`、`with_follower()`、`is_follower()`；`with_profile1_gate` 强制 range 锚定。
- `orchestrator/consensus_loop.rs`：follower 丢弃引擎的出站共识消息。`execution_bridge.rs`：follower 的出块拒绝日志。
- `range_catchup.rs`：`enforce_anchors()`。`n42-consensus/src/protocol/state_machine.rs`：日志级别。
- `tests/e2e/README.md`：follower 所需环境变量表。

### 12.3 测试

- `profile1_v6.rs::follower_follows_v6_and_sends_nothing`（gov5 V6 报文，引擎级）：follower 引擎对 proposal_v2、检查与执行事件、
  prepare_qc_v2（锁到 view 2）、decide_v2（提交区块 2，view 3）与验证者行为一致；没有 Round-1、CommitVote；三次超时不发 timeout；
  NewView（TC）推进到边界视图后给它区块也不提案；0..1000 视图都不是 leader；外来集合聚合的 Decide 被拒绝（E13）。
- `orchestrator/tests/profile1.rs`：
  - `follower_publishes_no_consensus_message`：V6 的 7 种消息分别经 `handle_engine_output`（广播与定向）、`broadcast_engine_consensus`、
    `broadcast_via_rotor`、网络端口直发，再加区块公告、gov5 区块推送、blob、出块触发——记录网络收到 0 条共识发布、0 次构建，
    无待重发投票；range 请求照常发出；range 客户端为 `Enforce`。
  - `follower_imports_a_profile1_chain_and_halts_on_a_tampered_carried_field`：follower 编排器导入并定案 4 个 Profile-1 块
    （每块携带父块的本节点结果），然后一个携带错误 stateRoot 的已提交块触发 E12 停机（证据字段 stateRoot），不导入、不定案；全程 0 发布。
- 真实执行的导入（独立 reth 树与 QMDB、Phase B 检查、引擎）沿用 10.3 的 `profile1_deferred.rs` (f)，follower 走的正是这条路径。

### 12.4 四验证者 + 两 follower 本地运行（`/tmp/p1-run/run4f.sh`，`check_follower.py`）

- 端口：验证者 reth 30400-30403、HTTP 18545-18548、共识（QUIC+TCP）19400-19403；follower 1 为 30410/18555/19410，
  follower 2 为 30411/18556/19411。未碰 3070x/2014x。数据在 `/tmp/p1-run/data4f`，genesis 为 `genesis_now.json` 改当前时间戳。
- 验证者的 `N42_TRUSTED_PEERS` 列出两个 follower（固定 `N42_P2P_KEY` 的 peer id），因为 n42-26 验证者只对受信或验证者
  peer 提供 `bodies_by_range`；follower 的 `N42_TRUSTED_PEERS` 是四个验证者的 `/ip4/127.0.0.1/tcp/1940i/p2p/<id>`。
- 第一轮（含最终代码前的二进制）：follower 1 与验证者同时启动；follower 2 晚 2 分钟（高度 16）启动，经认证 catch-up（反向祖先遍历，
  缺口 < 32）补 1-17 后跟随；清空 follower 2 数据在高度 41 重启，range 客户端 `head=0 target=42`，3 秒 `CaughtUp released=40`。
  到 47 块 6 节点一致，0 不符。
- 最终二进制（range 强制锚定后）重跑：10 笔 V3 交易；follower 2 在高度 16 晚启动，补 17 块后跟随；在高度 34 清空重启，
  `range catch-up started mode="enforce" head=0 target=34` → 3 秒 `stopped reason=CaughtUp released=32 released_loose_only=0 faults=0 conflicts=0`。
  `check_follower.py 1 2`：区块 1-41 六个节点哈希相同，每块携带的五个字段等于每个节点（含两个 follower）`executed_fields.log`
  中为父块登记的结果，0 处不符；六个节点标签均为 latest=41 safe=40 finalized=40；10 笔交易在 follower 2 上 status 1（区块 3-12）。
- 日志：六个节点 ERROR 0、HALT 0；follower 的 "sending vote to leader" 0 次、"follower: dropped" 0 次（引擎根本没产生出站消息）；
  follower 唯一的拒绝是启动瞬间 gossip 块的 `parent_unknown`（非永久，随后由 catch-up 补齐）。

### 12.5 门禁

- `cargo clippy --workspace --all-targets -- -D warnings` 通过。
- `cargo test --workspace`（`--test-threads=12`）：2092 通过、0 失败、38 忽略。
- 构建说明：`/data` 在运行期间被同机的 gov5 qs 节点（`/data/blockchain/qs-node*`）以约 10 MB/s 写满，本次后半段构建改到 tmpfs
  `/tmp/n42-26-p1f-target`（`CARGO_INCREMENTAL=0`，dev/test 无 debuginfo），并删除了 `/data/n42-build/n42-26-p1f/target`。

### 12.6 后续 / 缺口

- 对外部验证者（gov5、n42-rs）：follower 能否拉 range 取决于对方的授权规则；n42-26 验证者要求把 follower 列入 `N42_TRUSTED_PEERS`。
- follower 不发布 Decide（task 要求不发任何共识消息）；E12 的 "Decide is broadcast" 由验证者完成。
- follower 仍按 GossipSub 规则在 mesh 中转发它收到的验证者消息（传输层转发，不是自己发起或签名）。
- Profile-1 头只在进程内登记表中（10.3 的限制）：follower 重启后无法向他人提供重启前的块。

## 13. V5 重新发布（AM25，8 块）与 EIP-8282 builder 合约向量（2026-11-25 追加）

### 13.1 来源

- gov5 feat/profile1 590d9673（"docs(profile1): document the V5 EIP-8282 builder request case"）或更新；
  `/data/blockchain/wr-logs/qs-scratch/profile1/vectors/V5/` 下三个文件与 gov5 仓库
  `internal/testdata/profile1/vectors/V5/` 在 590d9673 及 feat/profile1 6a3e1eb9 上逐字节相同。
- 复制到 `crates/n42-node/testdata/profile1/gov5/V5/`：`header_validity.json`（6 块 → 8 块）、
  `builder_request_contracts.json`（新）、`README.md`。vectors/ 下没有 SHA-256 清单；按 V3/V4 的做法，
  在 `testdata/profile1/gov5/README.md` 的 SHA-256 表中补记 V5 三个文件。

### 13.2 设计决策

- 生产代码不改：AM25（requestsHash 为延迟字段，块 N 携带 N−1 的 EIP-7685 requests hash）此前已在
  `N42Consensus::validate_block_post_execution` 与登记表中实现；之前测试只用常量 `PROFILE1_REQUESTS_HASH`
  作为 gov5 的期望，现改为读取向量的 `executed.requestsHash`，不放宽任何比较。
- EIP-8282 "强制开启"的对照沿用已有 `non_native_amsterdam_chain_keeps_the_eip8282_calls` 的做法，与 gov5
  的 sensitivity 运行一致（把链配置改成非原生）：去掉 `n42Profile`/`consensus`/`hotstuff`，reth Amsterdam 从 0 开始；
  写集仍按原生链的规则与创世 QMDB 树计算。未新增生产代码中的测试开关。
- 8282 向量的块没有封装头，只给 number/timestamp；其余输入按 gov5 `v3Runner.run`：coinbase = 验证者 0、
  gasLimit = 创世、baseFee 1 gwei、parentBeaconBlockRoot 0x11..11。gov5 内部头的哈希未公布，块 2 的 parentHash
  取自该块自己的 EIP-2935 写入（slot 1），块 1 的同一写入必须等于本节点的创世哈希（已断言）。

### 13.3 实施细节

- `crates/n42-node/tests/profile1_deferred.rs`：8 块全部经真实导入路径（gov5 线格式 → Profile-1 解码 →
  头 gate → Engine payload → reth engine validator + QMDB 状态根策略）；断言原生开关关闭；每块 RLP 重编码、
  哈希、携带的五字段、登记的五字段、requests 原文、receipts 累计 gas 均等于 gov5；块 7 也是 state-gas 块；
  块 8 携带块 7 的 requestsHash。新增的 B8 "requestsHash absent" 拒绝在 Profile-1 头解码时即被拒（元素 21
  必须是 32 字节），测试将其计为 import 拒绝（只允许这一条无法解码）。兄弟链 6' 现在 reorg 掉 6-8。
- `crates/n42-node/tests/profile1_vectors.rs`：
  - `v5_builder_request_contracts_equal_gov5_without_eip8282_calls`：创世 = V1 + allocAdditions（gov5 把 code
    写成 base64，测试内解码为 hex，并核对 `5f546001015f5500` 与 builderCodeHash）；创世 QMDB 根、每块 stateRoot、
    receiptsRoot、gasUsed、requests（只有 0x01/0x02）、requestsHash 与完整 QMDB 写集都等于 gov5；两个 8282
    账户不在 bundle、不在任何写集，执行后 code/nonce/balance/slot 0 不变。
  - `v5_builder_request_contracts_with_eip8282_calls_write_the_slots`：开关强制开启时两地址 slot 0 每块被写
    （1、2），进入写集，stateRoot 与 gov5 不同；receipts/gas/requests 不受影响。

### 13.4 结果

| 块 | 本节点哈希 = gov5 哈希 | executed stateRoot | 携带/产生的 requestsHash |
| --- | --- | --- | --- |
| 1 contract-create | `0x9b5b7c1a…a921d` | `0x63baa78a…af74` | sha256("") |
| 2 sstore-clear-equal-timestamp | `0xfc85cbcc…69ff5` | `0x9daa4f18…780d` | sha256("") |
| 3 new-account-transfer | `0xb2b01293…e22c7` | `0x4c08a0ca…afcb` | sha256("") |
| 4 plain-transfer | `0x88f4c28f…ec1e0` | `0xb8fbeee0…9884` | sha256("") |
| 5 idle | `0x4a62bdd5…cff52` | `0xb20942ec…b448` | sha256("") |
| 6 carries-idle | `0x1534b303…b94ae` | `0x5a2a4b1c…cb24` | sha256("") |
| 7 withdrawal-and-consolidation-requests | `0x7eb1ca7d…d52c4` | `0xf6fd81de…904e` | 产生 `0x9f1fcb7b…a2c4` |
| 8 carries-requests | `0xa189a6c3…ba7a9` | `0x9967ae8b…f0b8` | 携带 `0x9f1fcb7b…a2c4`，自身 sha256("") |

8282 用例：块 1 stateRoot `0x959b5e91…538b`、receiptsRoot `0xc5d24601…a470`、gasUsed 0；块 2 stateRoot
`0x63456d12…0ed8`、receiptsRoot `0x1ac6d164…1066`、gasUsed 1077120、requestsHash `0x9f1fcb7b…a2c4`，全部等于 gov5。
无任何字段差异，未改生产代码。

### 13.5 门禁

- `cargo clippy --workspace --all-targets -- -D warnings` 通过。
- `cargo test --workspace`（`--test-threads=12`）：2094 通过、0 失败、38 忽略（比 §12 多 2 个新测试）。

### 13.6 后续

- 8282 用例的头只是执行输入；若 gov5 以后公布其内部头哈希，可直接用它代替从 EIP-2935 写入回推 parentHash。

## 14. AM27：从创世时间戳起算的 slot 网格（rc4 A17/B4）（2026-11-25 追加）

### 14.1 设计决策

- 规则（标准 rc4，n42-rs `f9272d937`）：slot s 起点 `G + s·I`，G 为创世头 timestamp，I 为
  `config.hotstuff.blockIntervalMs`（取自创世，不写死 8）。导入时检查 `(time − G) mod I = 0` 与
  `time ≥ parent.time + I`（允许跳 slot）；投票前的未来界 `time ≤ floor(now) + 2` 不变。取代 AM26 与 rc3 的
  `max(parent.time, floor(now))`。
- 检查放在 `validate_profile1_header`（`Profile1ImportGate` 唯一入口），因此跟随者、验证者、range 同步、
  catch-up/replay、观察者全部路径自动生效；非 Profile 1 不经过该 gate，无变化。
- 网格以毫秒计算，I 不是整秒时也精确；只有整秒的 slot 起点可盖戳。
- leader：在父块 slot 之后、且不在过去的第一个 slot 出块：`s = max(sP + 1, ceil((now − G)/I))`，
  到点前不建块，时间戳就是 `G + s·I`。已排定的 slot 若定时器晚到不超过 1 s（`PROFILE1_SLOT_START_GRACE_MS`）
  仍沿用，否则顺延到下一个边界。旧实现按 unix 纪元对齐边界、并在 `timestamp <= last_committed` 时 +1/+2，
  在 Profile 1 上都会离开网格，已关闭。

### 14.2 实施细节

- `crates/n42-consensus-service/src/profile1_header.rs`：新增 `Profile1SlotGrid`（`is_on_grid`、`slot_of`、
  `proposal_slot`、`plan_proposal`）、`check_profile1_import_time`、错误 `TimeOffGrid`/`TimeSameSlot`（规则 id B4）；
  `Profile1HeaderRules.grid`、`Profile1ImportGate::grid()`，gate 从创世头与配置构造网格。
- `orchestrator/execution_bridge.rs`：`profile1_proposal_plan`（父块时间取自 gate / 已记住的头）；
  `do_trigger_payload_build_on` 在 Profile 1 上先算 slot，起点未到则挂 `next_build_at` 并返回，到点后以该戳建块；
  `build_payload_attributes` 不再改动 Profile-1 戳；FCU 重试在 Profile 1 上不抬戳；build-on-output 也用网格戳。
- `orchestrator/consensus_loop.rs`：`next_slot_boundary` 在 Profile 1 上用 G 网格与父块 slot。
- 测试：网格单测（在网格/偏 ±1 s；parent+0 拒、parent+8/+16 收；I=12 s、I=1.5 s）、leader 计划单测（含超时后
  mid-slot 进入）、gate 对 V1 块 2 的六个重签变体；跟随者、同步、catch-up、观察者四条路径各自用同一组变体；
  leader 端到点前不建块、到点建块且 attributes.timestamp 等于网格戳。

### 14.3 问题与向量冲突

- **V5（rc3，gov5 2026-11-24 发布）块 2 `sstore-clear-equal-timestamp` 与父块同时间**，AM27 下被 B4 拒。未改向量、
  未放宽规则：`v5_deferred_chain_imports_and_every_wrong_carried_field_is_invalid` 标 `#[ignore]`，新增
  `v5_block2_equal_timestamp_is_refused_under_am27` 记录冲突，待 gov5 按 AM27 重发 V5。
- V3（`G+1..G+7`）与 V4（`G+1`）时间戳也不在网格上；它们只作执行向量，不经过头 gate，测试仍通过，但作为
  真实块会被拒，同样需要 gov5 重发。V1 在网格上（G+8、G+16），全部通过。qs-scratch 与 n42-rs 均未见 AM27 重发的向量。
- 测试自造链：`profile1_import.rs` 由 parent+1 改为 parent+I；`profile1_deferred.rs` 的 6' 由 +1 改为 +8。
- clippy `manual_is_multiple_of`：改用 `is_multiple_of`。

### 14.4 本地演练

4 验证者 + 1 跟随者（`/tmp/p1-am27/run_am27.sh`，G = 1795593997，G mod 8 = 5，故网格不与 unix 纪元对齐），
块 8 后停验证者 2 约 65 s 再启动。27 块：全部 `(t − G) mod 8 = 0` 且 `t ≥ parent + 8`；两次超时后块 10、13
分别落在 slot 13、19（dt = 32）；各节点建块起点均在 slot 起点后 0.6–2.5 ms，无累计漂移；5 节点哈希一致，
携带字段不符 0，头拒绝 0，E14 标签全部正常。

### 14.5 后续

- gov5 按 AM27 重发 V3/V4/V5 后，恢复被忽略的 V5 延迟执行测试。
- build-ahead（`N42_BUILD_ON_OUTPUT`，默认关）在 Profile 1 上已用网格戳，但提前建好的块在本视图开始即发布，
  若早于 slot 起点 2 s 以上会被投票前未来界拒；Profile 1 上开启前需再加发布等待。

### 14.x 换用 gov5 的 AM27 版 V1/V5（2026-11-25 03:4x）

- gov5 03:04 重新发布 V1/V5（qs-scratch/profile1/vectors；旧版在 vectors.bak-pre-am27-20261125）。
  sha256：V1 `header_profile_v1.json` 13e53897…890c，V5 `header_validity.json` aeeb8357…2060，与 n42-rs 转告一致。
- V5：块 2 不再与父块同一时间戳，`v5_deferred_chain_imports_and_every_wrong_carried_field_is_invalid` 取消 ignore，通过；
  只针对 rc3 V5 的 `v5_block2_equal_timestamp_is_refused_under_am27` 删除。
- V1：新增 "timestamp equal to parent"、"timestamp off the slot grid" 两个导入拒绝；"requestsHash absent" 改成解码期（B8）拒绝，
  不再由延迟检查拒绝。导入拒绝从 21 个增至 24 个，静态检查总数 25（含一个投票期）；四处计数和 requestsHash 特例已相应更新，
  chainspec 解码测试把 "requestsHash absent" 归入解码错误。
- 只改测试和测试数据，未改产品代码，因此 06:30 前发布的二进制（5679751，sha256 d0a11c27…f5c9）仍然有效。
- 门禁：clippy 通过；workspace 2100 passed / 0 failed / 38 ignored。

## 15. Gate 6：n42-26 作为混合验证者集合中的验证者（无 peer 绑定，按 BLS 签名认证）（2026-11-25 追加）

分支 `feat/profile1-mixed-validator`（基于 origin/main 0d51c07），改动未提交。

### 15.1 设计决策

- **与 n42-rs 约定的规则**：只在 Profile 1 上，创世中没有任何验证者 `p2p_peer_id` 绑定（混合创世只列 `{address, blsKey}`）
  的验证者，与 follower 一样以 H2-v4 消息上的 BLS 签名认证验证者，而不以传输层 PeerId 认证。gov5/n42-rs 不发送 n42-26 的
  认证帧，它们的 PeerId 也不是确定性 PeerId，原有规则下多验证者节点会以
  "multi-validator peer authentication requires explicit p2p_peer_id…" 拒绝启动。
- 判定函数 `profile1_unbound_validator(profile1, observer_or_follower, n, no_bindings, deterministic_opt_in)`：
  Profile 1、非观察者/follower、n > 1、无绑定、未显式设置 `N42_ALLOW_DETERMINISTIC_P2P=1`。显式设置该变量时保持旧的
  n42-26-only 确定性身份（此前四验证者本地演练的方式）。有绑定或非 Profile 1 的链行为不变。
- 发现的阻塞点（逐项处理）：
  1. 启动：`expected_validator_peer_ids_with_policy` 报错 → 无绑定模式下期望表为空，`allow_deterministic` 置 false；
  2. libp2p 身份：原来无绑定时回退到按索引派生的确定性密钥 → 改为 `N42_P2P_KEY`，否则随机（同 follower）；
  3. 自动连接按端口与确定性 PeerId 推算 → 跳过，只拨 `N42_TRUSTED_PEERS`；
  4. 网络层 H2-v4 直连扇出（`hotstuff_direct` 与 `block_push`）按 `validator_peer_map` 选目标，无绑定时为空 → 改为发给所有在
     Identify 中声明该协议的已连接 peer（gossip 照旧）；
  5. 入站直连（`hotstuff_direct`/`block_push`）只接受受信或已绑定 peer → 无绑定模式下接受任意已连接 peer（消息由引擎验
     BLS 签名，块由导入门验 leader 封印；gossip 本来就不做来源认证）；
  6. leader 出块前的 "validator peer quorum" 检查按已路由验证者计数，无绑定时永远为 0，leader 永不出块 → 无绑定模式下按已连接
     peer 数计（只是“不对空网出块”的启发式，投票仍由引擎按 BLS 计数）；
  7. 交易转发按 leader 的 PeerId 发送，无绑定时找不到 → 无绑定模式下交易留在本地池（同 `N42_DISABLE_TX_FORWARD`），本节点
     轮到出块时打包；
  8. epoch 切换时重算绑定会走错误分支并 WARN → 无绑定模式下跳过；Identify 中 `n42/1.0.0/v{i}` 声明在无绑定模式下降为 debug。
- n42-26 原生 `consensus_direct`/Rotor 中继在 H2-v4 参与者路径上不使用（`h2_v4_identity.is_some()` 时只走 `BroadcastH2V4`），无需改。
- follower（期望表为空且未允许确定性 id 时）也打开同一网络开关：入站直连同样只按 BLS/封印认证。follower 的出站仍由
  `FollowerNetwork` 全部拦截，不受影响。
- bodies_by_range/state-sync 的服务授权未放宽：仍只对 `N42_TRUSTED_PEERS` 中的 peer 提供（演练中把对端全部列为受信）。

### 15.2 实施细节（文件）

- `bin/n42-node/src/main.rs`：`profile1_unbound_validator`（~1119）及单测；启动处计算并覆盖 `allow_deterministic_validator_peers`、
  空期望表（~2187-2215）；身份（~3175）；`trust_signed_messages_without_peer_bindings()`（~3257）；跳过自动连接（~3290）；
  `orchestrator.with_unbound_validator_peers`（~4034）。
- `crates/n42-network/src/service.rs`：`h2_v4_direct_fanout_peers`（~339）；字段 `unbound_signed_validators`、
  `gov5_hotstuff_direct_peers`、`gov5_block_push_peers`；`trust_signed_messages_without_peer_bindings()`（有绑定、允许确定性 id
  或非 H2-v4 参与者时拒绝）、`gov5_direct_inbound_authorized`、`h2_v4_direct_peers`；Identify 记录两类协议；连接全部关闭
  （`num_established == 0`）时才移除；两处入站检查与两处出站扇出改用新函数。
- `crates/n42-consensus-service/src/orchestrator/mod.rs`：字段 `unbound_validator_peers`、`with_unbound_validator_peers`；
  `connected_validator_peer_count`；`flush_tx_forward_buffer`。`consensus_loop.rs`：epoch 切换跳过重绑定。

### 15.3 测试与门禁

- `service.rs`：`h2_v4_direct_fanout_without_bindings_reaches_every_capable_peer`；`unbound_signed_trust_is_refused_when_bindings_exist`
  （有绑定、原生网络拒绝）；`profile1_unbound_validator_exchanges_direct_h2_v4_with_an_unbound_peer`（真实 QUIC：服务端无绑定，
  对端是未订阅任何 H2 topic 的裸 swarm，只能走直连——服务端接受其 `hotstuff_direct`，自身广播经 `hotstuff_direct` 到达对端）；
  `bound_validator_refuses_direct_h2_v4_from_an_unbound_peer`（不开开关时两方向都不通，对照）。
- `orchestrator`：`unbound_profile1_leader_counts_connected_peers_for_quorum`。`main.rs`：`profile1_unbound_validator_only_on_profile1_without_bindings`。
- `cargo clippy --workspace --all-targets -- -D warnings` 通过；`cargo test --workspace --no-fail-fast`：2109 passed / 0 failed / 38 ignored。

### 15.4 演练 A：4 个 n42-26 验证者，无 peer id，无确定性身份（`/tmp/p1-g6/runA.sh`）

- 创世 `/tmp/p1-run/genesis_now.json` 改时间戳（G = 1795621687，G mod 8 = 7），验证者条目只有 `address`/`blsKey`；
  `N42_ALLOW_DETERMINISTIC_P2P` 未设置；每节点 `openssl rand` 生成 `N42_P2P_KEY`；`N42_TRUSTED_PEERS` 为另外三个节点。
  端口 reth 30400+i、HTTP 18545+i、共识 19400+i。
- 日志确认 "Profile 1 without validator p2p_peer_id bindings…"；leader 直推块 `peers=3`（验证者 2 停机期间 `peers=2`）。
- 高度 8 停验证者 2 约 60 s 后重启，经认证 catch-up 补齐。23 块：4 节点哈希一致；携带字段与各节点 `executed_fields.log` 92 次比较 0 不符；
  E14 标签 latest=23 safe=22 finalized=22；出块者 {0:6, 1:7, 2:4, 3:6}；AM27 网格 0 违规（停机导致块 10、13 跳 slot，dt=32）。
- 四节点 ERROR 0、E12 停机 0、panic 0；WARN 只有重复投票抑制、停机期间的超时与断连。

### 15.5 演练 B：3 个 gov5 验证者 + 1 个 n42-26 验证者（`/tmp/p1-g6/B/runB.sh`）

- gov5 二进制 `n42-p1.am27`（sha256 3b7da0a2…39be，复制到 `/tmp/p1-g6/B/` 并校验）；gate4a 的 `run-p1mixed.sh` 只改端口行
  （TCP 30430、UDP 30440、HTTP 18570、MOBILE 18580、PPROF 6100）、P1_DIR、P1_BIN、P1_GENESIS；`P1_SET=smoke`，只启动 0、1、2，
  gov5 node 3 不启动；创世 `genesis-gate4a.json`，init 把时间戳改为当时（G = 1795621929），G+10 s 同时启动。
  n42-26 为验证者 3（同一 BLS 推导），`N42_P2P_KEY` 固定（12D3KooWMTTa…），经 `P1_EXTRA_PEERS` 加入 gov5 的 `--p2p.peer`；
  n42-26 的 `N42_TRUSTED_PEERS` 为三个 gov5 的 TCP 地址。两边创世哈希一致 `0xef0c96…bc8282`。
- 期间：gov5 node 1 按 pid 停 ~60 s 后重启（高度 17→22 由其余三家继续出块；重启后 gov5 node 1 从 n42-26 拉 range 补齐）；
  两路负载（gov5 v0 RPC 800 笔、n42-26 RPC 600 笔，4 tx/s）；随后 n42-26 自身停 ~60 s 后重启，认证 catch-up 补齐后继续出块、投票。
- 结果（113 块）：4 节点每个高度哈希一致；1432 笔交易；n42-26 `executed_fields.log` 与每个头携带的五个延迟字段 113 次比较 0 不符
  （gov5 产出的块同样被比较，跨客户端一致）；出块者 {0:29, 1:27, 2:30, 3:27}，n42-26 的 27 个块全部被 gov5 接受（同一链）；
  头中携带的 111 个 QC 里 n42-26（索引 3）签名出现 56 次（停机期间的 QC 为 {0,1,2}）；AM27 网格 0 违规。
- n42-26 日志：ERROR 0、E12 停机 0、panic 0，"rejected Gov5 direct" 0；WARN 只有超时、断连与已知启动提示。
- gov5 日志（`/tmp/p1-g6/B/logs/`）：error 全部为 "Failed to reconnect to peer"——对象是未启动的 gov5 node 3（30433）以及停机期间的 node 1；
  另有 node 1 的 2 条 "sync anchor: catch-up range not fully anchored"（catch-up 时区间末块尚未锚定，随后补齐）。
- 经 n42-26 RPC 提交的交易只由 n42-26 自己出块时打包（无转发），gov5 node 1 重启后未打包任何交易（gov5 侧行为，未深究）。
  gov5 RPC 的 `safe`/`finalized` 标签返回 null（gov5 侧未实现或不同语义，未核实）。

### 15.6 未完成 / 后续

- 未与 n42-rs 混跑（本次只有 gov5 + n42-26）；n42-rs 是否声明 `hotstuff_direct`/`block_push`、是否接受 n42-26 的 ed25519 PeerId 未验证。
- bodies_by_range/state-sync 仍要求对端列入 `N42_TRUSTED_PEERS`；是否在无绑定模式下对完成同创世 status 握手的 peer 放开需要讨论。
- 无绑定模式下入站直连对任意已连接 peer 开放，只受 BLS/封印校验与现有 gossip 级 DoS 限制约束。
- 交易不转发给其他客户端的 leader；如需要应改为 mempool gossip 或按 BLS 身份映射 leader。
