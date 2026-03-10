# 开发日志-44: 24K 块 + 限速 12K TPS 稳定性验证

## 背景

目标：验证 2s slot, 24K tx/block cap, 12K TPS 注入能否稳定运行 120s。
前置修复：blast 限速 bug（per_rpc_tps=0）、补全 FCU/Broadcast 时序日志。

## 修复内容

### 1. Blast 限速 Bug
- **根因**：上次测试使用未重新编译的旧二进制（代码已改但未 build）
- **验证**：重新编译后 `per_rpc_tps=1714`（12000/7），`batch_interval_ms=250`
- **效果**：`injection_tps=13700`（稳定），`pool_pending` 不再无限堆积

### 2. 新增时序日志
- **N42_FCU**：所有 FCU 调用都记录 elapsed_ms（原来只记 >5ms）
- **N42_FCU_RETRY**：retry FCU 调用独立计时
- **N42_BROADCAST**：分别记录 send_ms（直推）、gossip_ms（GossipSub）、total_broadcast_ms

### 3. MAX_TXS_PER_BLOCK 调整
- 40000 → 24000

## 测试结果

### 摘要

| 指标 | 数值 |
|------|------|
| **overall_tps** | **11,536** |
| **avg_block_tps** | **11,291** |
| p50_tps | 12,000 |
| p95_tps | 12,000 |
| **avg_block_time** | **2.0s** ✅ |
| max_tx_in_block | 24,000 |
| gas_utilization | 23.7% |
| total_tx | 1,130,588 |
| blocks (load phase) | 50 |
| injection_tps | 13,700 (稳定) |
| injection_err | 0 |
| pool_pending | 25K-50K (稳定，未饱和) |

### 评估结论：**能稳定接近 12K TPS**

- overall_tps=11,536（vs 目标 12,000，差距 3.8%）
- avg_block_time=2.0s（完美匹配 2s slot）
- 120s 内无 chain stall、无 timeout、无 nonce 错误
- **所有 50 个有效块都是 24K tx**（hit cap）

差距原因：首块较小（22K tx，pool 还没满），以及 pool queued 有少量 nonce gap（~5K）。

## Leader Block 完整时序表 (validator-0, 24K tx blocks)

| View | tx_count | packing_ms | evm_ms | pool_oh | finish_ms(SR) | ser_ms | compress_ms | broadcast_ms | build_ms | R1_collect | R2_collect | total |
|------|----------|------------|--------|---------|---------------|--------|-------------|-------------|----------|------------|------------|-------|
| 77   | 22145    | 30         | 25     | 5       | 29            | 3      | 15          | 0            | 91ms     | 460ms      | 77ms       | 1776ms |
| 84   | 24000    | 31         | 25     | 5       | 28            | 3      | 17          | 0            | 93ms     | 507ms      | 83ms       | 1400ms |
| 91   | 24000    | 32         | 26     | 5       | 28            | 4      | 18          | 0            | 96ms     | 565ms      | 43ms       | 1737ms |
| 98   | 24000    | 36         | 28     | 7       | 32            | 4      | 17          | 0            | 121ms    | 484ms      | 161ms      | 1920ms |
| 105  | 24000    | 35         | 28     | 7       | 30            | 3      | 17          | 0            | —        | 345ms      | 355ms      | 1829ms |
| 112  | 24000    | 33         | 26     | 7       | 29            | 3      | 16          | 0            | —        | 627ms      | 117ms      | 1785ms |
| 119  | 24000    | 31         | 25     | 6       | 29            | 3      | 24          | 0            | —        | 382ms      | 122ms      | 1738ms |
| 126  | 24000    | 38         | 29     | 8       | 30            | 3      | 17          | 0            | —        | 454ms      | 74ms       | 1631ms |
| 133  | 24000    | 48         | 38     | 10      | 29            | 3      | 16          | 0            | —        | 441ms      | 232ms      | 1673ms |
| 140  | 24000    | 57         | 46     | 10      | 30            | 4      | 17          | 1            | —        | 752ms      | 257ms      | 2164ms |

**Leader Build 子阶段统计（24K tx）**:
- packing_ms: 30-57ms (avg 37ms)
- evm_exec_ms: 25-46ms (avg 30ms)
- pool_overhead_ms: 5-10ms (avg 7ms) ← 限速后极其稳定！
- finish_ms (state root): 28-32ms (avg 29ms)
- ser_ms: 3-4ms
- compress_ms: 15-24ms (avg 17ms)
- broadcast_ms: 0-1ms
- **total build: 91-121ms (avg ~100ms)** ← 远小于历史 293ms（28K）和上轮 550ms（40K）

### FCU 时序 (新增)

| View | elapsed_ms | Status |
|------|------------|--------|
| 77   | 9ms        | Valid  |
| 78   | 17ms       | Valid  |
| 79   | 7ms        | Valid  |
| 80   | 16ms       | Valid  |
| 84   | 8ms        | Valid  |
| 87   | 19ms       | Valid  |
| 89   | 33ms       | Valid  |
| 76   | 5ms        | Syncing (偶发) |
| 90   | 17ms       | Syncing (偶发) |

FCU p50=9ms, p95=33ms — **不在关键路径**（共识完成后才执行 FCU）

### Follower 时序样本 (view 78-100)

| View | proposal@ | vote_delay | import_ms | commit_ms | total@ |
|------|-----------|------------|-----------|-----------|--------|
| 78   | @1545ms   | 128ms      | 461ms     | 581ms     | @2255ms |
| 80   | @1281ms   | 188ms      | 345ms     | 628ms     | @2097ms |
| 85   | @1502ms   | 227ms      | 278ms     | 549ms     | @2279ms |
| 87   | @1189ms   | 208ms      | 223ms     | 422ms     | @1820ms |
| 92   | @1426ms   | 363ms      | 114ms     | 358ms     | @2147ms |
| 96   | @927ms    | 222ms      | 131ms     | 375ms     | @1524ms |
| 98   | @1274ms   | —          | —         | 768ms     | @1920ms |

Follower import: 100-461ms (compact block cache hit, 大部分 100-200ms)
vote_delay: 35-670ms (投票等待 import 完成)

## 详细时序图 — 24K tx 块 (Leader View=91, 典型稳态)

```
Leader (view=91, 24K tx, 504M gas, 2s slot):
================================================================================

  t=0ms                        t=96ms  t=1128ms        t=1693ms  t=1737ms
  │                            │       │               │         │
  ├── Payload Build (串行) ────┤       │               │         │
  │                            │       │               │         │
  │  ┌── packing 32ms ──┐     │       │               │         │
  │  │ evm_exec=26ms     │     │       │               │         │
  │  │ pool_overhead=5ms │     │       │               │         │
  │  └───────────────────┘     │       │               │         │
  │        ┌── finish/SR 28ms ─┐       │               │         │
  │        └───────────────────┘       │               │         │
  │             ┌─ ser 4ms ─┐          │               │         │
  │             └───────────┘          │               │         │
  │               ┌── compress 18ms ──┐│               │         │
  │               └───────────────────┘│               │         │
  │                          ┌ broadcast 0ms ┐         │         │
  │                          └───────────────┘         │         │
  │  build_ms = 96ms                   │               │         │
  │                                    │               │         │
  ├── Slot Wait (等待 slot 对齐) ──────┤               │         │
  │                                    │               │         │
  │                             Proposal @1128ms       │         │
  │                                    │               │         │
  │                                    ├── R1 565ms ───┤         │
  │                                    │               ├─ R2 43ms┤
  │                                    │               │   total=1737ms
  │                                    │               │         │
  │  ┌── Eager Import (并行于共识) ──────────────┐     │         │
  │  │ new_payload 156ms (不阻塞共识)             │     │         │
  │  └────────────────────────────────────────────┘     │         │
  │                                                     │         │
  │                                    ┌── FCU (commit 后串行) ──┐│
  │                                    │ fcu_elapsed=2ms         ││
  │                                    └─────────────────────────┘│


Follower (view=92, 接收 view=91 的块):
================================================================================

  t=0ms                 t=1426ms        t=1789ms   t=1978ms   t=2147ms
  │                     │               │          │          │
  │  等待 proposal...    │               │          │          │
  │                     │ 收到 Proposal   │          │          │
  │                     ├─ compact_inject │          │          │
  │                     │ decompress 0ms  │          │          │
  │                     │ deser 0ms       │          │          │
  │                     ├─ new_payload ───┤          │          │
  │                     │ import 114ms    │          │          │
  │                     │ (cache hit)     │          │          │
  │                     └─────────────────┘          │          │
  │                          ├─ vote_delay 363ms ────┤          │
  │                          └───────────────────────┘          │
  │                                        @1978ms commit_vote  │
  │                                                  ├─ wait QC ─┤
  │                                                  │  @2147ms  │
  │                                                  └──────────┘
  │                                                     ┌── FCU ─┐
  │                                                     │ ~10ms  │
  │                                                     └────────┘
```

## 串并关系总结

```
Leader 关键路径（串行）:
  [Build 96ms] → [Slot Wait ~1032ms] → [R1 565ms] → [R2 43ms] = 1737ms
                                        ↑
                                        ├── Eager Import 156ms (并行，不阻塞)
                                        └── FCU 2ms (commit 后串行，不在关键路径)

Follower 关键路径（串行）:
  [等 Proposal ~1426ms] → [Import 114ms] → [Vote Delay 363ms] → [等 CommitQC ~169ms]
                                                                  = @2147ms total

关键路径分配 (leader view=91, total=1737ms):
  Build:      96ms  (5.5%)   ← 不是瓶颈
  Slot Wait: 1032ms (59.4%)  ← 等待 2s slot 对齐
  R1:         565ms (32.5%)  ← follower 投票收集
  R2:          43ms (2.5%)   ← commit 投票收集
```

## 与历史基线和上轮对比

| 指标 | 历史 28K 稳态 | 上轮 40K | 本轮 24K | 变化 |
|------|-------------|---------|---------|------|
| build_ms | 293ms | 550ms avg | **96ms avg** | **-67%** ✅ |
| packing_ms | ~100ms | 71-454ms | **30-57ms** | **-70%** ✅ |
| pool_overhead | 未知 | 21-165ms | **5-10ms** | **-95%** ✅ |
| R1_collect | 670ms | 680ms avg | **460ms avg** | **-31%** ✅ |
| avg_block_time | 2.0s | 2.5s | **2.0s** | **恢复** ✅ |
| sustained TPS | 14,000 | 16,353 | **11,536** | 与目标匹配 |
| compress_ms | — | 29-184ms | **15-24ms** | **-48%** ✅ |
| broadcast_ms | — | — | **0-1ms** | 首次测量 |
| FCU elapsed | — | — | **p50=9ms, p95=33ms** | 首次测量 |

## 关键发现

### 1. 限速是 Pool 健康的关键
- 上轮 35K TPS 灌入 → pool_overhead 21-165ms（波动 8x）
- 本轮 13.7K TPS 限速灌入 → pool_overhead 5-10ms（波动 2x）
- **pool_overhead 下降 95%** — 这是 build_ms 从 550ms 降到 96ms 的主因

### 2. Build 只占关键路径 5.5%
- 96ms build / 1737ms total = 5.5%
- **Slot Wait (59.4%)** 和 **R1 Collect (32.5%)** 才是关键路径主体
- 进一步优化 build 对 TPS 没有意义（除非缩短 slot）

### 3. Follower Import 稳定在 100-280ms
- compact block cache hit: EVM=0ms, root=0ms
- 主要耗时是 reth new_payload 的状态写入

### 4. FCU 和 Broadcast 不是瓶颈
- FCU: p50=9ms, p95=33ms — commit 后执行，不在关键路径
- Broadcast: 0-1ms（2MB 压缩包，6 个直推对象）— 极快

## 下一步

1. **提高注入目标**: 当前 12K TPS 已被 24K cap 精确消化。提高到 14K-16K 需增大 cap 到 28K-32K
2. **Pool Queued 残留**: ~5K txs 因 nonce gap 永远无法执行。改进 blast pre-sign 的 nonce 连续性
3. **R1_collect 优化**: 占 32.5% 关键路径，可能通过减少网络 hop 或压缩投票大小改善
4. **稳定 20K TPS**: 需要 28K cap + 限速 20K 注入，下一轮测试验证
