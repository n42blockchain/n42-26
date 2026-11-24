# devlog-188：对齐当前 Gov5 的 chain-94 块头形态与 requests hash 分叉时间（2026-11-24）

路线归属：[自定义链兼容主线](04-Product-Lines-and-Performance.md)，ETH ECDSA 交易、H2 共识、QMDB bin 状态承诺。

## 背景

本工程的 chain-94 原生路径是照 2026-10-29 之前的 Gov5 写的：那时除创世外每个原生块头都在第 23 位带一个值为零的 `mobileRegistryRoot`，代码把"父块头带这个根"当作原生 profile 的标志。

Gov5 在 10-29 之后改了块头布局（`common/block/header.go`）：第 23 位是 EIP-7843 的 slot number，注册根挪到第 24 位，且只在 MobileAnchor 与 Glamsterdam 都激活后才写；`internal/consensus/hotstuff/adapter.go` 在此之前**拒绝**带注册根的块头（`unexpected MobileRegistryRoot … before MobileAnchor and Glamsterdam activation`）。当前测试网（chain 94，Gov5 main）的块头因此只有 20 个字段，止于 parent beacon root。

用测试网块 `0xdfbd53` 的真实字段验证过两点：本工程的编解码按 20 字段算出的哈希与链上一致（读没有问题）；但以它为父块规范化子块时，因父块无注册根而落入早期 devnet（pre-Shanghai）分支，奖励根、blob 字段和委员会链接全被抹掉，rewards 为 0。即本工程在当前测试网上只能跟块，不能出块。

另一件相关的事：N42-gov5 PR #5 把块头的 requests hash 交给 `pragueTime` 决定（此前 Gov5 从 `pectraTime` 起执行 Prague 规则，但从不写、不校验 requests hash）。测试网计划把 `pragueTime` 设为未来的切换时间 T。n42-rs 对应改动在 N42-rs PR #62。

## 设计决策

### 原生 profile 的判定

`native_profile_parent`：父块头带注册根，**或**带齐 Cancun 四个字段（withdrawals root、blob gas used、excess blob gas、parent beacon root）。原来的 `native_cancun_genesis` 就是后一个条件加上 `number == 0`，去掉高度限制即可。早期 devnet profile 会抹掉这四个字段，所以两种 profile 不会混淆。

考虑过按分叉时间表判定（读 `mobileAnchorTime` / `glamsterdamTime`）。规范化函数在 `n42-network` 里拿不到 chain spec，且父块头的形态本身就是验证者已接受的事实，比配置更直接，未采用。

### 子块的注册根跟随父块

父块无根 → 子块无根；父块为零根 → 子块零根；其他值仍拒绝（注册根更新未实现）。验证者接受哪种形态取决于它们跑的 Gov5 版本，而父块是它们已经接受的块头。

- 当前 Gov5 的链：父块无根，子块无根，正确。
- 旧 Gov5 的链：父块零根，子块零根，与原行为相同。
- 行为变化只有一处：Cancun 创世（无根）的第一个子块，原来写零根，现在不写。这与当前 Gov5 一致，与 10-29 之前的 Gov5 不一致。`cancun_genesis_bootstraps_the_native_first_child` 的断言随之修改。
- 验证者刚从旧 Gov5 升到新 Gov5 时，若本工程恰好先出块，会按父块写零根并被拒，损失一个 view；下一个 Gov5 leader 出无根块后自动恢复。

### 生产者启动时的链头

`authenticated_native_producer_head` 原来只接受零根形态（创世例外）。现在零根与无根都接受，仍以精确的原生哈希鉴别；其他根值或哈希不符照旧拒绝。

### requests hash 的分叉时间

- `n42_consensus::set_gov5_requests_hash_time` / `gov5_requests_hash_active_at`：进程级设置，默认"永不"，即现有行为。
- 时间来源（`bin/n42-node/src/main.rs` `gov5_requests_hash_time`）：创世 `config` 的 `requestsHashTime`（n42-rs 读的同一个键）；没有则取创世 `pragueTime`，前提是它晚于 `N42_GOV5_PRAGUE_TIME`（即 Gov5 的 `pectraTime`）。Gov5 chainspec 里 `pectraTime` 是 Prague 执行规则的时间，`pragueTime` 是 requests hash 的时间；reth 只有一个 Prague 时间，`apply_gov5_prague_time` 用环境变量把它设成前者，在覆盖前读出后者。
- `pragueTime` 不晚于 `pectraTime` 的 chainspec 不视为有 requests hash 时间：那类链此前封的是 Gov5 旧的空 trie 根写法或什么都不封，保持原样。
- 出块（`normalize_native_payload`）：T 之前删去 requests hash（仅允许空请求集，原行为）；T 起保留构建器算出的值与 sidecar，构建器没给则拒绝。
- 校验（`N42Consensus::normalize_header` 与 post-execution 路径）：T 之前对缺失的字段代填 EIP-7685 空哈希（原行为）；T 起不代填，缺字段的块头在内层 Prague 规则下失败，与 Gov5 一致。

进程级全局是因为规范化函数没有 chain spec 入参，本模块已有同类全局（原生块头注册表）。

## 实施细节

- `crates/n42-network/src/gov5_block.rs`：`native_profile_parent` 取代 `native_cancun_genesis`（两处判定）；`normalize_native_payload` 的根来自父块，requests hash 与 sidecar 按分叉时间处理。
- `crates/n42-consensus/src/gov5_native_header.rs`、`lib.rs`：requests hash 时间的设置与查询。
- `crates/n42-consensus/src/adapter.rs`：两处代填加分叉时间条件。
- `bin/n42-node/src/main.rs`：`authenticated_native_producer_head`、`gov5_requests_hash_time`、`apply_gov5_prague_time`。

## 测试

- `a_native_parent_without_registry_root_gets_a_native_child`：测试网块头形态的父块，连续两代子块都保留奖励根与委员会链接、无注册根、无 requests hash。
- `a_native_child_carries_the_requests_hash_from_gov5_prague_time`：T 前一秒不带、T 起带（原生编码以该哈希结尾，21 个字段），sidecar 保留；T 起构建器未给哈希则拒绝。
- `cancun_genesis_bootstraps_the_native_first_child`：创世子块改为无根。
- `native_producer_preserves_rewards_committee_and_mobile_header`（未改）：零根父块仍得零根子块。
- `native_producer_accepts_a_head_authenticated_with_a_zero_registry_root_or_none`、`the_requests_hash_time_is_the_gov5_prague_time_after_pectra`。
- `gov5_prague_header_without_requests_hash_is_accepted_when_no_requests_were_produced`：增加 T 起不代填的断言。

## 状态

- `cargo clippy --workspace --all-targets --locked -- -D warnings` 通过；受影响 crate 的单元测试通过（数字见 PR 描述）。
- **未验证**：没有与真实 Gov5 节点联调。出块形态的依据是 Gov5 源码与测试网块头的字段；本工程产出的块是否被当前 Gov5 接受，需要一条 Gov5 + n42-26 的 Cancun 创世混合链来确认，T 的跨越同样需要。
- requests hash 部分依赖 N42-gov5 PR #5，该 PR 尚未评审，设计若变需同步调整。

## 后续

- 24 字段编解码（第 23 位 slot number、第 24 位注册根）：Glamsterdam 激活时才需要，本次未做；编解码仍以 23 字段为上限。
- 本地 Gov5 + n42-26 混合链（Cancun 创世，跨 T）。
- `N42_GOV5_PRAGUE_TIME` 仍需手工与创世的 `pectraTime` 保持一致，可改为直接读创世。

## 进度记录（2026-11-24 续）：与真实 Gov5 的本地混合链

补上"未验证"一项。单机 loopback，全新 Cancun 创世，不用任何线上数据：3 个 Gov5（N42-gov5 PR #5，`42cacc0e`）+ 4 个本工程节点（`N42_GOV5_NATIVE_PRODUCER=1`、`N42_GOV5_PRAGUE_TIME=0`），七人固定委员会，1 s 出块。创世的 fork 字段取 chain 94 的组合（Shanghai/Cancun/Pectra/Osaka/Fusaka 于 0，`unchangedAccountNoOpTime`、`storageNetChangeTime` 于 0，奖励与 committee pool 开启），`pragueTime = T` 设在起链后数分钟。本工程节点从 Gov5 导出的第 0 块启动（`n42-qmdb-export` 与 `scripts/gov5-export-genesis-range.go`）。每 10 秒经 Gov5 RPC 发一笔零小费转账。

这是轻负载的正确性检查，不是性能数据，也不替代多机或长时间验收。

### 第一轮：T 之后 Gov5 的块无法导入（已修复）

- T 之前 406 块：Gov5 出 174、本工程出 232，全部 20 字段，逐块哈希无差异。本 PR 第 1 部分（原生 profile 与注册根）由此在真实 Gov5 上得到确认。
- T 之后：本工程出的块为 21 字段且被 Gov5 接受；轮到 Gov5 出块时本工程全部超时（每轮约 45 s），链未分叉。
- 日志：`could not reconstruct gov5 execution payload … finalized block 411 requires execution requests absent from finalized-range v1`。
- 原因：`replay_import.rs` 的 `validate_v1_payload_inputs` 对任何带 `requests_hash` 的 live 块头返回 `MissingRequests`，因为 Gov5 的块不带 request 列表。这条检查不在本 PR 原先改动的出块路径上，单元测试覆盖的是"本工程出块"，没有覆盖"导入 Gov5 的块"。
- 修复：Gov5H2 profile 下 `requests_hash == EMPTY_REQUESTS_HASH` 视为可重建（空列表不需要来源）；其他值仍返回 `MissingRequests`。测试 `a_gov5_block_committing_to_no_requests_becomes_a_prague_payload`。
- 搭建中的一次失误（与代码无关）：第一版创世的 alloc 缺 Prague 系统合约，Gov5 以 `System contract address 0x0000…7002 has empty code` 拒绝第 1 块；补入 alloc 后正常。

### 第二轮（含修复）：跨 T 正常

| 项目 | 结果 |
|---|---|
| 块数 | 380（T 前 224，T 后 156） |
| Gov5 与本工程逐块哈希 | 0 处不同 |
| T 前块头 | 全部 20 字段：Gov5 出 96，本工程出 128 |
| T 后块头 | 全部 21 字段（带 EIP-7685 空哈希）：Gov5 出 67，本工程出 89 |
| 出块分布 | 七个验证者各 54–55 块 |
| 跨 T 的块间隔 | T-1 s、T+0 s、T+1 s，无停顿 |
| 36 笔转账的回执 | 两端完全一致（块号、块哈希、状态、gas） |
| Gov5 日志中的 BAD BLOCK | 0 |

### 仍未覆盖

- 合约创建与调用、带 execution requests 的块（chain 94 不产生）。
- 节点重启、停掉部分节点、多机、长时间运行。
- 三个客户端同在一条链上（本次是 Gov5 + 本工程；Gov5 + n42-rs 另行跑过）。
