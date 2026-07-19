# Devlog 99: S5 坏块缓存与恢复路径过滤

日期：2026-07-18
范围：`codex-task-sync-from-gov5-2026H1.md` / S5

## 审计结论

S5 描述的问题在 n42-26 中真实存在：

1. `new_payload` 的确定性 `Invalid` 只写日志，未按 block hash 记忆。相同块经 sync fan-out、
   catch-up、direct-push/GossipSub 或 observer sync 再次到达时，会重新提交给 reth。
2. direct-push 与 GossipSub 的短期去重只依赖 `pending_block_data`。它既不表达执行失败原因，
   也覆盖不了缓存淘汰后的重传及同步响应路径。
3. committed 后台导入、follower/leader eager import、sync retry 和 observer 各自直接调用
   `new_payload`，没有共享拒绝集合。
4. 审计过程中额外发现：wire envelope 的 `block_hash` 在注入 compact execution output 和调用
   reth 前没有与解码后的 execution payload hash 绑定。恶意同步 peer 可用错误 payload 诱导
   `Invalid`，再把本来正确的声明哈希写入坏块缓存。

## 修复

- 新增进程内 `BadBlockCache`：`B256 -> InvalidPayload(reason)`，共享 `Arc<Mutex<_>>`，LRU
  上限 512。validation error 按 UTF-8 边界截到 256 bytes；插入、命中、淘汰和当前大小均有
  metrics。
- 只有 Engine API `new_payload` 明确返回 `PayloadStatusEnum::Invalid` 才写入缓存。
  `Syncing`、`Accepted`、Engine transport/internal error、超时/IO、解压/JSON 错误及 FCU 结果
  都不写入，避免把 unknown-ancestor 或本地暂态故障永久化。
- validator 路径在 block-data 到达、sync response、sync import、sync retry、committed 后台导入
  以及 leader/follower eager import 前检查缓存；observer 的 gossip 与 sync import 使用同一规则。
- sync response 首次遇到确定性拒绝后不再继续写 committed metadata，也不会把该块计为有用的
  catch-up 结果；后续 fan-out 响应在调用 reth 前被过滤。
- 每条外部 payload 路径先验证 `execution_data.block_hash() == envelope.block_hash`。不一致时
  丢弃并计数，但绝不写坏块缓存；compact execution output 也不会被注入声明哈希。
- 缓存刻意不持久化。节点/reth 升级后执行规则可能变化，重启时重新验证比跨版本保留旧 verdict
  更安全。

## 兼容性

本修复不改共识消息、签名域、wire 编码或磁盘格式，可滚动升级。新旧节点对区块有效性的判定
不变；新节点只会减少对已获得确定性 `Invalid` verdict 的重复提交。

新增 metrics：

- `n42_bad_block_cache_inserts_total{source}`
- `n42_bad_block_cache_hits_total{source}`
- `n42_bad_block_cache_evictions_total`
- `n42_bad_block_cache_entries`
- `n42_sync_bad_blocks_filtered_total`
- `n42_sync_blocks_import_rejected_total`
- `n42_block_data_payload_hash_mismatch_total`

## Rotor 生产调用方核对

devlog-55 的 Rotor 不是死代码：

- `ConsensusService::run` 启动时调用 `set_validator_context(my_index, validator_count)`，epoch
  切换后也会刷新上下文。
- `EngineOutput::Broadcast` 的真实事件处理调用 `broadcast_via_rotor`；该函数计算 assignment，
  向 relay validator peer 直发，并保留 GossipSub fallback。
- network service 收到 leader 的 direct proposal 后调用 `maybe_relay_proposal`，只有真实 leader
  source 才向本 relay 的 targets 转发，避免转发环。
- block payload 数据路径另行向 `all_validator_peers()` 可靠直推并 GossipSub 广播，不依赖一个
  未接线的 peer 注册表。

因此 N3 在 n42-26 已覆盖，本任务无需修改 Rotor。

## 验证

- `cargo check -p n42-consensus-service --all-targets`：通过。
- `cargo clippy -p n42-consensus-service --all-targets -- -D warnings`：通过。
- `cargo test -p n42-consensus-service --lib`：127 passed。
- `cargo check --all-targets`、`cargo clippy --all-targets -- -D warnings`、
  `cargo test --workspace`：全绿；其中 `chaos_7node` 12/12 通过。
- 新增单测覆盖：相同 Invalid 第二次不调用 `new_payload`、`Syncing` 两次仍提交且不缓存、
  direct-push 命中过滤、512-capacity 等价小容量 LRU 淘汰/刷新、reason 替换与 UTF-8 截断、
  envelope/payload hash mismatch 不调用 reth 且不投毒。
- E2E Scenario 4 correctness（1/3/5 validators）：全部 V1-V5 通过。高度分别为 13、
  13/13/13、12/12/12/12/12；采样区块哈希一致；平均间隔 7.9s/8.0s/8.0s。
- E2E Scenario 9 recovery（60s 实弹缩短档，500ms block interval，15s crash，10s downtime）：
  node-2 从高度 57 原数据目录重启，6.067s 完成 catch-up；最终三节点高度均为 127，
  10 个采样哈希一致，354 笔交易验证通过，7/7 checks 通过。

全部 P0 的组合 E2E 由 S1-S5 ordered validation 分支统一记录。
