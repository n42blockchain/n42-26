# 开发日志-26：高负载链停滞修复与 TPS 基线数据（修复后）

> 日期：2026-03-06
> 目标：修复 Step 1000+ TPS 链停滞问题，获取修复后完整 TPS 基线

---

## 问题描述

日志-25 修复了链启动停滞问题后，Step 500 TPS 正常工作（750 TPS overall），
但 Step 1000 TPS 时链完全停滞：pool 涨到 50,000 但 0 个新块产生。

### 根因分析

`building_on_parent` 守卫在构建开始时设置，但 **成功完成时从未清除**。

只有两条 FCU 失败路径（`execution_bridge.rs:210` 和 `229`）清除此守卫。
正常构建成功后（`spawn_payload_resolve_task` 完成 → `handle_built_payload`），
守卫保持不变。

死亡螺旋序列：
1. `do_trigger_payload_build` 设置 `building_on_parent = Some(parent)`
2. 大 pool (50k+ txs) 导致 resolve task 打包慢（EVM 顺序执行 ~2000 txs）
3. Pacemaker 超时（base_timeout = 20s）→ view 变化
4. 新 leader 调用 `do_trigger_payload_build` → `building_on_parent == parent`（head 未变）→ **跳过**
5. 又超时 → 又跳过 → 永久停滞

### 为什么 head 不变

View 变化不改变 `head_block_hash`。只有 block commit + import 才更新 head。
但 block 无法 commit 因为没人能构建新块 → 死锁。

---

## 修复方案

### Fix 5: Build 完成信号通道

添加 `build_complete_tx/rx: mpsc::UnboundedSender/Receiver<()>` 通道：

1. **设置**：在 `spawn_payload_resolve_task` 中 clone `build_complete_tx`
2. **发送**：监控 task 在 resolve task 完成后（无论成功/失败/panic）发送 `()`
3. **接收**：select! loop 中 `build_complete_rx.recv()` → 清除 `building_on_parent`
4. **快速路径**：`block_ready_rx.recv()` 中也立即清除（成功时提前清除）

关键设计决策：
- 不在 `handle_view_changed` 中清除 `building_on_parent`（会允许并发重复构建）
- 用完成信号而非超时清除（精确，无竞态）
- 监控 task 的 `handle.await` 天然等待 spawn task 结束，确保信号覆盖所有退出路径

---

## Stress Test 基线数据（修复后）

### 测试配置
- 7 节点本地测试网（单机 macOS）
- 500M gas limit, 4s slot time
- 600 sender accounts, prefill 20,000 txs
- Step mode: 500 → 1000 → 2000 → 3000 → 5000 → 7500

### 汇总结果

| Step Target | Effective TPS | Overall TPS | Blocks | Avg Block Time | Gas Util | Fail Rate |
|-------------|--------------|-------------|--------|---------------|----------|-----------|
| 500         | 472.7        | 589.7       | 15     | 4.0s          | 9.2%     | 0.0%      |
| **1000**    | **892.6**    | **944.6**   | **15** | **4.0s**      | **14.8%**| **0.0%**  |
| 2000        | 1495.1       | 1540.4      | 15     | 4.0s          | 24.2%    | 0.0%      |
| 3000        | 1756.3       | 1889.4      | 14     | 4.0s          | 29.5%    | 0.0%      |
| 5000        | 1882.9       | 1765.2      | 8      | **8.0s**      | **51.9%**| 0.1%      |
| 7500        | 2039.3       | 2289.3      | 7      | 4.7s          | 38.5%    | 3.5%      |

**关键改进**：Step 1000 TPS 从 0 块 → 15 块正常出块，链不再停滞！

### 峰值 Block 数据

| Block | TPS    | Gas Used    | Gas Util |
|-------|--------|------------|----------|
| #88   | 2076.8 | 348,894,000 | 69.8%   |
| #81   | 5952.2 | 499,999,000 | **100%** |

Block #81 达到了 gas limit 上限（500M / 21000 = 23,809 txs），证明理论极限被触及。

### Pipeline 时序分析

高负载（Step 5000+）时 pipeline 时序：
```
view 81: elapsed=7157ms  import=283ms  commit=5958ms  headroom=5674ms
view 83: elapsed=4198ms  import=1261ms commit=1621ms  headroom=359ms
view 84: elapsed=8359ms  import=770ms  commit=6608ms  headroom=5837ms
view 89: elapsed=10777ms import=464ms  commit=5582ms  headroom=5118ms
view 93: elapsed=7041ms  import=303ms  commit=4520ms  headroom=4216ms
```

关键发现：
- `import` 时间（new_payload + FCU）= 300-1260ms，合理
- `commit` 时间（共识延迟）= 1000-6600ms，在高负载时飙升到 5-10s
- `elapsed`（view 总时长）= 3-10s，超过 4s slot 时间

### 瓶颈分析

```
1. gas_utilization = 51.9% (step 5000) → EVM 容量未打满，但接近
   - 峰值 block 达到 100% gas → gas limit 是硬上限
   - 但平均只有 30-50% → builder 来不及打包更多

2. avg_block_time = 8.0s (step 5000) → 出块间隔翻倍
   - 4s slot + 构建延迟 = 实际 8s
   - 构建大块（23,809 txs）耗时 4s+，导致错过下一个 slot

3. consensus commit latency = 5-10s (高负载)
   - 大块数据广播耗时增加
   - GossipSub 在大消息下效率下降

4. pool_pending 稳定在 1000-3000 (step 1000-3000) → 链能跟上
   - pool_pending 到 50,000 (step 7500) → 链跟不上

结论：当前瓶颈是 出块周期（4s slot + builder 时间） 和 大块共识延迟。
理论 TPS 上限 = 500M / 21000 / 4 = 5,952 TPS（单块达到过）
实测持续 TPS 上限 = ~2,000 TPS（受 builder 打包速度限制）
```

---

## 改动文件

| 文件 | 改动 |
|------|------|
| `orchestrator/mod.rs` | 新增 `build_complete_tx/rx` 字段和通道初始化；`block_ready_rx` 中清除 `building_on_parent`；新增 `build_complete_rx` select 分支 |
| `orchestrator/execution_bridge.rs` | `spawn_payload_resolve_task` 传递 `build_complete_tx`；监控 task 完成后发送信号；clippy fix |

---

## 后续优化方向

### 短期（降低 slot_time）
- 将 `N42_BLOCK_INTERVAL_MS` 从 4000 降到 1000
- 理论上限 = 500M / 21000 / 1 = 23,809 TPS
- 但需要 builder 在 1s 内完成打包（当前大块需要 4s+）
- **需要先优化 builder 打包速度**

### 中期（并行 EVM）
- 当前 gas 利用率 30-50%，EVM 是串行执行
- 峰值 block 已达 100% gas → EVM 已是瓶颈之一
- 集成 PEVM/Grevm 可将 EVM 吞吐量提升 4-8x
- **优先级高**：这是解锁更高 TPS 的关键

### 调查项
- 高负载时共识延迟飙升到 5-10s 的根因
- GossipSub 大块传输优化（压缩？分片？）
- builder 打包速度优化（当前 23k txs 需要 4s+）
