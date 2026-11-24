# devlog-188：chain-94 原生路径按以太坊规则对齐当前 Gov5（2026-11-24）

路线归属：[自定义链兼容主线](04-Product-Lines-and-Performance.md)，ETH ECDSA 交易、H2 共识、QMDB bin 状态承诺。

## 前提

链尚未正式上线，主网与测试网的历史都会重新 replay。因此本次的目标是**从第 0 块起按以太坊的规则**，不为早先二进制写出的块保留兼容路径，也不引入任何切换时间。三个客户端同步调整：Gov5 见 N42-gov5 PR #5，n42-rs 见 N42-rs PR #62。

本文件早先的版本描述过一个"从 Gov5 `pragueTime` 起才携带 requests hash"的时间开关，已在同一 PR 内撤掉，此处按最终状态记录。

## 一、原生 profile 与注册根

### 缺陷

原生路径是照 2026-10-29 之前的 Gov5 写的：那时除创世外每个原生块头都在第 23 位带一个零值的 `mobileRegistryRoot`，代码把"父块头带这个根"当作原生 profile 的标志。当前 Gov5 把注册根放在第 24 位（第 23 位是 EIP-7843 的 slot number），只在 MobileAnchor 与 Glamsterdam 都激活后才写，在此之前带根的块头会被拒绝（`internal/consensus/hotstuff/adapter.go`）。以当前 Gov5 出的块为父块时，规范化落入早期 devnet 分支，奖励根、blob 字段和委员会链接被抹掉。

### 修复

- `native_profile_parent`（`crates/n42-network/src/gov5_block.rs`）：父块头带注册根，或带齐 Cancun 四个字段。早期 devnet profile 会抹掉这四个字段，两者不会混淆。
- 子块的注册根跟随父块：无根 → 无根；零根 → 零根；其他值仍拒绝。
- `authenticated_native_producer_head`（`bin/n42-node/src/main.rs`）：零根与无根两种形态都接受，仍以精确的原生哈希鉴别。
- 行为变化：Cancun 创世的第一个子块不再写零根。

## 二、requests hash 按 EIP-7685

### 缺陷

Gov5 此前从 `pectraTime` 起执行 Prague 规则，但 live 块头从不写 requests hash，replay 的块头则写空 trie 根。本工程相应地：出块时删去该字段；校验时对缺失的字段代填空哈希；replay-v2 块头要求空 trie 根；导入 Gov5 的块时凡带该字段一律 `MissingRequests`。

### 修复

- 出块（`normalize_native_payload`）：保留构建器算出的 requests hash 与 sidecar，不再删去。
- 校验（`N42Consensus::normalize_header` 与 post-execution 路径）：不再代填。Prague 的块头没有该字段即 `RequestsHashMissing`，Prague 之前带该字段即 `RequestsHashUnexpected`，与以太坊相同。
- replay-v2 块头（`validate_gov5_replay_v2_header`）：接受"无"或 EIP-7685 空哈希；具体该是哪一个由内层规则按 Prague 时间判定。空 trie 根不再接受。
- 导入（`replay_import.rs` `validate_v1_payload_inputs`）：块头承诺的是空请求列表时照常重建（空列表不需要来源）；其他值仍 `MissingRequests`。
- `decode` 路径里"原生块头无 requests hash 时以空请求集代替"的分支已删除。

## 三、状态根规则跟随创世配置

### 缺陷

Gov5 有两条根规则：`unchangedAccountNoOpTime`（块碰过但未改变的账户不写叶子）与 `storageNetChangeTime`（写了又恢复原值的槽不写叶子）。本工程的 `TrackingExecutor`（`crates/n42-execution/src/restored_slots.rs`）无条件地把这两类补回 bundle，即始终按"规则未开"计算。规则打开的链上，一笔零金额、零小费、发给已有账户的转账就使本工程的状态根与 Gov5、n42-rs 不同，本工程节点停在该块。

`bin/n42-node/src/main.rs` 里那条"创世未从 0 激活 `unchangedAccountNoOpTime`"的启动告警写于 evm2 迁移之前，迁移后行为反了过来，告警却没有跟着改；本次之后它重新成立。

### 修复

`TrackingBlockExecutorFactory::with_gov5_root_rules` 从创世 `config` 读这两个时间；块的时间戳不早于某条规则的时间时，该执行器不为它记录任何东西，bundle 即 evm2 的原样结果，也就是以太坊客户端的自然行为。chain 94 的 Gov5 配置把两者都设为 0。

没有把补回去的那套机制整体删除：它连着 15 个文件与一批测试，适合在 replay 完成、旧链不复存在之后清理。Gov5 一侧的旧路径同理。

## 测试

单元与集成测试：

- `a_native_parent_without_registry_root_gets_a_native_child`、`cancun_genesis_bootstraps_the_native_first_child`、`native_producer_preserves_rewards_committee_and_mobile_header`、`native_normalization_edits_only_the_header_and_matches_a_full_rebuild`（`gov5_block.rs`）。
- `gov5_prague_header_is_held_to_the_requests_hash`（`adapter.rs`）。
- `a_gov5_block_committing_to_no_requests_becomes_a_prague_payload`（`replay_import.rs`）。
- `a_touched_and_unchanged_account_follows_the_gov5_rule_time`（`parallel_build.rs`）：规则在 0 与在未来两种配置下，stock 导入、批处理（direct 开与关）、并行导入四条路径给出同一个 bundle。
- `native_producer_accepts_a_head_authenticated_with_a_zero_registry_root_or_none`（`main.rs`）。
- 集成测试 `follower_parallel_import_is_validated_by_the_engine_and_matches_serial` 与 `gov5_replay_v2_preserves_nonempty_body_and_exact_decoding` 的断言随行为修改。前者是在 CI 上才发现的：最初只跑了 `--lib`。

`cargo clippy --workspace --all-targets --locked -- -D warnings` 通过；`cargo test --workspace --locked`：1937 通过，1 失败（`memory_watch::tests::resident_memory_is_read`，读 `/proc/self/statm`，只在 Linux 上通过，与本次无关）。

## 本地三客户端同链

单机 loopback，全新 Cancun 创世，不用线上数据。七人固定委员会：验证者 0–2 为 Gov5（PR #5），3–4 为本工程，5–6 为 n42-rs（PR #62）。三个客户端读同一份创世：Shanghai/Cancun/Pectra/Prague/Osaka/Fusaka 与两条根规则全部于 0 激活，没有任何切换时间。本工程节点从 Gov5 导出的第 0 块启动。

| 项目 | 结果 |
|---|---|
| 块数 | 271 |
| 三个客户端逐块哈希 | 0 处不同 |
| 块头 | 从第 1 块起全部 21 字段（带 EIP-7685 哈希）：Gov5 出 121，本工程出 76，n42-rs 出 74 |
| 22 种交易形态 × 2 遍，回执三端比对 | 44 通过，0 失败 |
| 重启演练 | 12 通过，0 失败 |
| Gov5 日志中的 BAD BLOCK | 0 |

交易形态：普通转账、legacy、零小费、零金额、发给新地址、自转、调用预编译、合约部署、存储写入、写入后恢复原值、回滚、gas 用尽、事件日志、自毁、带金额自毁。演练：三种客户端各停一个再恢复；两个 Rust 客户端各停一个（恰为法定人数 5）；全停全起后锚定块七节点一致。

这是轻负载的正确性检查，不是性能数据，也不替代多机或长时间验收。

过程中起真链才暴露的三个问题（均已在上文修复）：Gov5 的块带 requests hash 后无法导入；零金额转账导致根不同；n42-rs 一侧的同类问题见其 PR。

## 未覆盖

- replay 产出的历史块没有在本工程上实际导入过：Gov5 的 replay 工具刚改为新格式，尚未重跑。
- 多机、长时间、较高负载；带 execution requests 的块（chain 94 不产生）。
- 24 字段编解码（第 23 位 slot number、第 24 位注册根）没有做，Glamsterdam 激活时才需要。
- `N42_GOV5_PRAGUE_TIME` 仍需手工与创世一致，可改为直接读创世。
