# 开发日志 40 — Pipeline 时序深度分析与 TPS 优化方案

## 日期: 2026-03-09

## 一、Pipeline 时序图（一个完整出块周期）

### 1.1 当前 Pipeline（2s slot, 24K tx/block, ~12K TPS 稳态）

```
时间轴 (ms)    LEADER (validator-0)              FOLLOWER (validator-1)           CONSENSUS
─────────────────────────────────────────────────────────────────────────────────────────────
T=0            ┌─ slot boundary ──────────────── slot boundary ──────────────── slot start
               │
T=0-35ms       │ PACK: pool→tx select (35ms)
               │  └ evm_exec: 26ms
               │  └ pool_overhead: 8ms
T=35-65ms      │ FINISH: state root (30ms)
               │
T=65-68ms      │ SERIALIZE payload (3ms)
T=68-85ms      │ COMPRESS zstd (17ms)
T=85ms         │ DIRECT PUSH to 6 peers (0ms)    ···network latency (~100ms)···
               │
T=85ms         │ BlockReady → consensus  ─────────────────────────────────────→ Proposal sent
               │                                                                 │
T=85-95ms      │ Leader new_payload ──────┐                                      │
               │  (cache hit=3ms)         │                                      │ R1: Prepare
T=95ms         │ import done (66ms total) │                                      │ collection
               │                          │      T≈185ms                         │ (~245ms)
               │                          │      ┌─ DECOMPRESS (3ms)             │
               │                          │      │  DESERIALIZE (3ms)            │
               │                          │      │                               │
               │   ┌──────────────────────┤      │  Follower new_payload ──┐     │
               │   │ PARALLEL:            │      │  ┌─ EVM exec (691ms) ──┤     │
               │   │ consensus voting     │      │  │  ≡≡≡≡≡≡≡≡≡≡≡≡≡≡≡≡≡ │     │
               │   │ & follower import    │      │  │  SEQUENTIAL!        │     ├─ R1 done
               │   │ happen concurrently  │      │  │  can't start until  │     │  (~330ms)
               │   │                      │      │  │  full block arrives │     │
               │   │                      │      │  └─ root (54ms) ──────┤     │ R2: Commit
               │   │                      │      │                       │     │ collection
               │   │                      │      │  total: 770ms         │     │ (~149ms)
               │   │                      │      └─────────────────────────     │
               │   │                      │                                     │
               │   └──────────────────────┤      T≈955ms                       ├─ R2 done
               │                          │      import done                    │  (~479ms)
               │                          │                                     │
               │                          │                                     │ Decide
T≈1374ms       │ COMMITTED ←──────────────┤      T≈1374ms COMMITTED  ←─────────┘ broadcast
               │                          │                                     │
               │ FCU (39ms) ─────────┐    │      FCU (498ms) ──────────┐       │
               │                     │    │       (waits for engine     │       │
               │                     │    │        persist queue)       │       │
T≈1413ms       │ finalized ←─────────┘    │                            │       │
               │                          │      T≈1872ms              │       │
               │                          │      finalized ←───────────┘       │
               │                          │                                     │
               │ ┌─ PERSIST (async) ──────┤      ┌─ PERSIST (async) ──────┐    │
               │ │ p50=720ms, p95=1.1s    │      │ p50=720ms, p95=1.1s   │    │
               │ │ NOT blocking import    │      │ NOT blocking import   │    │
               │ └────────────────────────┤      └────────────────────────┘    │
               │                          │                                     │
T=2000ms       └─ next slot ──────────────└── next slot ──────────────────── next slot
```

### 1.2 关键发现

**BUG: Compact Block 未生效！**

日志显示所有 follower import 都是 `compact_injected=false, has_compact_block=false`。

根因：revert payload builder 时丢失了 `store_broadcast_execution()` 调用。当前只存了 `store_payload_execution`（给 leader 自己的 new_payload），没存 `store_broadcast_execution`（给 compact block 序列化）。

**影响极大**：follower import 从应有的 3-22ms 退化到 600-1000ms，是 **当前最大瓶颈**。

## 二、时间开销分解（实测数据）

### 2.1 Leader 路径（一切正常）

| 阶段 | 耗时 | 备注 |
|------|------|------|
| TX Pool Packing | 35ms | evm 26ms + pool 8ms |
| State Root (finish) | 30ms | 同步计算 |
| Serialize | 3ms | serde_json |
| Compress (zstd) | 17ms | 97% 压缩率 |
| Network Send | 0ms | 异步非阻塞 |
| **Leader Build Total** | **85ms** | |
| Leader new_payload | 3ms | cache hit (evm=0, root=3) |
| Leader FCU | 39ms | 引擎空闲时 |
| **Leader Import Total** | **42ms** | |

### 2.2 Follower 路径（Compact Block 未生效 — 当前现状）

| 阶段 | 耗时 | 备注 |
|------|------|------|
| Network Receive | ~100-150ms | libp2p 延迟 |
| Decompress + Deserialize | 6ms | |
| **EVM Re-execution** | **600-1000ms** | ← **瓶颈！不应发生** |
| State Root | 0-54ms | |
| **Follower Import Total** | **600-1050ms** | |
| FCU Wait | 300-850ms | 引擎持久化队列 |
| **Follower Finalize Total** | **900-1900ms** | |

### 2.3 Follower 路径（Compact Block 生效 — 应有表现）

| 阶段 | 耗时 | 预期 |
|------|------|------|
| Network Receive | ~100-150ms | 不变 |
| Decompress + Deserialize | 6ms + injection 5ms | |
| **EVM (skipped)** | **0ms** | cache hit |
| State Root | 0-5ms | Synchronous on cached state |
| **Follower Import Total** | **3-22ms** | **从 1000ms → 20ms** |
| FCU Wait | 30-100ms | 引擎不积压 |
| **Follower Finalize Total** | **33-122ms** | |

### 2.4 Consensus（固定开销，不随块大小变化）

| 阶段 | 耗时 | 备注 |
|------|------|------|
| Leader Proposal Delay | ~1300-1560ms | 等待 slot 内投票窗口 |
| R1 (Prepare) Collection | 245-305ms | 5/7 quorum |
| R2 (Commit) Collection | 62-149ms | 5/7 quorum |
| **Consensus Total** | **1650-1950ms** | 固定，不可优化 |

## 三、并行关系图

```
       SEQUENTIAL (关键路径)            PARALLEL (非阻塞)
       ─────────────────────           ──────────────────────

  ┌──  Leader Build (85ms)
  │      ↓
  │    Leader Import (42ms)        ┐
  │      ↓                         │  Consensus R1+R2 (~400ms)
  │    Network → Follower          │
  │      ↓                         │  Follower Import (3-22ms*)
  │    Consensus Decide            ┘    *with compact block
  │      ↓
  │    FCU Finalize                    Persist (async, 720ms)
  └──  DONE

  关键路径 = Build(85) + Consensus(1950) = 2035ms > 2000ms slot

  BUT: Build overlaps with previous slot's consensus tail
       → effective = Consensus(1950) + FCU(39) = 1989ms < 2000ms ✓
```

**真正的关键路径是 consensus 投票时间 (~1950ms)**，占据了 2000ms slot 的 97.5%。

当 compact block 生效时，follower import 不在关键路径上（它与 consensus 投票并行发生）。

当 compact block 未生效时，follower import (600-1000ms) 可能超出 consensus 窗口，导致 FCU 等待 → block_time 膨胀。

## 四、瓶颈优先级排序

### 4.1 P0: 修复 Compact Block（立即，预期 +50% TPS）

**问题**：`store_broadcast_execution()` 调用缺失
**影响**：follower import 从 3-22ms 退化到 600-1000ms
**修复**：在 `reth/crates/ethereum/payload/src/lib.rs` 的 payload cache 存储代码中添加 `store_broadcast_execution` 调用
**预期**：
- Follower import 从 ~800ms → ~15ms
- Block_time 在高负载下从 3.8s → 2.0s（不再膨胀）
- Sustained TPS 从 12K → 18-20K（block_time 不膨胀，每块仍打 40K tx）

### 4.2 P1: 提升 MAX_TXS_PER_BLOCK（简单配置）

当前 MAX_TXS=40K 限制了每块 tx 数量。payload builder 日志显示 40K tx 打包仅需 88ms、EVM 65ms、state root 30-49ms → 总共 ~150ms。

**方案**：提升到 80K-100K
**预期**：单块 TPS 从 20K → 40K-50K
**前提**：compact block 必须生效（否则 follower 无法在 2s 内 import 80K tx 块）

### 4.3 P2: 缩短 Consensus 投票时间（最大 TPS 天花板）

当前 consensus 占据 1950ms/2000ms = 97.5%。这是 TPS 的最终天花板。

**方案 A — Pipelined Consensus**：
```
Slot N: [Build N] [Consensus N-1 finalizing] [Build N+1 starts]
                                              ↑ overlap
```
在 Block N 的共识投票期间，开始 Block N+1 的构建。

**方案 B — 1s Slot**：
如果每块 tx 数足够（80K+），1s slot 可以直接翻倍 TPS。
但需要 consensus 在 ~950ms 内完成 → 需要降低 BASE_TIMEOUT 或优化网络延迟。

**方案 C — 减少 Proposal Delay**：
当前 leader proposal delay ~1300-1560ms（等待 slot 内的投票窗口）。
如果 build 完成就立即 propose（不等 slot boundary），可以缩短 ~500ms。

### 4.4 P3: FCU 延迟尖峰优化

FCU 延迟范围 18-853ms，尖峰 853ms 说明 reth 引擎持久化积压。

**方案**：
- 将 FCU 改为异步非阻塞（不等待引擎确认）
- 或调整 reth persist threshold
- 这不在关键路径上（consensus 并行），但影响下一块的 build 启动时机

### 4.5 P4: 压测工具 v10

当前注入能力 ~17K TPS，制约了对更高 TPS 的测量。

## 五、优化实施路线

### Phase 1: 修复 Compact Block + 提升 MAX_TXS（预期 20K TPS sustained）
- 添加 `store_broadcast_execution()` 调用（1 行代码）
- 将 MAX_TXS 默认值从 40K → 80K
- 重新压测验证 follower import 时间

### Phase 2: 减少 Slot 浪费（预期 25-30K TPS）
- 移除 slot boundary 强制对齐（当前等待到 2s 整数倍）
- Leader build 完成后立即 propose（不等时间窗口）
- 减少 pool 迭代深度上限

### Phase 3: 1s Slot（预期 40-50K TPS）
- BLOCK_INTERVAL_MS=1000
- 需要 consensus 在 ~900ms 完成
- 每块 40K-50K tx → 40K-50K TPS

### Phase 4: 压测 v10 + Pipelined Consensus（预期 50K+ TPS）
- 并行签名线程池，突破 17K 注入瓶颈
- Pipelined consensus: N+1 块构建与 N 块投票重叠

## 六、Compact Block 修复验证

### 6.1 Bug 修复
在 `reth/crates/ethereum/payload/src/lib.rs` 添加 `store_broadcast_execution` 调用（在 `store_payload_execution` 之前 clone）。同时将 MAX_TXS 默认值从 40K → 80K。

### 6.2 验证结果

**修复前后完整对比（7 nodes, 2G gas, 2s slot）：**

| Step | Target | 修复前 TPS | 修复前 block_time | **修复后 TPS** | **修复后 block_time** |
|------|--------|-----------|------------------|---------------|---------------------|
| 4 | 7500 | 11,868 | 2.0s | **13,891** | **2.0s** |
| 5 | 10000 | 11,885 | 2.0s | **13,781** | **2.0s** |
| 6 | 15000 | 16,728 | 3.8s | **20,941** | **2.6s** |
| 7 | 20000 | 9,277 | - | **19,083** | **2.6s** |
| 8 | 30000 | 16,201 | 5.4s | **20,305** | **3.0s** |

**关键改进：**
- Sustained 14K TPS @ 2.0s block_time（+17%）
- Peak 20K TPS（block_time 从 3.8s → 2.6s，-32%）
- 30K target: 20.3K TPS @ 3.0s（之前 16.2K @ 5.4s，TPS +25%, block_time -44%）
- fail_rate=0.0% 全程
- 553+ 块连续出块，7/7 节点存活

**Follower import 时间改进：**
- 修复前：600-1000ms（EVM 重执行）
- 修复后：30-55ms（compact block cache hit，EVM=0ms）
- 提升 **20-30 倍**

### 6.3 当前瓶颈（修复后重新排序）

1. **压测注入能力** (~20K TPS 天花板) — 制约测量
2. **Consensus 投票时间** (1950ms/2000ms slot = 97.5%) — 理论天花板
3. **Compact block 序列化** (17ms compress + 3ms serialize = 20ms) — 次要
4. **Leader build** (85ms) — 不在关键路径
5. **Follower import** (30-55ms) — 已修复，不再是瓶颈
