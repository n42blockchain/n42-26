# 开发日志 32 — Compact Block 实现 + Delayed State Root 实验

## 日期: 2026-03-08

## 一、Compact Block (Phase A2) 实现

### 设计决策

Leader 出块时将 `(BlockExecutionOutput, senders)` 缓存到 `BROADCAST_CACHE`，序列化为 `CompactBlockExecution`（serde_json + zstd 压缩），随 BlockDataBroadcast 广播给 follower。Follower 收到后注入 payload_cache，使 reth 的 `new_payload` 命中缓存，跳过 EVM 重执行。

**关键设计**:
- `CACHE` vs `BROADCAST_CACHE` 双槽设计：避免 leader 的 `take` 消费掉广播数据
- serde_json（非 bincode）：Receipt 的 `alloy_serde::quantity` 用 hex 格式，与 bincode 不兼容
- zstd 压缩：532KB → 19KB（97% 压缩率）
- 安全性：state root 仍由 reth 验证（即使 cache hit 也走 Synchronous SR）

### 修改文件
- `reth/crates/evm/evm/src/payload_cache.rs` — 双槽 CACHE/BROADCAST_CACHE
- `reth/crates/ethereum/payload/src/lib.rs` — Leader 缓存 broadcast copy
- `n42-node/src/orchestrator/mod.rs` — BlockDataBroadcast + CompactBlockExecution
- `n42-node/src/orchestrator/execution_bridge.rs` — 序列化/反序列化/注入逻辑
- `n42-node/src/orchestrator/consensus_loop.rs` — background_import 注入
- `n42-node/src/orchestrator/observer.rs` — observer import 注入

### 压测结果 (7 nodes, 2s slot, 500M gas)

| Target TPS | Effective TPS | Fail Rate | Gas Util | Max Block TPS |
|---|---|---|---|---|
| 500 | 464 | 3.4% | 4.1% | 547 |
| 1,000 | 924 | 0.3% | 7.8% | 1,148 |
| 2,000 | 1,706 | 0.1% | 14.4% | 2,156 |
| 3,000 | 2,361 | 0.6% | 20.1% | 2,952 |
| 5,000 | 3,430 | 0.0% | 28.9% | 5,560 |
| 7,500 | 4,417 | 0.0% | 37.0% | 6,607 |

**Follower import timing** (with compact block):
- avg=57ms, p50=54ms, p95=103ms, max=146ms
- EVM: 0ms (cache hit), root_ms: 0ms (Synchronous SR on cached state)
- 瓶颈已不在 follower import

## 二、Delayed State Root 实验（已放弃）

### 设计思路
将 state root 计算从 `new_payload` 关键路径移除，后台异步验证。
传递空 `TrieUpdates::default()` 给 `spawn_deferred_trie_task`，主路径直接信任 leader 的 state root。

### 失败原因
**空 TrieUpdates 导致 sparse trie 状态损坏**:
1. Delayed SR 传递空 TrieUpdates → reth 的 anchored sparse trie 不更新
2. 当后续某块没有 compact block 数据（leader 自己的块）时，StateRootTask 使用过期的 sparse trie
3. 计算出错误的 state root → block rejected → 链永久卡死

### 尝试过的修复
1. **v1 (后台 compute_state_root_serial)**: 偶尔 MISMATCH（overlay 状态推进），且在某些情况下导致链卡死
2. **v2 (主线程预创建 provider)**: 性能反而大幅下降，2000 TPS 即卡死

### 结论
- Compact block cache hit 时 root_ms=0（Synchronous SR 在 cached state 上几乎免费）
- **没有必要延迟 state root** — 它已经不在关键路径上了
- 已移除 delayed state root 代码

## 三、性能分析

### 当前瓶颈分析
- Follower import: ~57ms avg（compact block 已优化到位）
- Gas 利用率仅 37% @ 7500 TPS — **链还没被推满**
- 压测工具是当前限制因素（nonce 管理、RPC 并发）
- 理论极限（500M gas / 2s slot）: ~23809 tx/block = ~11905 TPS

### 对比基线
| 指标 | 日志-31 基线 (4s slot) | 当前 (2s slot + compact) |
|---|---|---|
| Peak sustained TPS | ~5952 | 4417 |
| Max block TPS | 5952 | 6607 |
| Follower import avg | 209ms | 57ms (3.7x 改进) |
| Slot time | 4s | 2s |

## 四、下一步计划
1. 提高压测工具能力（更多 accounts、更高并发）推到 gas limit
2. Phase 3: Hot state cache / optimize reth import — 解决 disk I/O 瓶颈
3. Phase 4: 1s slot time（需要 import < 500ms）
