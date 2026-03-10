# 开发日志 39 — Timeout Fix 压测验证与 TPS 突破

## 日期: 2026-03-09

## 一、本次改动

### 1.1 Timeout Collector Bug 修复（保留自日志-38）
- Bug 1: `on_timeout()` 无条件重建 collector → `get_or_insert_with()` 保留已收投票
- Bug 2: repeat timeout 不重新广播 → 重新发送 + 重检 quorum

### 1.2 Block Size Management 重新应用
Hot state 零拷贝 revert 时丢失了 payload builder 改动，重新应用：
- `N42_MAX_TXS_PER_BLOCK`：默认 40,000
- `N42_BUILD_TIME_BUDGET_MS`：默认 1000ms，每 256 tx 检查一次
- EVM 执行计时（per-tx + packing 总计 + finish 计时）
- Payload cache 存储（leader new_payload 跳过 EVM 重执行）

修改文件：`reth/crates/ethereum/payload/src/lib.rs`

## 二、压测结果（7 nodes, 2G gas, 2s slot, macOS）

### 2.1 全量 Step Test 结果

| Step | Target TPS | Sustained TPS | avg_tx/block | avg_block_time | max_block_tps | fail_rate |
|------|-----------|--------------|-------------|---------------|---------------|-----------|
| 1 | 1,000 | 1,216 | 2,423 | 2.0s | 2,214 | 12.5% |
| 2 | 3,000 | 3,442 | 6,869 | 2.0s | 4,590 | 0.0% |
| 3 | 5,000 | 5,447 | 10,973 | 2.0s | 6,680 | 0.0% |
| 4 | 7,500 | **11,868** | 24,000 | 2.0s | 12,513 | 0.0% |
| 5 | 10,000 | **11,885** | 24,037 | 2.0s | 12,831 | 0.0% |
| 6 | 15,000 | **16,728** | 63,949 | 3.8s | 20,000 | 0.6% |
| 7 | 20,000 | 9,277 | 41,504 | - | 20,000 | 46.0% |
| 8 | 30,000 | 16,201 | 86,969 | 5.4s | 20,000 | 11.6% |
| 9 | 40,000 | 14,848 | 90,855 | 6.1s | 20,000 | 29.6% |
| 10 | 50,000 | 17,938 | 109,587 | 6.1s | 20,000 | 12.3% |

### 2.2 稳定性验证
- 761 块连续出块（block 9 → 761+），36 分钟
- 7/7 节点全部存活
- **零 chain stall**（Timeout fix 完全有效）
- bp_pauses=0（无 backpressure 暂停）

### 2.3 Payload Builder 性能
```
N42_PAYLOAD_PACK: tx_count=40000, packing_ms=88, evm_exec_ms=65, pool_overhead_ms=23
N42_PAYLOAD_FINISH: finish_ms=49-382 (state root)
cumulative_gas_used=840M (2G gas limit)
```

40K tx 打包仅需 88ms，EVM 65ms，线性！

## 三、瓶颈分析

### 3.1 稳态区间（block_time=2.0s）
- **Sustained 12K TPS**，p50=11,911 TPS
- 受限于 gas limit（500M effective → 24K tx/block × 21K gas/tx）
- Step 4-5 的 gas_utilization=25% 说明 pool 注入不够快

### 3.2 高负载区间（block_time>2.0s）
- **Peak 20K TPS** 但 block_time 膨胀到 3.8-6.1s
- **主因：大块 import（new_payload 磁盘 I/O）**
- 40K tx 块的 import 需要 1-2s，挤压了下一块的构建窗口
- avg_tx_per_block 高达 40K（命中 MAX_TXS_PER_BLOCK 限制）

### 3.3 与之前基线对比
| 指标 | 之前（日志-26） | 本次 | 提升 |
|------|----------------|------|------|
| Sustained TPS | 5,952 | **11,885** | **2.0x** |
| Peak TPS | 5,952 | **20,000** | **3.4x** |
| Max tx/block | 23,809 | **40,000** | 1.7x |
| Chain stall | 偶发 | **零** | 完全修复 |
| Gas utilization | 100% (500M) | 25-42% (2G) | 有余量 |

## 四、对 43K TPS 计划的影响

### 已完成
- ✅ Timeout collector bug fix — 消除链 stall
- ✅ Block size management — MAX_TXS、BUILD_TIME_BUDGET
- ✅ Payload cache — leader 跳过 EVM 重执行

### Hot State 零拷贝（Phase 1）状态
- ❌ 已回退 — hot_backing 导致 state root mismatch（update_hot_state 只在 leader 调用）
- EVM 在当前负载下（40K tx = 65ms）不是瓶颈
- 只有当 tx/block > 100K 时才需要零拷贝优化

### 下一步优先级调整
1. **Phase 2: Block time 优化** — 消除 slot 边界等待、限制 pool 迭代深度，让 block_time 回到 2.0s
2. **Phase 3: 压测 v10** — 提升注入能力到 40K+ TPS（当前 step 4-5 注入受限）
3. **提升 MAX_TXS_PER_BLOCK** — 从 40K → 80K，配合 import 优化
4. **Phase 4: 1s slot** — 当 block_time 稳定在 <1.5s 时切换

### 新瓶颈排序
1. **压测注入能力**（~16K TPS 天花板）— 制约了 sustained TPS 测量
2. **Import 磁盘 I/O** — 40K tx 块 import 导致 block_time 膨胀
3. **MAX_TXS_PER_BLOCK=40K** — 硬性上限
4. EVM 执行（65ms/40K tx）— 目前不是瓶颈
