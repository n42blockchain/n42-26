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

## 状态

修复 1/2/3 已落地,root/结果不变(par_ok=true 收敛正确),parallel-evm 测试 + clippy 绿。
deferred coinbase 待续。
