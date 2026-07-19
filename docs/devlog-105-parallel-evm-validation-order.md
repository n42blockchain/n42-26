# Devlog 105: Block-STM validation 完成顺序修复

日期：2026-07-18  
来源：P1-4 提交前 `cargo test --workspace` 门禁

## 现象

与网络改动无文件交集的 `n42-parallel-evm` 差分测试出现可重复的非确定性错误：

- `deferred_coinbase_tests::hot_recipient_matches_sequential` 第二次独立重跑即失败，热收款账户
  余额为 14900，顺序执行正确值为 15000；
- 同一轮之前还观察到 storage conflict 精确计数偶发少 1。

这不是断言噪声，而是并行执行结果漏掉一个依赖更新，属于执行有效性问题。

## 根因

调度器声称按交易顺序 validation，但 `val_cursor` 在任务被 worker **领取**时就前移。tx `v`
尚在验证时，另一个 worker 可立即领取 tx `v+1`。因此顺序只约束了领取次序，没有约束完成次序；
高位交易可能对尚未 settle 的热账户中间值通过验证，在特定 abort 交错下不再被重验。

已有的 value-based read-set 与 abort CAS 都是必要保护，但不能替代“低位前缀已 VALIDATED”这一
Block-STM 正确性前提。

## 修复

`next_task` 领取 validation `v` 前，除要求 `status[v] == EXECUTED` 外，新增要求：

- `v == 0`；或
- `status[v-1] == VALIDATED`。

这样 validation 的完成顺序与交易顺序一致。若更低交易 abort，现有逻辑仍会把所有高位
EXECUTED/VALIDATED 交易原子改为 REDO 并回退 cursor。

新增调度器单测刻意领取 tx0 的 validation 但不完成，断言 tx1 不能被领取；完成 tx0 后 tx1
才可进入 validation。

## 验证

验证结果：

- `hot_recipient_matches_sequential`：30/30 连续通过（修复前第 2 次即失败）；
- `cargo test -p n42-parallel-evm`：全包 10/10 轮通过，每轮 10/10 tests；
- 新增 scheduler 竞态单测：通过；
- `cargo clippy -p n42-parallel-evm --all-targets -- -D warnings`：通过。

workspace check/clippy/test 在前置提交与 P1-4 组合后再执行。该修复使用独立提交/分支，避免把
执行有效性修复藏进网络 diff。
