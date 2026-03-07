# 开发日志-27：EVM 跳过基准测试与瓶颈精确定位

> 日期：2026-03-07
> 目标：通过跳过 EVM 执行的对照测试，精确定位高负载下的性能瓶颈

---

## 实验设计

### 背景

日志-26 确认了 1s slot time 下的性能数据：
- EVM 执行：median 16ms, max 202ms（满块 23k txs）
- State root：median 15ms, max 200ms
- Consensus commit：median 3909ms, P95 7041ms, max 10777ms

假设：高负载时的 3-10s commit 延迟是否由 EVM 执行时间引起？

### 方法

在 reth 中添加 `N42_SKIP_EVM=1` 环境变量开关：
- Builder 端：跳过 `default_ethereum_payload` 中的交易循环（产生空块）
- Validator 端：无需修改（空块无交易需要执行）

修改文件：`reth/crates/ethereum/payload/src/lib.rs`

```rust
let skip_evm = std::env::var("N42_SKIP_EVM").is_ok();
while let Some(pool_tx) = if skip_evm { None } else { best_txs.next() } {
```

---

## 实验 1：空块（EVM 跳过）基线

### 配置
- 7 节点本地测试网, 1s slot time, 500M gas limit
- `N42_SKIP_EVM=1` — 所有节点跳过 EVM

### 结果 (110 samples)

| 指标 | p50 | p95 | max |
|------|-----|-----|-----|
| **Import 延迟** | 13ms | 48ms | 67ms |
| **Commit 延迟** | 36ms | 76ms | 129ms |
| **Build 时间** | 0ms | 0ms | 0ms |
| **View elapsed** | ~947ms | - | - |

**关键发现**：空块时共识协议本身只需 **36ms**（p50），7 节点本地网络上共识不是瓶颈。

---

## 实验 2：正常 EVM + 逐步压测 (4s slot time)

### 配置
- 7 节点本地测试网, 4s slot time, 500M gas limit
- 600 sender accounts, prefill 20,000 txs
- Step mode: 500 → 1000 → 2000 → 3000 → 5000 → 7500 → 10000

### Stress Test 汇总

| Step Target | Effective TPS | Overall TPS | Blocks | Avg Block Time | Gas Util | Fail Rate |
|-------------|--------------|-------------|--------|---------------|----------|-----------|
| 500         | 479.4        | -           | 0*     | -             | -        | 0.2%      |
| **1000**    | **818.2**    | **1023.0**  | **15** | **4.0s**      | **14.2%**| **0.0%**  |
| **2000**    | **1633.7**   | **1600.0**  | **9**  | **7.0s**      | **41.8%**| **0.4%**  |
| **3000**    | **1861.9**   | **1950.7**  | **9**  | **7.0s**      | **51.0%**| **4.1%**  |
| 5000        | 2351.3       | 2678.2      | 6      | 4.0s          | 37.5%    | 0.4%      |
| 7500        | 3340.1       | 2670.8      | 2      | 4.0s          | 22.4%    | 11.2%     |
| 10000       | 5576.2       | -           | 0**    | -             | -        | 0.7%      |

*Step 500 的 blocks=0 是因为节点还在追赶 prefill 数据
**Step 7500+ 链停滞，pool 涨到 50,000 pending

### Pipeline 时序分析（按负载分层）

| 负载水平 | 代表 View | Import | Commit | Elapsed |
|---------|----------|--------|--------|---------|
| 空闲 | 51-61 | 4-58ms | 24-77ms | ~3.9s |
| 中负载 | 64-67 | 110-480ms | 477-3535ms | 3-8s |
| 高负载 | 68-70 | 581-945ms | 5130-14299ms | 7-17s |
| **极端** | **77** | **1401ms** | **25680ms** | **28s** |

### 峰值 Block 数据

| Block | TPS | Txs | Gas Used | Gas Util |
|-------|------|-----|----------|----------|
| #77 | 5952.2 | 23,809 | 499,989,000 | 100% |
| #68 | 5952.0 | 23,808 | 499,981,468 | 100% |
| #63 | 1751.1 | 14,009 | 294,189,000 | 58.8% |

---

## 瓶颈精确定位

### 对比分析

| 指标 | 空块 (EVM 跳过) | 正常 (中负载) | 正常 (高负载) | 倍数 |
|------|----------------|-------------|-------------|------|
| Commit 延迟 p50 | **36ms** | 477-2650ms | 5130-25680ms | **13-710x** |
| Import 延迟 p50 | **13ms** | 110-441ms | 581-1401ms | **8-108x** |
| Build 时间 | 0ms | ~16ms | ~200ms | N/A |

### 结论

**共识协议本身不是瓶颈**（空块只需 36ms）。

高负载时的性能退化完全由**块数据大小**驱动：

1. **GossipSub 大块传播**（commit 延迟主因）
   - 23k txs 的块 ≈ 3.5MB 数据
   - GossipSub 在大消息下效率急剧下降
   - 单块 commit 从 36ms 暴增到 25,680ms（700x）

2. **new_payload 处理**（import 延迟）
   - 大块的 state root 计算 + 事务处理
   - 从 13ms 增长到 1,401ms（108x）

3. **Pool 积压引发的死亡螺旋**
   - Pool 达到 50k → 每块打包 23k txs → 块太大 → 传播慢
   - 传播慢 → 出块间隔变长 → pool 进一步积压
   - 最终 reth FCU 不返回 payload_id → 链完全停滞

### 理论 TPS 上限

```
当前限制因素：
- 持续 TPS ≈ 2000（受 commit 延迟限制）
- 峰值 TPS = 5952（单块 gas limit 上限）
- 理论 TPS = 500M / 21000 / 4s = 5952

EVM 执行时间占比分析：
- Build (EVM) = 16-200ms
- Commit (网络) = 36ms (空块) → 25,680ms (满块)
- 瓶颈占比 = Commit / Total > 95%

结论：即使 EVM 执行时间降为 0，TPS 也不会显著提升。
真正的瓶颈是 大块数据在 7 节点间的 GossipSub 传播延迟。
```

---

## 1s Slot Time 的问题

### 实验结果

1s slot time + 正常 EVM + Step 500 TPS → **链在 view 39 后停滞**

原因：pool 积累到 15k-50k 后，builder 打包大块（23k txs），reth 处理不过来，
FCU 不返回 payload_id，pacemaker 超时级联，链永久停滞。

**1s slot time 不适合当前架构**：builder 打包大块的时间 + 共识传播时间 > 1s。

---

## 优化方向建议（按优先级排序）

### 1. 块大小限制（立即可做）
- 限制每块最大 tx 数量（如 5000 txs 而非 23k）
- 或限制每块 gas 利用率上限（如 30% = 150M gas）
- 牺牲峰值 TPS 换取出块稳定性

### 2. 块传播优化（中期）
- 将 GossipSub 替换为 **leader 直推** 模式
- 使用 **Erasure Coding** 分片传输大块
- QUIC 多流并发
- 预期：commit 延迟降低 5-10x

### 3. 减小 slot time + 限制块大小（组合）
- slot_time=1s + max_txs_per_block=2000
- 理论 TPS = 2000 × 1 = 2000（持续稳定）
- 避免大块传播问题

### 4. 并行 EVM（长期）
- 当前 EVM 不是瓶颈（仅占总时间 <5%）
- 但当块传播优化后，EVM 将成为下一个瓶颈
- 集成 PEVM/Grevm 预备

---

## 改动文件

| 文件 | 改动 |
|------|------|
| `reth/crates/ethereum/payload/src/lib.rs` | 添加 `N42_SKIP_EVM` 环境变量，跳过 tx 循环 |

注：此修改为临时基准测试用途，不应合入主分支。
