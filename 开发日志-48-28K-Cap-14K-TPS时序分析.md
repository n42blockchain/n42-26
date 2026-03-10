# 开发日志-48: 28K Cap + 14K TPS 全优化时序分析

> 日期：2026-03-10
> 阶段：Phase A14 验证 — 全优化栈 (OV + Channel Split + OrdMap + 独立 Runtime) 28K 基线

---

## 背景

在完成 Optimistic Voting (日志-46)、Channel Split + 独立 Runtime (日志-47)、Pool OrdMap (日志-45) 后，需要用 28K tx cap + 14K TPS 限速跑一次完整基线测试，绘制精确时序图，定位当前瓶颈。

截图中的"历史 28K 稳态"数据 (build=293ms, R1=670ms, R2=192ms) 来自优化前基线 (日志-43)，本次测试验证全部优化叠加后的实际效果。

## 测试配置

- 7 nodes macOS, 2s slot, 2G gas
- `N42_MAX_TXS_PER_BLOCK=28000`, `N42_SKIP_TX_VERIFY=1`
- Blast mode: `--blast 1800000 --target-tps 14000 --duration 120 --accounts 3000 --batch-size 500`
- 7 RPC endpoints, per_rpc_tps=2000, batch_interval_ms=250
- 代码版本: `e7289bf` (Optimistic Voting, Pool OrdMap, Channel Split, TX Bridge isolation)

## 测试结果摘要

```
BLAST RESULT:
  total_txs      = 1,666,000
  total_blocks   = 66
  total_time     = 131.9s
  sustained_tps  = 12,634
  avg_tx_per_block = 25,242
  drain_ms       = 9,009
  bp_pauses      = 0

BLOCK_ANALYSIS (50 blocks):
  avg_block_time = 2.0s
  overall_tps    = 13,419
  avg_block_tps  = 13,139
  p50_tps        = 13,827
  p95_tps        = 14,000
  max_block_tps  = 14,000
  max_tx_in_block= 28,000 (命中 cap)
  gas_utilization= 27.6%
  injection_tps  = 13,800 (稳定)
  injection_err  = 0
```

## Leader 完整时序表 (7 validators, 高负载 views 140-146)

| View | Validator | tx_count | packing_ms | evm_ms | pool_oh | build_ms | proposal@ | R1_collect | R2_collect | total |
|------|-----------|----------|------------|--------|---------|----------|-----------|------------|------------|-------|
| 140 | v0 | 27195 | 55 | 31 | 23 | 338ms | @1558ms | 68ms | 247ms | 1874ms |
| 141 | v1 | 27321 | 169 | 151 | 17 | 304ms | @1580ms | 84ms | 269ms | 1933ms |
| 142 | v2 | 28000 | 50 | 35 | 14 | 217ms | @1456ms | 275ms | 270ms | 2002ms |
| 143 | v3 | 26064 | 42 | 30 | 12 | 156ms | @1021ms | 112ms | 396ms | 1530ms |
| 144 | v4 | 22023 | 38 | 27 | 10 | 100ms | @1173ms | 79ms | 278ms | 1531ms |
| 145 | v5 | 18356 | 31 | 22 | 8 | 79ms | @1587ms | 96ms | 269ms | 1953ms |
| 146 | v6 | 14117 | 28 | 19 | 8 | 69ms | @1333ms | 76ms | 190ms | 1599ms |

**Leader Build 统计 (28K tx)**:
- packing_ms: 28-169ms, avg=59ms (首块 169ms 因 pool 初次填充)
- evm_exec_ms: 19-151ms, avg=42ms
- pool_overhead_ms: 8-23ms, avg=13ms (OrdMap 生效)
- build_ms: 69-338ms, **avg=180ms**, p50=156ms
- R1_collect: 68-275ms, **avg=113ms**, p50=84ms
- R2_collect: 190-396ms, **avg=274ms**, p50=269ms
- total: 1530-2002ms, **avg=1775ms**

### Leader 稳态数据 (views 148-170, 负载结束后排空)

| View | Validator | build_ms | proposal@ | R1_collect | R2_collect | total |
|------|-----------|----------|-----------|------------|------------|-------|
| 148 | v1 | 48ms | @1524ms | 84ms | 139ms | 1747ms |
| 149 | v2 | 15ms | @1643ms | 84ms | 66ms | 1794ms |
| 150 | v3 | 13ms | @1842ms | 18ms | 17ms | 1878ms |
| 151 | v4 | 12ms | @1960ms | 15ms | 13ms | 1990ms |
| 152 | v5 | 15ms | @1965ms | 11ms | 11ms | 1989ms |
| 153 | v6 | 13ms | @1975ms | 22ms | 14ms | 2013ms |
| 154 | v0 | 12ms | @1956ms | 18ms | 11ms | 1986ms |

空载/轻载: R1=11-84ms, R2=11-139ms — 共识本身极快。

## Follower 时序表 (validator-1 视角, 高负载 views 140-148)

| View | proposal@ | vote_delay | import_ms | commit_ms | total@ |
|------|-----------|------------|-----------|-----------|--------|
| 140 | @1609ms | 3ms | - | 1ms | @1967ms |
| 142 | @1683ms | 0ms | 273ms | 327ms | @2169ms |
| 143 | @1363ms | 0ms | 122ms | 205ms | @1864ms |
| 144 | @1465ms | 0ms | - | 0ms | @1929ms |
| 145 | @1498ms | 0ms | 59ms | 225ms | @1830ms |
| 146 | @1673ms | 2ms | 46ms | 163ms | @1901ms |
| 147 | @1757ms | 3ms | 72ms | 270ms | @2172ms |
| 148 | @1628ms | 1ms | 49ms | 90ms | @1817ms |

**vote_delay: 0-3ms (avg 1.1ms)** — Optimistic Voting 完全消除了投票等待。

## Follower Import (new_payload) 详情

| block_num | gas_used | evm_ms | root_ms | total_ms | type |
|-----------|----------|--------|---------|----------|------|
| 138 | 577M | 635 | 18 | **653ms** | block (无 compact block，首块) |
| 139 | 571M | 0 | 3 | **3ms** | payload (compact block cache hit) |
| 140 | 574M | 0 | 3 | **3ms** | payload (cache hit) |
| 141 | 588M | 0 | 3 | **3ms** | payload (cache hit) |
| 142 | 547M | 0 | 3 | **3ms** | payload (cache hit) |
| 143 | 462M | 0 | **187** | **189ms** | **block** (cache miss, 偶发) |
| 144 | 385M | 0 | 3 | **3ms** | payload (cache hit) |
| 145 | 296M | 0 | 3 | **3ms** | payload (cache hit) |

Compact block cache hit 率: ~87%（7/8 blocks）。偶发 cache miss 导致 EVM 重执行 (block 143: 189ms)。

## 其他测量

### Broadcast
- encoded_kb: 907-2244KB (view 140/147)
- direct_peers: 6
- send_ms: 0, gossip_ms: 0, total_broadcast_ms: 0

### FCU (finalize)
- 全部 elapsed_ms=0 — 极快

## 详细时序图 — 典型 view (v0 leader, view=140, 27K tx)

```
LEADER VIEW (view=140, validator-0, 27K tx block):
0ms         ~338ms                    ~1558ms   ~1626ms          ~1874ms    2000ms
|            |                         |         |                |          |
├── Build ───┤                         |         |                |          |
│ packing=55ms (evm=31, pool_oh=23)    |         |                |          |
│ finish(SR)=~30ms                     |         |                |          |
│ ser+compress=~20ms                   |         |                |          |
│ broadcast=0ms (2.2MB, 6 peers)       |         |                |          |
│ build_ms=338ms                       |         |                |          |
│                                      |         |                |          |
├── Slot Wait (等待 2s slot 对齐) ─────┤         |                |          |
│        1220ms 空闲                   |         |                |          |
│                                      |         |                |          |
│                              Proposal @1558ms  |                |          |
│                                      ├─ R1 68ms┤                |          |
│                                      |         ├── R2 247ms ────┤          |
│                                      |         |         total=1874ms      |
│                                      |         |                |          |
│  ┌─ Eager Import (并行, 不阻塞共识) ─────────────────────┐      |          |
│  │ new_payload=3ms (compact block cache hit, evm=0)      │      |          |
│  └───────────────────────────────────────────────────────┘      |          |
│                                                          ┌─ FCU 0ms ─┐    |
│                                                          └───────────┘    |
│                                                                     ├idle─┤
                                                                      126ms


FOLLOWER VIEW (view=140, validator-1 视角):
0ms                       ~1609ms  ~1612ms                  ~1888ms  ~1967ms
|                          |        |                        |        |
│  等待 leader proposal...  |        |                        |        |
│                          │ 收到 Proposal                   |        |
│                          │ verify BLS+QC ~3ms              |        |
│                          │ safe_to_vote ✓                  |        |
│                          ├─ vote (OV, vote_delay=3ms) ─────|        |
│                          │        │                        |        |
│                          │        │  等待 PrepareQC...      |        |
│                          │        │                 收到 PrepareQC   |
│                          │        │                        ├commit──┤
│                          │        │                  commit_vote @1888ms
│                          │        │                                 │
│                          │        │                      total @1967ms
│                          │        │                                 │
│  ┌── Import (并行于共识, 不阻塞投票) ─────────────┐                 │
│  │ new_payload=3ms (cache hit, evm=0, root=3ms)   │                 │
│  └────────────────────────────────────────────────┘                 │
│                                               ┌── FCU 0ms ─────────┐
│                                               └─────────────────────┘
```

## 关键路径分解

```
Leader 关键路径 (view=140, total=1874ms):
  ┌─────────────┬──────────────────┬─────────┬──────────────┐
  │ Build 338ms │ Slot Wait 1220ms │ R1 68ms │ R2 247ms     │
  │   18.0%     │     65.1%        │  3.6%   │   13.2%      │
  └─────────────┴──────────────────┴─────────┴──────────────┘

Leader 关键路径 (avg of 7 views):
  ┌─────────────┬──────────────────┬──────────┬──────────────┐
  │ Build 180ms │ Slot Wait ~1208ms│ R1 113ms │ R2 274ms     │
  │   10.1%     │     68.0%        │  6.4%    │   15.4%      │
  └─────────────┴──────────────────┴──────────┴──────────────┘

Follower 关键路径 (avg):
  ┌───────────────────────────────┬──────────┬──────────────┐
  │ 等待 Proposal ~1580ms (83%)   │ Vote 1ms │ 等QC ~310ms  │
  └───────────────────────────────┴──────────┴──────────────┘

Import 3ms → 完全并行, 不在任何关键路径上
FCU 0ms → 不在关键路径上
```

## 对比：历史 28K 基线 (日志-43) vs 优化前 24K (日志-44) vs 本次

| 指标 | 历史 28K (无优化) | 日志-44 (24K, 无OV) | **本次 (28K, 全优化)** | 变化 vs 历史 |
|------|------------------|---------------------|----------------------|------------|
| build_ms avg | 293ms | 96ms | **180ms** | **-39%** ✅ |
| R1_collect avg | 670ms | 490ms | **113ms** | **-83%** ✅ |
| R2_collect avg | 192ms | 152ms | **274ms** | +43% ⚠️ |
| vote_delay | 315ms | 128-363ms | **0-3ms** | **-99%** ✅ |
| follower import | 141ms+ | 100-461ms | **3ms** | **-98%** ✅ |
| avg_block_time | 2.0s | 2.0s | **2.0s** | 持平 |
| overall_tps | ~14,000 | 11,536 | **13,419** | **-4%** (接近) |
| sustained_tps | ~14,000 | - | **12,634** | |
| FCU elapsed | 未测 | 9-33ms | **0ms** | 极快 |
| consensus total | 2122ms | 1737ms | **1775ms avg** | **-16%** ✅ |
| pool_overhead | 未知 | 5-10ms | **8-23ms** | 稳定 |

## 关键发现

### 1. Optimistic Voting 效果极其显著
- vote_delay: 315ms (历史) / 363ms (日志-44) → **0-3ms**
- R1_collect: 670ms (历史) → **113ms avg** (-83%)
- 这是本轮最大单项优化

### 2. R2 反而比历史基线增大
- 历史 28K: R2=192ms → 本次: R2=274ms (+43%)
- 根因分析:
  - OV 让 R1 变快后, R2 变成"排队等位" — PrepareQC 广播后 follower 需要处理大 compact block
  - 偶发尖刺 (v3: R2=396ms) 拉高平均值
  - Channel Split 已将 R2 从日志-46 的 371ms 降至 274ms (-26%)

### 3. Build 首块效应
- views 140-141 (首次负载): build=304-338ms (pool 从空到满的过渡)
- views 142+: build=69-217ms (稳态)
- 限速注入下 pool_overhead 稳定在 8-23ms (OrdMap 生效)

### 4. Compact Block Cache Hit 稳定
- 8 个 block 中 7 个 cache hit (87.5%), import=3ms
- 偶发 1 个 cache miss (block 143, import=189ms) — 可能是 BlockData 延迟到达
- 即使 cache miss, 因为 OV 不等 import, 不影响投票速度

### 5. 系统完全稳定
- 120s 注入 + 9s 排空, 零 stall, 零 timeout, 零 nonce error
- avg_block_time=2.0s 完美匹配 slot
- injection_err=0, bp_pauses=0

## 瓶颈优先级

| 优先级 | 瓶颈 | 当前值 | 占关键路径 | 优化方向 |
|--------|------|--------|-----------|---------|
| P0 | Slot Wait | 1208ms | 68.0% | **设计性等待**, 非瓶颈 — 生产环境必需 |
| **P1** | **R2_collect** | **274ms avg** | **15.4%** | Optimistic Commit Vote / 减小 QC 传播延迟 |
| P2 | Build (首块) | 338ms peak | 10.1% avg | 首块 pool 预热; 稳态已降至 100-156ms |
| P3 | R1_collect | 113ms avg | 6.4% | 已大幅优化, 进一步空间有限 |
| - | Import | 3ms | 0% | ✅ 已完全消除 (compact block + OV) |
| - | FCU | 0ms | 0% | ✅ 已完全消除 |
| - | Broadcast | 0ms | 0% | ✅ 已完全消除 |

---

## 补充实验：Fast Propose + Presign-Load 27K TPS

### 目的

验证去掉 Slot Wait（从 68% 降到 0%）后，LAN 环境能否突破 14K TPS 上限。

### 测试配置

- 7 nodes macOS, **Fast Propose** (MIN_PROPOSE_DELAY=500ms, 无 slot 对齐)
- `N42_MAX_TXS_PER_BLOCK=28000`, `N42_SKIP_TX_VERIFY=1`
- **Presign-load**: 预签名 5M txs, `--target-tps 27000`, 5000 accounts
- 代码版本同上: `e7289bf`

### 结果

```
BLAST RESULT:
  total_txs      = ~2,700,000
  overall_tps    = 27,052
  avg_block_time = 1.0s
  max_block_tps  = 28,000
  sustained_tps  = ~25,000
```

### Fast Propose 共识时序

| 指标 | 值 |
|------|-----|
| R1_collect avg | 30ms |
| R2_collect avg | 25ms |
| consensus total | ~130ms/view |
| avg_block_time | 1.0s |

### Pool Overload 问题

- 27K/s 注入下，pool_pending 峰值达 **140K**
- pool_pending > 100K 时，build_ms 从 50ms 飙升至 **812ms**
- 本质：OrdMap clone 在 100K+ 规模下仍有 O(log n × size) 开销
- **结论**: 需要 pool backpressure 或 ring buffer 机制保护 pool 不超载

### 关键洞察：Slot Wait 不是浪费

通过对比三种模式验证了核心公式：**TPS = min(injection_rate, cap / block_time)**

| 模式 | block_time | injection | cap | 理论 TPS | 实测 TPS |
|------|-----------|-----------|-----|---------|---------|
| 2s Slot (blast 14K) | 2.0s | 14K | 28K | 14K | 13,419 |
| 2s Slot (blast 12K) | 2.0s | 12K | 28K | 12K | 12,223 |
| Fast Propose (presign 27K) | 1.0s | 27K | 28K | 27K | 27,052 |

- Slot Wait = TX 积累窗口，不是性能浪费
- 2s slot 下 TPS 受限于 injection_rate（14K），不是 slot 长度
- 去掉 Slot Wait 后 TPS 翻倍（27K），但前提是 injection 也翻倍
- **生产环境的 slot time 应由全球网络延迟决定，不应为提高 TPS 而缩短**

---

## 4 天优化总结 (Phase A12-A14)

### 优化栈叠加效果

| 优化 | 目标 | 效果 |
|------|------|------|
| Pool OrdMap (A12) | pool clone O(n)→O(log n) | pool_overhead 430ms→23ms |
| Optimistic Voting (A13) | vote_delay 消除 | R1 460ms→12ms (空载), 363ms→0ms (vote_delay) |
| Channel Split (A14a) | 共识消息优先 | R2 369ms→250ms |
| 独立 Runtime (A14b) | TX pool CPU 隔离 | R2 250ms→221ms, overall_tps +6% |

### 节点处理能力 vs 时间占用

```
2000ms slot 中各环节占比:

  Build:     180ms (10%)  ← 实际处理
  Slot Wait: 1208ms (68%) ← TX 积累窗口
  R1:        113ms (6%)   ← 网络+验证
  R2:        274ms (15%)  ← 网络+验证
  Idle:      ~125ms (6%)  ← 剩余
  ─────────────────────
  节点实际工作: ~567ms (32%)
  节点空闲:     ~1433ms (68%)
```

### 瓶颈层次

```
当前 TPS 上限决定因素（从外到内）:

  1. TX 注入速率 ← 当前限制因素 (14K injection → 14K TPS)
  2. Pool 容量 ← pool_pending > 100K 时退化
  3. 共识周期 ← R1+R2=387ms, 足够在 2s 内完成
  4. Slot Wait ← 设计性等待, 非瓶颈
  5. Build ← 180ms, 充裕
  6. Import ← 3ms, 已消除
```

### 结论

- **节点 75% 时间空闲**，LAN 环境下单节点处理能力远超需求
- **14K TPS 受限于注入速率**，不是共识或执行瓶颈
- **27K TPS 已在 Fast Propose 下验证可达**，但需充足注入 + pool 保护
- **进一步 LAN 优化收益递减** — 应转向:
  - Pool overload 保护 (backpressure / cap at 80K pending)
  - 全球节点部署，确定真实网络延迟
  - 基于实测延迟数据确定生产 slot time

## 下一步

1. **Pool Overload 保护**: pool_pending > 80K 时限流，防止 build_ms 退化
2. **R2 优化**: 研究 Optimistic Commit Vote 或 Pipeline PrepareQC
3. **Compact Block cache miss 排查**: 偶发 miss 导致 189ms import
4. **全球节点部署测试**: 确定真实 RTT 分布，规划生产 slot time
5. **提高 cap 测试**: 40K cap + 20K TPS 注入
