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
