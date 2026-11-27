# devlog-197：第二十个 reth 补丁——限制 RocksDB memtable 历史与写缓冲预算

日期：2026-11-27。分支 `fix/reth-rocksdb-memtable-history`（自 `origin/main` `b64f650`）。
reth 适配分支 `n42/v2.11.0-rocksdb-memtable`，在十九补丁基线 `5a81f36296` 上加一个提交
`61b79664195f5ffe61870f612620085ac00358d8`。状态：**补丁、锁文件、脚本与测试完成；舰队验证待做**。

## 背景

96 分钟混合负载运行中，节点 RocksDB memtable（reth 指标 `rocksdb.memtable_size`，即
`rocksdb.size-all-mem-tables`）从 238 MiB 涨到 375 MiB 且仍在增长。

原因：reth 的 RocksDB provider（`crates/storage/provider/src/providers/rocksdb/provider.rs`）以
`OptimisticTransactionDB` 打开数据库。`OptimisticTransactionDB::Open`
（librocksdb-sys 10.4.2，`utilities/transactions/optimistic_transaction_db_impl.cc`）把每个
`max_write_buffer_size_to_maintain == 0` 的列族改成 -1，即
`max_write_buffer_number x write_buffer_size` = 2 × 128 MiB 的 memtable 历史，用于乐观事务在内存中做冲突检查。
flush 后的 memtable 进入历史（`MemTableListVersion::Remove`）而不是释放，继续计入写缓冲管理器
（4 GiB），只在历史超过上限时由后续写入裁剪。五个 reth 列族各自最多攒到约 256 MiB。

## 验证"节点从不依赖冲突检查"

逐条列出 RW 数据库上的写/事务路径（`RocksDBProviderInner::db_rw()` 的全部调用者）：

| 路径 | 实现 | 冲突检查 |
|------|------|----------|
| `put_cf` / `delete_cf`（`RocksDBProvider::put/delete`） | `OptimisticTransactionDB::put_cf` → `DB::Put` | 无 |
| `delete_range_cf` | `DB::DeleteRange` | 无 |
| `commit_batch`、`write_batch`、`RocksDBBatch::commit` 及自动提交（`auto_commit_threshold`） | `write_opt(WriteBatchWithTransaction)` → `rocksdb_optimistictransactiondb_write` → `OptimisticTransactionDBImpl::Write`，只拒绝 DeleteRange，然后直接 `DB::Write` | 无 |
| 快照/迭代器（`RocksReadSnapshot`、`raw_iterator_cf_opt`） | 只读 | 无 |
| `RocksDBProvider::tx()` → `RocksTx`（`transaction_opt` + `Transaction::commit`） | 乐观事务，commit 时检查冲突；历史不足时返回 `TryAgain`，被映射成 `DatabaseError::Commit` | **有** |

`RocksTx` 的调用者全部在测试里：`provider.rs` 的 `mod tests` 与 `either_writer.rs` 的
`#[cfg(test)] mod rocksdb_tests`。n42-26 自身（crates/bin/vendor/tests）不引用 `RocksTx`、`OptimisticTransactionDB`
或 `rocksdb_provider().tx()`。编译级证明：临时给 `RocksDBProvider::tx` 加 `#[cfg(test)]`，n42-26
`cargo check --all-targets` 重新检查 reth-provider 及其全部下游（reth-engine-tree、reth-node-builder、
n42-node、n42-node-bin 等）后照常通过，说明节点依赖图中没有任何非测试代码调用它。所以历史只服务于测试中的乐观事务，
在节点里是死内存。

## 设计决策

- **历史上限设为 1 字节，而不是 0 或一个 memtable。** 0 会被 `OptimisticTransactionDB::Open` 改成 -1，关不掉；
  正数会被保留。按 `MemTableListVersion::TrimHistory` 的规则（`MemoryAllocatedBytesExcludingLast() + usage >= max`，
  最新一个 flush 掉的 memtable 不计入），上限为 1 时 flush 仍把最新的 memtable 放进历史，但下一次写该列族就释放它；
  上限为一个 memtable（n42-rs `be474d0f0` 的做法）则通常一直多占一个 memtable。两者最坏都是每列族保留一个 flush 后的
  memtable，所以预算停写不会死锁的论证相同；1 字节的稳态占用更低（见测量）。
- **默认开启，预算 512 MiB，`N42_ROCKSDB_MEMTABLE_MB` 可调，0 回到上游。** 写缓冲管理器 = 预算；
  每列族 memtable = `预算 / 32`（夹在 1–128 MiB，默认 16 MiB）。n42-rs 的版本默认关闭；这里按 owner 的批准默认开启，
  并保留 `N42_ROCKSDB_MEMTABLE_MB=0` 作回退开关（上游 4 GiB / 128 MiB / 历史 -1）。非数字值告警并用默认。
- **为什么 /32。** 停写条件是管理器用量 ≥ 预算；只有可变 memtable 超过预算一半（或 7/8）才触发 flush。
  不可由 flush 释放的只有历史，最坏每列族一个 memtable。reth 有 5 个表列族加 default 列族，
  6 × 预算/32 < 预算/2，停写总能以 flush 结束。
- **只改选项。** `write_buffer_size`、`max_write_buffer_size_to_maintain` 与写缓冲管理器大小都是运行时选项，
  不改 SST/WAL 格式；已有数据库直接打开（`test_reopens_*` 系列照常通过）。default 列族用数据库级 options 打开，
  同样加上限；为兼容降级而保留的未知列族也加上限。
- **未照搬的部分。** n42-rs 分支另有 `N42_ROCKSDB_WRITE_BUFFER_MB`、`N42_ROCKSDB_WRITE_BUFFER_STALL`
  两个旋钮，本仓库 reth 没有，未引入；也未引入其 `with_write_buffer_manager_size`。
  只加 `RocksDBBuilder::with_memtable_budget(Option<usize>)`（覆盖环境变量）。

## 实施细节

reth 提交 `61b79664`（`patches/reth-rocksdb-memtable-history.patch`，`git diff --abbrev=9` 生成，与已有补丁同格式）：

- `memtable_budget()` / `parse_memtable_budget()`：读 `N42_ROCKSDB_MEMTABLE_MB`；`bounded_memtable_size()`；
  常量 `DEFAULT_N42_MEMTABLE_BUDGET`（512 MiB）、`BOUNDED_MEMTABLE_HISTORY`（1）。
- `RocksDBBuilder`：新字段 `memtable_budget`、`memtable_history`（后者只有测试 setter）；
  `bound_memtables()` 应用到每个列族和 default 列族；`default_options()` 多一个写缓冲管理器大小参数。
- `RocksDBProvider::memtable_usage()` → `RocksDBMemtableUsage { all_bytes, unflushed_bytes }`：
  对 reth 表求和 `rocksdb.size-all-mem-tables`（含 flush 后保留的历史）与 `rocksdb.cur-size-all-mem-tables`
  （仅未 flush）。比 `table_stats()` 便宜（每列族两个属性）。
- 测试：`test_memtable_budget_parsing`、`test_memtable_budget_frees_flushed_memtables`（32 MiB 预算写 256 MiB，
  不停写、memtable 在预算内、保留的 flush 后 memtable 不超过每列族一个）、`test_memtable_usage_counts_reth_tables`、
  `bench_memtable_footprint`（ignored，测量用）。

n42-26：

- `reth-source.lock`：`patch_series` 追加 `reth-rocksdb-memtable-history`，`adapted_commit` 改为 `61b79664…`；
  `scripts/apply-reth-patches.sh`（补丁名单与默认 adapted revision）、`scripts/test_apply_reth_patches.py`、
  `CLAUDE.md`（二十个补丁、新 adapted commit）、`README.md`、`DEVLOG.md` 第 186 条的补丁数。CI 工作流只调用
  `apply-reth-patches.sh`，没有写死提交或数量。
- `N42_MEM` 行新增 `rocksdb_memtable_mib`（`size-all-mem-tables`，含 flush 后保留的 memtable——这正是本问题增长的量）
  与 `rocksdb_memtable_unflushed_mib`（`cur-size-all-mem-tables`），以及 gauge
  `n42_mem_rocksdb_memtable_bytes` / `n42_mem_rocksdb_memtable_unflushed_bytes`。来源在 `on_node_started`
  里经 `RocksDBProviderFactory::rocksdb_provider()` 注册（`memory_watch::set_rocksdb_source`）。
  此前 `N42_MEM` 没有 RocksDB 字段。

## 测量

`bench_memtable_footprint`（release，两个列族，4 MiB 一批经 `write_batch` 写 1 GiB，结束时读属性；三次运行，
内存数三次一致，时间取范围）：

| 值大小 | 配置 | 全部 memtable | 未 flush | flush 后仍保留 | 写入时间 |
|--------|------|---------------|----------|----------------|----------|
| 64 KiB | 上游（4 GiB、128 MiB、历史 -1） | 581 MiB | 70 MiB | 511 MiB | 1.27–1.43 s |
| 64 KiB | 预算 512 MiB，历史一个 memtable | 66 MiB | 32 MiB | 34 MiB | 1.15–1.27 s |
| 64 KiB | **预算 512 MiB（默认），历史 1 字节** | **32 MiB** | 32 MiB | 0 | 1.18–1.25 s |
| 64 KiB | 预算 1024 MiB，历史 1 字节 | 69 MiB | 69 MiB | 0 | 1.23–1.38 s |
| 64 KiB | 预算 128 MiB，历史 1 字节 | 17 MiB | 8 MiB | 9 MiB | 1.17–1.34 s |
| 64 B | 上游 | 577 MiB | 192–320 MiB | 256–385 MiB | 3.43–3.67 s |
| 64 B | 预算 512 MiB，历史一个 memtable | 63 MiB | 26 MiB | 37 MiB | 3.55–3.81 s |
| 64 B | **预算 512 MiB（默认），历史 1 字节** | **44 MiB** | 26 MiB | 18 MiB | 3.68–4.43 s |
| 64 B | 预算 1024 MiB，历史 1 字节 | 88 MiB | 88 MiB | 0 | 3.43–3.88 s |
| 64 B | 预算 128 MiB，历史 1 字节 | 14–20 MiB | 8 MiB | 6–12 MiB | 3.57–4.10 s |

上游的 511 MiB 正是每列族两个 128 MiB 的历史；n42-rs 的同类测量是 573 → 8 MiB（其预算 128 MiB）。
前台写入时间在噪声内（64 B 小值默认配置最多慢约 5–10%）；更小的 memtable 意味着更多 L0 文件与 compaction，
这部分不在前台时间里，需舰队验证。

## 验证

- reth：`cargo test -p reth-provider --lib rocksdb` 100 通过 / 1 ignored；`cargo clippy -p reth-provider --all-targets` 无警告。
  这些测试在默认预算下运行，包括 `RocksTx` 的提交/回滚测试。
- `bash scripts/check-reth-source.sh ../reth` 通过；`scripts/test_apply_reth_patches.py`（7）、
  `test_prepare_reth_upgrade.py`（5）、`test_reth_ci_baseline.py`（1）通过。
- 模拟 CI：干净检出官方 `b9abbcc`，运行 `apply-reth-patches.sh`，二十个补丁全部应用，结果树
  `792b30bb58…` 与 `61b79664` 的树完全相同；随后 `check-reth-source.sh` 对该检出也通过。
- n42-26：`cargo check --all-targets`、`cargo clippy --all-targets -- -D warnings` 通过；
  `cargo test -p n42-node --lib memory_watch` 通过；`n42-node-bin` release 构建完成。

## 后续

- 舰队验证：同一混合负载跑 ≥ 96 分钟，看 `N42_MEM rocksdb_memtable_mib` 是否稳定在几十 MiB，
  以及持久化耗时（`save_blocks_rocksdb_*`）、RocksDB L0 文件数 / compaction 是否变差；
  高 TPS 原生路线也要对比一次。若持久化变慢，可调大 `N42_ROCKSDB_MEMTABLE_MB`（例如 1024 → 32 MiB memtable）。
- 若以后真的要在生产中使用 `RocksTx`，需重新评估历史上限（历史不足时乐观提交返回 `TryAgain`）。

## Merge with main (2026-11-27)

- Main (#51) had meanwhile added a `structures=` source named `rocksdb_memtable_mib`, which summed `table_stats().memtable_size_bytes`. This branch logs `rocksdb_memtable_mib` and `rocksdb_memtable_unflushed_mib` as fields of their own, from `memtable_usage()`. Keeping both would print two fields with the same name, so the merge drops main's structure source and keeps this branch's, because it separates flushed history from unflushed memtables.
