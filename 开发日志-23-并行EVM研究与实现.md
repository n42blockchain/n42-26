# 开发日志-23：并行 EVM 研究与 Block-STM 引擎实现

> 日期：2026-03-06
> 目标：研究高性能 EVM 并行执行方案，实现 Block-STM 引擎

---

## 行业调研

### 技术调研结果

| 项目 | 实测 TPS | 核心技术 | revm 版本 |
|------|---------|---------|----------|
| **Grevm 2.1** | 11.25 gigagas/s | Block-STM + DAG 调度 + 任务组 | v29 (自定义分支) |
| **PEVM** | 2-22x 加速 | Block-STM + lazy 更新 | v19 |
| **Sei Giga** | 5 gigagas/s | OCC 乐观并发 + SeiDB | Go 实现 |
| **N42 (reth)** | — | 顺序执行 | **v36** |

### 版本兼容性问题

**关键发现**：Grevm 使用 revm v29 (自定义分支), PEVM 使用 revm v19,
与我们的 reth（revm v36）**完全不兼容**。无法直接作为依赖集成。

### Grevm 2.1 架构要点

1. **三阶段执行**：并行执行 → 验证 → 最终化
2. **DAG 调度**：基于事务依赖关系构建有向无环图
3. **MVCC 存储**：多版本并发控制，每个 key 维护 (tx_idx → value) 版本链
4. **Lock-Free DAG**：替换全局锁为细粒度节点同步，降低 60% 调度开销
5. **任务组（Task Groups）**：相邻依赖事务在同一线程串行执行

---

## Phase 1 改进：Follower Eager Import

### 问题

之前的 eager import 只针对 leader 自身构建的区块（1/7）。
Follower 收到区块数据后仅缓存+通知共识，实际导入要等到 `finalize_committed_block`。

### 方案

在 `handle_block_data` 中，follower 收到区块数据后：
1. 缓存数据 + 通知共识（原有逻辑，不变）
2. **异步启动 new_payload + fcu**（新增）

共识投票与区块导入并行进行，commit 时 block 大概率已在 reth 中。

### 改动

| 文件 | 改动 |
|------|------|
| `execution_bridge.rs:handle_block_data()` | 添加 follower eager import spawned task |

---

## Phase 2：Block-STM 并行 EVM 引擎

### 设计决策

由于 Grevm/PEVM 与 revm v36 不兼容，选择**自研简化版 Block-STM**：
- 参考 Grevm 2.1 的 MVCC + 验证 + 重执行架构
- 直接使用 revm v36 的 `Database` trait
- 用 rayon 进行并行调度

### 架构

```
parallel_execute(txs, base_db, cfg_env, block_env)
       │
       ├── Scheduler (Block-STM)
       │     └── Execute → Validate → Re-execute conflicts
       │
       ├── MvMemory (MVCC Store)
       │     ├── accounts: DashMap<Address, BTreeMap<TxIdx, AccountInfo>>
       │     └── storage:  DashMap<(Address, Slot), BTreeMap<TxIdx, U256>>
       │
       └── ParallelDb (per-tx Database adapter)
             ├── reads from MvMemory (higher-priority)
             ├── falls back to base_db
             └── records read_set for validation
```

### 新建 crate: `n42-parallel-evm`

| 文件 | 内容 |
|------|------|
| `src/lib.rs` | 主入口 `parallel_execute()`, 执行/验证/输出构建 |
| `src/mv_memory.rs` | MVCC 多版本存储 (DashMap + BTreeMap) |
| `src/parallel_db.rs` | 并行 Database 适配器 (SharedReadSet) |
| `src/scheduler.rs` | Block-STM 调度器 (Execute/Validate/Redo) |
| `src/types.rs` | 共享类型 (TxIdx, LocationKey, ReadEntry) |

### Benchmark 结果

```
=== N42 EVM Benchmark ===

--- ETH Transfer (21000 gas) ---
  Per tx:     576 ns
  Single-core TPS: 1,735,798

--- ERC-20 Transfer (SLOAD/SSTORE) ---
  Per tx:     1027 ns
  Single-core TPS: 973,439

--- Parallel ETH Transfers (Block-STM, 10 threads) ---
    100 txs: seq=  0.5ms  par=  16.3ms  speedup=0.03x
    500 txs: seq=  2.1ms  par=  77.5ms  speedup=0.03x
   1000 txs: seq=  3.8ms  par= 211.4ms  speedup=0.02x
   2000 txs: seq=  7.3ms  par=1382.0ms  speedup=0.01x
   5000 txs: seq= 20.3ms  par=9365.9ms  speedup=0.00x
```

---

## 关键发现

### 1. EVM 执行不是 TPS 瓶颈

单核 ETH 转账理论上限 **1.74M TPS**。2000 txs 仅需 7.3ms。
当前实测 1500 TPS 的瓶颈在 pipeline（共识+导入），不在 EVM。

### 2. Block-STM 对简单转账无效

MVCC 开销（DashMap 锁、Arc 克隆、读集追踪、每 tx 创建 EVM 上下文）远超
简单转账的执行时间（576ns）。并行 50-100x 慢于顺序。

### 3. Block-STM 对复杂合约有价值

当单 tx 执行时间 > 10μs（如 Uniswap swap ~100μs），MVCC 开销可被摊薄。
Grevm 2.1 在 Uniswap workload 上达到 11.25 gigagas/s。

### 4. 真正的 TPS 瓶颈分析

```
1s 出块周期内：
Build (7ms) → Consensus (~200ms) → Import (~200ms) → 下一个 Build
                                                      ↑ 关键延迟
```

500M gas / 21000 gas per tx = 23,809 txs/block 理论上限。
实际 ~1500 TPS = ~1500 txs/block，远低于 gas limit 上限。

瓶颈候选：
- **交易池吞吐**：stress test 发送速率
- **Pipeline 延迟**：build→consensus→import 串行等待
- **State root 计算**：每个区块结束时的 MPT 更新

---

## Phase 1.5：Speculative Build（推测性构建）

### 设计决策

Leader 轮换采用 `view % n` 的 round-robin 机制。7 节点每 7 个 view 才轮到同一 leader。
因此 speculative build 的核心受益者是：**下一个 view 的 leader（当前是 follower）**。

### 架构

```
当前 leader (node A) 构建 Block N
     │
     ├── 广播 Block N → followers
     │
     ├── Leader eager import: new_payload + fcu → 信号 eager_import_done_tx
     │
     └── Follower (node B, 下一个 leader):
           ├── 收到 Block N → follower eager import
           ├── Eager import 完成 → 信号 eager_import_done_tx
           ├── handle_eager_import_done: is_leader_for_view(V+1)? → YES
           ├── ★ 立即开始构建 Block N+1（不等 consensus commit）
           └── ViewChanged(V+1) → speculative build 已在进行，跳过重复构建
```

### 改动

| 文件 | 改动 |
|------|------|
| `state_machine.rs` | 新增 `is_leader_for_view(view)` 方法 |
| `mod.rs` | 新增 `eager_import_done_tx/rx` 信号通道 + `speculative_build_hash` |
| `mod.rs` (event loop) | 新增 `eager_import_done_rx` 分支 |
| `execution_bridge.rs` | Leader/follower eager import 成功后发送信号 |
| `consensus_loop.rs` | 新增 `handle_eager_import_done()`，修改 `finalize/view_changed` 避免双重构建 |

### 时序优化

```
优化前（下一个 leader 视角）：
  Consensus(200ms) → ViewChanged → trigger_build → warmup(10ms) → build(3ms) → broadcast
  总延迟: ~213ms 从 consensus 开始到下一个块广播

优化后：
  Eager import(100ms) → speculative build starts → build(3ms) ready
  ...meanwhile consensus(200ms) → ViewChanged → build already done → broadcast
  总延迟: ~200ms（consensus 时间主导，build 被隐藏）
  节省: ~13ms per block
```

---

## 后续优化方向

### 短期（提升现有 TPS 到 5000+）

1. **Pipeline 深度优化**
   - Speculative build（leader 在 commit 前就开始构建 N+1）
   - 异步 state root（不在关键路径上等待）

2. **Transaction Pool 优化**
   - 提高 tx pool 入池速率（批量导入）
   - 预热 state 缓存（发送前预加载账户数据）

3. **Gas Limit 调优**
   - 已设为 500M，确认实际 block gas 使用率

### 中期（并行 EVM 真正发挥价值）

1. **优化 Block-STM 开销**
   - 无冲突 fast path（跳过验证）
   - 批量 EVM 上下文创建
   - 替换 DashMap 为低开销替代方案

2. **集成到 payload builder**
   - 替换 `default_ethereum_payload` 中的顺序执行循环
   - 复杂合约 workload 下测试

3. **State I/O 并行化**
   - 并行预加载 tx 涉及的账户状态
   - 异步磁盘 I/O
