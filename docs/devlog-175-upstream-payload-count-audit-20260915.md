# 上游增量复核：自建块的交易列表与换主卡顿

日期：2026-09-15。没有启动构建、签名、fleet 或压测；没有新增 TPS 成绩。
本轮读取本机已经更新的远程跟踪引用，未执行 fetch，也未修改另两个仓库。

## 新增上游版本

| 仓库 | 上次审阅 | 本次固定版本 | 增量 |
| --- | --- | --- | --- |
| n42-rs | 466c1839791afadba7f1dd4d48c5518440dcafba | 00e6653fdedc308a825523bfa7c543eb8eb752d3 | e5d859d82 修复自建块 payload 交易列表，00e6653fd 更新实测与未解问题 |
| N42-gov5 | 074f9c8f22a7755de3bbe3e4455dec6a32b2a4d2 | 69af0130955a700a0a0718463d98756afb0cc876 | 仅交接文档更新，暂停且交给 n42-rs，35zzq 未运行 |

## n42-rs 修复的因果链

固定版本的 `bin/n42/src/payload_serve.rs` 原先向自建块的 `newPayload` 传入空交易列表，
依赖已执行块插入树成功。换主后重提同父兄弟块时，插入可能被判过时而丢弃。
随后 Engine 执行的交易数量来自 payload 自身，而非转换函数返回的缓存块：
即使缓存块有完整交易，空列表仍使执行结果为零笔、零收据，污染后续 nonce/state。
此前仅修改迭代器来源的尝试没有解决 transaction_count，已被撤回。

e5d859d82 在 worker pool 将自建块交易按顺序编码成 EIP-2718 字节，完整传入
`execution_data_from_raw_parts`。插入成功仍可命中已执行结果，插入失败则有足够
数据供 Engine 真正执行。不能因缓存命中率高就省略回退路径的数据。

`docs/FLEET7_PATH_AUDIT.md` 报告 loop159 的 W/V7/C7/B7/V8/C8 均无拒绝块和
不完整执行，C8 实际触发兄弟块插入被丢弃的回退；V7/V8 的 QMDB 读取验证为
2,097,152 / 1,048,576 次、零不一致。这是**上游报告**，本轮没有重算其原始日志。
其首窗口约 271.5k–293.3k（W 预热 303.9k 不计），不能作为本项目四节点成绩，
也不能混同整轮成功提交 TPS。

另一项必须分清：`hotstuff_consensus.rs` 的 deferred execution 分支在收据数量
不足时改为 error 日志，但仍返回 requests-hash 检查结果。日志升级本身不等于
拒绝不完整执行。本项目不移植该 deferred validation 分支。

## 当前项目对应路径的核对结果

1. `crates/n42-node/src/el/reth_engine.rs::to_built_block` 从实际完整块调用
   `block_to_payload`，没有构造 header-only 空交易列表。
2. `crates/n42-network/src/gov5_block.rs::normalize_native_payload` 从输入 payload
   恢复完整块，修改原生头字段后用 `ExecutionData::from_block_unchecked(hash, &block)`
   重建 payload，保留交易体。
3. `crates/n42-consensus-service/src/orchestrator/execution_bridge.rs::handle_built_payload`
   在 H2-v4 分支向 `new_payload_for` 传入完整 normalized execution_data，并在真实
   Engine 返回 Valid 后才释放提案。没有采用上游“header-only + 缓存插入必成功”的假设。
4. `crates/n42-node/src/engine_validator.rs` 从 payload 自身解析交易；记录的原生头
   用于原生哈希/头字段重建，不用缓存块体替换空 payload 的交易列表。
5. 本地干净 Reth 的 `payload_validator.rs` 确实以 `input.transaction_count()`
   驱动执行/收据任务，同时 `tx_iterator_for` 的 Payload 分支取 payload 自身。
   这印证上游教训；保留两者一致，不移植只替换迭代器的失败方案。
6. `crates/n42-consensus/src/adapter.rs` 的 Gov5H2 分支对本块原生收据根不一致
   返回错误，再执行标准化后的其余执行检查；保持当前块验证。

现有 `crates/n42-node/tests/common/parallel_qmdb_builder.rs` 已覆盖 4096 笔真实
交易的 native normalize → wire → payload JSON 一致性，随后以
`BlockOrPayload::Payload` 进入真实 Engine，并将完整 execution result/state 与
串行 oracle 对拍。此前 Debug/Release 通过的证据见 devlog-173；本轮仅核对
源代码，没有重新运行 Cargo。该证据不覆盖七节点换主/兄弟块回退竞态，也不证明
四节点 H2 网络达标。当前代码不需要照搬这次上游修复。

## 尚未解决的换主问题决定下一步测量

上游 loop159 每腿仍有 1–6 次 TC。审计记录某次 Decide 后 forkchoice 6.5 秒未到达
EL；追赶时连续启动 12–27 个提前构块任务，取消上层任务未取消 EL job，旧父状态
的 nonce 失效使并行路径退回昂贵的串行处理。`--builder.interval 60` 实际是 60 秒，
不能误读为 60 毫秒；本项目 launcher 明确使用 `50ms`。

因此不能仅凭修复零收据就默认打开 build-on-seal。获得硬件窗口后，先验证本项目
四节点正常提交，再测跨换主窗口，并关联 Decide、import 完成、forkchoice 发送/完成、
提案与 Engine payload job 数量；证明串行关键路径与取消行为后再决定是否提前构块。
H2、完整验签、二叉树 QMDB-only 和本块根验证保持不变。

## 共享硬件仍未交接

Gov5 新提交确认 00:10 EDT 已暂停并把机器交给 n42-rs，35zzq 仍未运行。
其 Shmem 门槛修正为不超过 22 GB，避免整数 GB 截断放过 22.6 GB；A/B 应记录
原始字节/KiB 数值再比较，不能只记录四舍五入后的显示值。

本轮重新读共享留言，末条仍为 00:06 EDT；未见双路径 claim，也没有 Rust 向本会话
交接的确认。当前 `/proc` 只有沙箱进程，不能用于宿主机安静证明；共享目录写权限
也仍未开放。已准备的协调消息尚未送达，详见 devlog-174。重任务保持停止。
