# QMDB 历史操作按需读取与常驻缓存预算

日期：2026-09-14，接续 [devlog-153](devlog-153-qmdb-streaming-recovery-20260914.md)。
保持 H2 共识、二叉树 QMDB 状态读取与根校验。本轮解决历史操作全量驻留的问题，
**尚未证明 4 节点 1M TPS，也未测量 RSS 改善。**

## 持久化后才能淘汰

原来的 `blocks` 为每个历史块保存完整 `Vec<QmdbOperation>`。现在运行时索引保存
parent hash、root、可选的常驻操作和 WAL 定位信息；磁盘上的 checkpoint/WAL
结构与编码保持兼容。常驻操作按外层 Vec 容量及各 value 容量计费，默认预算
128 MiB，超出后按提交顺序释放已持久化记录的操作。

新块先保留操作，WAL 追加和 `sync_data` 成功后才登记该帧的 offset、长度与
checksum，然后淘汰缓存、发布读取版本。追加失败会移除候选索引并恢复父状态；
重试使用回滚后的正确偏移。同步期间旧的持久版本仍可读取。没有 WAL 的内存模式
保留唯一操作副本，超过预算只报告指标，不删掉恢复所需数据。

冷读独立打开 WAL 文件并定位，检查捕获的长度及 64 MiB 帧上限、捕获的 checksum、
payload 的实际 checksum，以及解码后的 block hash、parent hash、root。
文件缺失、帧变更或定位错块均报错，不返回空操作，也不回退为空状态。
分支恢复仍实际应用操作、验证二叉树根，checksum 不替代根验证。

已持久 WAL 前缀必须保持不变。未来实现压缩或截除前缀时，必须先协调替换这些
定位信息；本轮没有引入 WAL 压缩。

## 启动和旧 checkpoint

流式 WAL 加载逐帧解码校验后仅保留定位信息；空操作记录可保留零容量空 Vec。
启动分支校验按需读取操作，沿用上一轮的有预算 undo 与认证 base 重放。

旧 checkpoint 可能包含尚无 WAL 副本的历史操作。构造持久 store 时，先完成原有
验证，再逐条追加这些记录并同步，成功后释放其常驻副本。原 checkpoint 不改写；
再次打开不会重复追加已有记录。迁移中断留下的半帧按既有断尾规则修复，缺失记录
可从原 checkpoint 再次迁移。每条迁移记录单独同步，会增加首次打开的成本。

## 容量对照与验证

在完整 node 依赖图内，用相同实现分别设置无限操作预算和默认 128 MiB。样本为
200,000 个初始账户，每块更新 147,000 个不同账户，连续 24 块，使用 gov5 账户
编码及真实临时文件 WAL，同步未关闭。各块状态根同时与独立树对照，两组根全部相同。

| 第 24 块后 | 无限操作预算 | 128 MiB 预算 |
| --- | ---: | ---: |
| 常驻操作计费容量（bytes） | 254,016,000 | 127,008,000 |
| 冷记录数 | 0 | 12 |
| WAL 长度（bytes） | 169,347,936 | 169,347,936 |

该样本的操作容量下降 **50%**。随后回读第一块，验证其根和账户原始编码一致。
这是 debug 测试中的容量对照，不是速度、RSS、EVM 或四节点吞吐测试。

- node QMDB 回归：51 passed、7 ignored；另显式运行容量测量，1 passed。
- 强制 1-byte 操作/undo 预算，覆盖冷版本、已固定读取视图、分支切换、失败追加与
  重试；冷块相同操作逆序重试不会追加 WAL。
- 覆盖长度、payload、checksum 损坏，包括重新计算 checksum 的替换帧、错块定位、
  文件缺失；覆盖 checkpoint-only 迁移、重复打开与迁移断尾恢复。
- WAL 延迟测试确认同步前候选仍有常驻操作且没有冷定位，同步后才可释放。
- workspace all-targets clippy `-D warnings`、`git diff --check` 通过。

[源码、日志与结构化证据](benchmarks/20260914-qmdb-cold-operations/)。验证环境仍为
固定且补丁化的 reth `23316e3ff8adca8c3bd5085ff0565fcae019202a`，使用 locked/offline 构建。
复现命令（依赖准备见此前记录）：

```sh
CARGO_BUILD_JOBS=2 cargo test --locked --offline -p n42-node --lib qmdb_
CARGO_BUILD_JOBS=2 cargo test --locked --offline -p n42-node --lib bench_persistent_operation_capacity -- --ignored --nocapture
CARGO_BUILD_JOBS=2 cargo clippy --locked --offline --workspace --all-targets -- -D warnings
```

## 剩余边界

128 MiB 只限制长期保留的已解码操作容量。正在提交的候选、冷读取帧及解码结果、
undo、读取版本缓存、树本身、分支索引和操作队列的元数据均需另外计费。
旧 checkpoint 的全量解码仍有启动瞬时内存开销；全量 checkpoint 写入辅助路径
也会临时物化操作。WAL 长度和分支元数据仍随历史增长。

冷分支重放会在状态锁内读取磁盘，读取延迟仍需测量和优化。新增指标为
`n42_qmdb_operations_resident_bytes`、`n42_qmdb_operations_over_budget_bytes`、
`n42_qmdb_cold_operations_read_ms` 和带 outcome 的 `n42_qmdb_cold_operations_reads_total`。

上游审阅依据仍为前面固定的本地引用，未声称刷新远端。真实四节点连续窗口、
重启追赶和 1M TPS 验收仍待可用网络环境，目标继续进行中。
