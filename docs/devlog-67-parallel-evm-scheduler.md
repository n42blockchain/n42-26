# devlog-67 — parallel-evm:消除两处 O(n²) + 定位 coinbase 级联

## 起因(devlog-66 #3)

samply 显示 parallel-evm 并行段 35% 线程等待。n42-evm-bench 的 Block-STM 段实测暴露真问题:

| txs | seq(修复前) | par(修复前) |
|-----|------------|------------|
| 5000 | **195243 ms** | 187909 ms |

5000 笔**互不冲突**转账,顺序执行竟要 195 秒 —— 明显 O(n²)。

## 修复 1:`MvMemory::clear_tx` 全表扫描(顺序路径 7500x)

`clear_tx(tx_idx)` 在重执行前清掉该 tx 的旧写。原实现:索引命中走 O(writes),**索引缺失时
回退全表扫描**(扫所有 account/storage entry 调 `remove(&tx_idx)`)。但"索引缺失"恰恰是
**每笔 tx 首次执行**的常态(还没写过任何东西)—— 于是每笔首执行都全表扫一遍 = O(n²)。

修复:索引缺失 = 该 tx 没写过 = no-op(`write_account`/`write_storage` 总会登记索引,回退
分支是死代码且有害)。**seq 5000: 195243 ms → 26 ms。**

## 修复 2:`Scheduler::try_get_task` 的 O(n) 状态扫描

每次取任务都 `for i in 0..num_txs` 线性扫 REDO 标记 → 任务获取 O(n)、全块 O(n²)。
改为 **REDO 队列**(`Mutex<VecDeque>`):`abort_and_reschedule` push 被作废的 tx,取任务时
O(1) pop + `REDO→PENDING` CAS 确认(status 数组仍是真值源,跳过陈旧/重复条目)。

## 修复 3:`N42_PARALLEL_THRESHOLD` 被 `OnceLock` 冻结

阈值用 `OnceLock` 缓存,进程内第一次读后永久固定 —— bench/测试改环境变量无效。改为每块读一次
(每块一次 env 读,纳秒级)。

## 仍然慢的真相:coinbase 写冲突级联(并行路径)

修复后 seq 5000 = 26 ms,但 **par 5000 仍 ~198 s**。根因不是调度,而是 Block-STM 的经典死穴:

> bench 的"无冲突"转账**全部写同一个 beneficiary(coinbase)账户** —— 每笔 tx 付 gas 费给
> 同一 coinbase,于是 tx_i 读到 tx_{i-1} 写的 coinbase 余额 → 全局串行依赖 → 每笔验证失败 →
> `abort_and_reschedule` 把所有更高 index 标 REDO → O(n²) 重执行。

这是**真实问题**(真实区块每笔都付 coinbase,gas_price>0),不是 bench 假象。Aptos Block-STM
的标准解法:coinbase 余额增量是**可交换累加**,不当普通读写,在 commit 时聚合(`lazy/
commutative` 累加器),从读集里剔除 coinbase 余额。

### 下一步(#3 续):deferred coinbase

在 `MvMemory` 给 beneficiary 余额增量设可交换累加器(每 tx 记 delta,验证不比对 coinbase 余额,
commit 时求和应用)。预期把转账块从 O(n²) 降到近线性,真正吃满多核。属中等改动,单列一轮。

## 修复 4:deferred coinbase(消除并行级联,2257x)

按"加法可交换"的洞察实现:
- **盲读**:`ParallelDb` 对 beneficiary 返回 base 值且**不进 read_set** → fee 入账不产生读依赖。
- **commutative delta**:`execute_single_tx` 抽出 `delta = new_balance - base_balance`,存进
  `MvMemory.bene_deltas`(不进 versioned accounts),`clear_tx` 重执行时一并清除。
- **commit 物化**:最终 `beneficiary = base + Σ delta`(加法,顺序无关),在 `build_output` 后
  一次性写回。
- **守卫**:beneficiary 若是某 tx 的 sender(nonce 变,不可交换)→ `bene_is_sender` 自动关闭
  deferral,退回正常 versioned 路径;tx 若改了 beneficiary 的 nonce/code → 该 tx 退回普通写。
  开关 `N42_DEFERRED_COINBASE=0`。
- **soundness**:只要无 tx 分支于 coinbase 的块内中间余额(标准转账/ERC-20/DeFi 均不读
  `block.coinbase` 余额)即与顺序逐字节一致。

**对拍测试**(`deferred_parallel_matches_sequential_state`):200 笔独立转账付费给共享 coinbase,
deferred 并行的**每个账户余额**(含 coinbase 费总额 `200×21000×7`)**逐一等于顺序执行**。
第二个测试验证 bene-as-sender 守卫。

实测(evm-bench Block-STM 段):

| txs | deferred OFF | deferred ON | 提速 |
|-----|-------------|-------------|------|
| 2000 | 19295 ms | 47 ms | 410x |
| 5000 | **227642 ms** | **100.9 ms** | **2257x** |

并行从 O(n²) 级联恢复为**近线性**(~50 txs/ms)。

> 注:trivial 转账下 par(100ms)仍 > seq(28ms)—— 这是并行协调的固定开销在"每笔仅 5µs"
> 工作量下未摊平,**不是级联**(级联已除)。真实区块(合约调用、多 SLOAD)每笔工作量大得多,
> 并行才赢;而那些块同样共享 coinbase,没有 deferred 就会被级联打死 —— 所以 deferred 是并行
> 可用的前提。trivial-only 大块可由阈值/自适应调度走顺序,属调优。

## 状态

修复 1/2/3/4 全部落地,结果与顺序执行逐账户一致(对拍测试),parallel-evm 4 测试 + clippy 绿。
全局优化 #3 完成。
