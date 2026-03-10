# 开发日志-25：链启动稳定性修复与 Stress Test 基线数据

> 日期：2026-03-06
> 目标：修复 7 节点测试网链停滞问题，获取 TPS 基线数据

---

## 问题描述

7 节点测试网启动后，链在 view 14-20 范围内永久停滞。

### 根因分析

三层问题叠加导致链停滞：

#### 1. 快速初始出块淹没 reth

启动后 views 1-13 在 ~1.5 秒内全部提交（每个 view ~100ms），产生的 `new_payload` 调用洪水般涌向 reth，触发 Pipeline Sync。reth 进入 SYNCING 状态后，`fork_choice_updated` 不返回 `payload_id`，leader 无法构建新块。

**根因**：所有 build 触发路径（Case A / view change / import done / eager import done）都直接调用 `do_trigger_payload_build(None)`，绕过了 `schedule_payload_build` 中的 4 秒 slot timing。

#### 2. 基于 parent hash 的重复构建竞态

当 `handle_eager_import_done` 和 `finalize_committed_block` 同时为相同 parent 触发构建时，产生两个不同的 block 14（相同 parent，不同 timestamp）。reth 收到两个冲突的 `new_payload` 后进入 Pipeline Sync。

原 `building_for_view` 守卫无法阻止此竞态，因为：
- Eager import 完成后在 view 13 构建（parent = block 12）→ building_for_view = 13
- View 变化 → building_for_view = None（清除）
- View 13 commit → finalize Case A → building_for_view = 14 → 构建 block 14（第二次）

但 eager import 也可能已将 head 更新为 block 13，触发基于 block 13 的 speculative build。两个 build 都以 block 13 为 parent，产生不同的 block 14。

#### 3. Speculative Build 跑飞

`handle_eager_import_done` 中没有检查 `speculative_build_hash`，导致：
1. Eager import block N → speculative build block N+1
2. Block N+1 eager import → speculative build block N+2
3. 无限循环，产生数千个未提交的块（实测 4958 个）

#### 4. FCU 失败后 `speculative_build_hash` 未清除

FCU 返回 SYNCING 时：
- `building_on_parent` 被清除 ✓
- `speculative_build_hash` **未被清除** ✗
- Build retry timer 检查 `speculative_build_hash.is_some()` → 跳过 retry → 永久停滞

---

## 修复方案

### Fix 1: 统一走 slot timing

将所有 build 触发路径重定向到 `schedule_payload_build()`，确保 4 秒 slot 间隔正确执行：

| 路径 | 修复前 | 修复后 |
|------|--------|--------|
| Case A (block in reth) | `do_trigger_payload_build(None)` | `schedule_payload_build()` |
| view change → leader | `do_trigger_payload_build(None)` | `schedule_payload_build()` |
| handle_import_done | `do_trigger_payload_build(None)` | `schedule_payload_build()` |
| handle_eager_import_done | `do_trigger_payload_build(None)` | `schedule_payload_build()` |

### Fix 2: Parent-based 构建守卫

将 `building_for_view: Option<u64>` 改为 `building_on_parent: Option<B256>`：
- 基于 parent hash 而非 view number 去重
- 自然阻止两个基于同一 parent 的并发构建
- view 变化时无需手动清除（parent 变化即自动失效）

### Fix 3: Speculative build 一级限制

在 `handle_eager_import_done` 中增加守卫：
```rust
if self.speculative_build_hash.is_some() {
    debug!("speculative build already done, skipping chain extension");
    return;
}
```

### Fix 4: FCU 失败时清除所有守卫

FCU 返回 SYNCING 或错误时：
```rust
self.building_on_parent = None;
self.speculative_build_hash = None;
self.schedule_build_retry();
```

---

## Stress Test 基线数据

### 测试配置
- 7 节点本地测试网（单机 macOS）
- 500M gas limit, 4s slot time
- 600 sender accounts, prefill 20,000 txs
- Step mode: 500 → 1000 TPS

### Step 500 TPS 结果

| 指标 | 值 |
|------|-----|
| overall_tps | **750** |
| avg_block_time | 4.0s |
| avg_block_tps | 516.6 |
| p50_tps | 457.5 |
| p95_tps | 1919.2 |
| max_tx_in_block | 8402 |
| gas_utilization | 11.3% avg / 32.2% peak |
| fail_rate (RPC) | 24.1% |

### Step 1000 TPS 结果

链停滞：pool 涨到 50,000 但 0 个新块产生。

### 瓶颈分析

```
1. gas_utilization = 11.3% → EVM 容量充足，非瓶颈
2. avg_tx_per_block = 2178 → 打包能力正常
3. avg_block_time = 4.0s → slot timing 正常工作
4. pool_pending 持续增长 → 出块速度跟不上发送速度

结论：当前瓶颈是 出块间隔（4s）导致的吞吐量上限。
理论上限 = gas_limit / 21000 gas_per_tx / slot_time
         = 500M / 21000 / 4
         = ~5952 TPS

实测 750 TPS vs 理论 5952 TPS = 12.6% 效率
原因：大量时间浪费在等待 slot boundary，而非并行执行
```

---

## 改动文件

| 文件 | 改动 |
|------|------|
| `orchestrator/mod.rs` | `building_for_view` → `building_on_parent: Option<B256>` |
| `orchestrator/execution_bridge.rs` | Parent-based 守卫；FCU 失败清除 `speculative_build_hash`；`schedule_build_retry` |
| `orchestrator/consensus_loop.rs` | 所有 build 路径走 `schedule_payload_build`；speculative build 一级限制；移除手动 startup throttle |

---

## 后续优化方向

### 短期（减小 slot_time）
- 将 `N42_BLOCK_INTERVAL_MS` 从 4000 降到 1000
- 理论上限 = 500M / 21000 / 1 = 23,809 TPS
- 需要验证 reth 能否在 1s 内完成 build + import

### 中期（并行 EVM）
- Gas 利用率仅 11.3%，说明 EVM 不是瓶颈
- 但随着 slot_time 降低和交易量增大，EVM 将成为瓶颈
- 集成 PEVM/Grevm 可将 EVM 吞吐量提升 4-8x

### 调查项
- Step 1000 TPS 链停滞的根因（可能是大 pool 导致 reth 性能下降）
- 高负载时 GossipSub 网络稳定性
