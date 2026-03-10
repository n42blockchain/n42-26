# 开发日志-43: 详细时序分析与 TPS 评估

## 背景

目标：评估当前系统能否稳定 20K TPS，画出详细时序图含串并关系。
测试配置：7 nodes, 2s slot, 2G gas, MAX_TXS_PER_BLOCK=40K, blast mode 2.4M pre-signed txs.

## 测试结果摘要

- overall_tps: 16,353 (sustained)
- avg_block_time: 2.5s
- avg_block_tps: 17,511
- p50_tps: 20,000
- max_tx_in_block: 40,000 (hit cap)
- 有效负载阶段: view 42-91 (8 个 leader blocks)
- 空闲阶段: view 92+ (pool 耗尽)

## Leader Block 完整时序表 (validator-0)

| View | tx_count | packing_ms | evm_exec_ms | pool_overhead | finish_ms(SR) | ser_ms | compress_ms | build_ms | proposal@ | R1_collect | R2_collect | total |
|------|----------|------------|-------------|---------------|---------------|--------|-------------|----------|-----------|------------|------------|-------|
| 35   | 0        | 0          | 0           | 0             | 0             | 0      | 0           | 12ms     | @1955ms   | 19ms       | 17ms       | 1991ms |
| **42** | 38969  | 141        | 73          | 68            | 120           | 6      | 29          | 696ms    | @2260ms   | 470ms      | 131ms      | 2862ms |
| **49** | 40000  | 335        | 170         | 165           | 128           | 15     | **184**     | 696ms    | @2016ms   | 896ms      | 130ms      | 3044ms |
| **56** | 40000  | 215        | 150         | 64            | 184           | 6      | 31          | 473ms    | @2424ms   | 667ms      | 286ms      | 3377ms |
| **63** | 40000  | 71         | 50          | 21            | 45            | 6      | 30          | 186ms    | @2485ms   | 596ms      | 278ms      | 3360ms |
| **70** | 40000  | 353        | 288         | 64            | —             | —      | —           | 852ms    | @1112ms   | 798ms      | 243ms      | 2155ms |
| **77** | 40000  | 454        | 386         | 68            | —             | —      | —           | 579ms    | @1214ms   | 783ms      | 124ms      | 2122ms |
| **84** | 40000  | —          | —           | —             | —             | —      | —           | 745ms    | @1068ms   | 533ms      | 222ms      | 1823ms |
| **91** | 40000  | —          | —           | —             | —             | —      | —           | 522ms    | @1095ms   | 753ms      | 124ms      | 1974ms |
| 98   | 0        | 0          | 0           | 0             | 0             | 0      | 0           | 13ms     | @1966ms   | 10ms       | 14ms       | 1991ms |

注：view 70+ 的 PACK/FINISH 日志被 payload 多次调用覆盖（两次 PACK 输出），具体子阶段数据丢失但 build_ms 从 pipeline 日志可得。

## Follower Block 时序样本

| View | proposal@ | vote_delay | import_ms | commit_ms | total@ |
|------|-----------|------------|-----------|-----------|--------|
| 45   | @2299ms   | 577ms      | 240ms     | 250ms     | @3127ms |
| 46   | @2945ms   | 907ms      | —         | 40ms      | @3892ms |
| 50   | @3240ms   | 319ms      | 196ms     | 592ms     | @4152ms |
| 58   | @2008ms   | 446ms      | 238ms     | 244ms     | @2699ms |
| 64   | @1344ms   | 559ms      | 307ms     | 511ms     | @2415ms |
| 76   | @1302ms   | 199ms      | 126ms     | 428ms     | @1928ms |
| 85   | @1302ms   | 549ms      | —         | 142ms     | @1993ms |
| 92   | @2620ms   | 45ms       | 56ms      | 102ms     | @2768ms |
| 93   | @1845ms   | 0ms        | 4ms       | 34ms      | @1876ms (空块) |

## 详细时序图 — 40K tx 块 (Leader View=77, 典型稳态)

```
Leader (view=77, 40K tx block, 2G gas):
================================================================================

  t=0                                                              t=1214ms     t=2000ms    t=2122ms
  │                                                                │            │           │
  ├──────── Payload Build (串行) ──────────────────────────────────┤            │           │
  │                                                                │            │           │
  │  ┌── packing (454ms) ──┐                                       │            │           │
  │  │ pool.best → 40K txs │                                       │            │           │
  │  │ evm_exec=386ms      │                                       │            │           │
  │  │ pool_overhead=68ms  │                                       │            │           │
  │  └─────────────────────┘                                       │            │           │
  │              ┌── finish/SR (~50ms) ──┐                          │            │           │
  │              └──────────────────────┘                           │            │           │
  │                         ┌── ser+compress (~40ms) ──┐           │            │           │
  │                         └──────────────────────────┘           │            │           │
  │                                                     ┌─ broadcast ─┐         │           │
  │                                                     └─────────────┘         │           │
  │  build_ms = 579ms                                              │            │           │
  │                                                                │            │           │
  ├──────── Consensus (与 follower import 并行) ──────────────────────────────────────────────┤
  │                                                     Proposal @1214ms        │           │
  │                                                                ├─ R1 783ms ─┤           │
  │                                                                             ├─R2 124ms──┤
  │                                                                             │     total=2122ms
  │                                                                             │           │
  │  ┌── Leader eager import (并行于共识) ──┐                      │            │           │
  │  │ new_payload 693ms (不阻塞共识)       │                      │            │           │
  │  └─────────────────────────────────────┘                       │            │           │
  │                             ┌── FCU commit (串行) ──────────────────────────────────────┐│
  │                             │ 等 commit QC 后 finalize                                  ││
  │                             └──────────────────────────────────────────────────────────┘│

Follower (对应 view 的 block):
================================================================================

  t=0                   t=1302ms          t=1501ms   t=1758ms   t=1928ms
  │                     │                 │          │          │
  │  等待 proposal...    │                 │          │          │
  │                     │ 收到 Proposal     │          │          │
  │                     ├─ compact_inject ─┤          │          │
  │                     │ ~0ms (empty exec)│          │          │
  │                     ├─ new_payload ────┤          │          │
  │                     │ import 126ms     │          │          │
  │                     │ (cache hit,0 EVM)│          │          │
  │                     └─────────────────┘          │          │
  │                          ├─ vote_delay 199ms ────┤          │
  │                          │ (等import完+投票传播)    │          │
  │                          └───────────────────────┘          │
  │                                        ├── commit wait ─────┤
  │                                        │ @1928ms total      │
  │                                        └────────────────────┘
```

## 串并关系总结

```
Leader 关键路径（串行）:
  [Payload Build] ──→ [Broadcast] ──→ [R1 Collect] ──→ [R2 Collect]
       579ms            ~20ms           783ms            124ms
                                                        ─────────
                                                   total ≈ 1500ms

  + proposal delay (@1214ms - 579ms = 635ms) ← 等待 slot 对齐

Leader 非关键路径（并行于共识）:
  [Eager Import] ← 693ms，与 R1/R2 并行，不阻塞
  [FCU/Finalize]  ← 在 commit 后执行

Follower 关键路径（串行）:
  [等待 Proposal] ──→ [Compact Inject + Import] ──→ [Vote] ──→ [等待 CommitQC]
     @1302ms              126-307ms                   ~10ms       100-500ms
```

## 与历史 28K 基线对比

| 指标 | 历史 28K 稳态 | 当前 40K | 退步幅度 |
|------|-------------|---------|---------|
| build_ms | 293ms | 186-852ms (avg ~550ms) | **+88%** |
| R1_collect | 670ms | 533-896ms (avg ~680ms) | ≈持平 |
| R2_collect | 192ms | 124-286ms | ≈持平 |
| consensus total | 2122ms | 1823-3377ms (avg ~2600ms) | **+23%** |
| avg_block_time | 2.0s | 2.5s | **+25%** |
| sustained TPS | 14,000 | 16,353 | +17% (因块更大) |

## 退步根因定位

**共识层没有退步** — R1_collect 和 R2_collect 与历史基本持平。

**退步全部来自 Leader Build**：
1. **packing_ms 波动巨大**: 71-454ms (历史 ~100ms) — tx pool 在 2.4M tx 灌入时有锁竞争
2. **evm_exec_ms 成比例增长**: 50-386ms — 40K tx vs 28K tx，EVM 时间正常增长
3. **compress_ms 偶发异常**: 184ms (view 49) vs 正常 30ms — 可能 GC 或内存压力
4. **pool_overhead_ms**: 21-165ms — blast 模式下 pool 被持续写入导致

## 评估：能否稳定 20K TPS？

**当前不能**。计算：
- 需要: 20K TPS × 2.0s slot = 40K tx/block @ 2.0s
- 实际: 40K tx/block @ 2.5s avg = **16K TPS**
- 瓶颈: build_ms avg 550ms 导致 block_time 超出 2.0s slot

**达到 20K TPS 的三条路径**：

### 路径 A: 减少块大小 + 快速出块
- MAX_TXS_PER_BLOCK=28K, Fast Propose (1.0s block_time)
- 预计: 28K/1.0s = **28K TPS** ← 超过目标
- 风险: Fast Propose 在大块时不稳定

### 路径 B: 优化 Build Pipeline
- 并行化: packing 与 compress 可部分重叠
- 减少 pool_overhead: 使用 snapshot 而非在线竞争
- 目标: build_ms 从 550ms → 200ms
- 预计: 40K/2.0s = **20K TPS**

### 路径 C: Stress Tool 限速修复 + 精确注入
- 当前 blast 模式无限速 (per_rpc_tps=0 bug)
- 导致 pool 瞬间灌满 → pool_overhead 飙升
- 修复限速后 pool_overhead 预计降至 <30ms

## 下一步

1. **修复 blast rate limiting bug** — per_rpc_tps=0 导致无限速
2. **降低 MAX_TXS_PER_BLOCK 到 28K** + 测试 Fast Propose
3. **Profiling pool_overhead** — 确认是 lock contention 还是 allocation
4. **长期**: 将 compress 移到后台线程，与共识投票并行
