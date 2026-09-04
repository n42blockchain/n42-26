# Devlog 141 — 三个月代码审计与 n42-rs 对照优化

日期：2026-09-04（UTC）。本轮修复保留在工作区，未提交、部署或修改参照仓。

## 范围与方法

- 窗口：2026-06-04 至审计时 HEAD `ba402c477ac86810656af534e21da356579ec2ba`。
- 窗口前基线：`d8bce608f48aab6d60ce2250ccfcd2bfdff52b8b`。
- `git log --since=2026-06-04` 共 712 个提交（包含合并）：六月 160、七月 161、八月 390、九月 1。
- `crates/`、`bin/` 的净 diff 涉及 177 个文件，新增 56,448 行、删除 8,837 行。
- 参照：本地 `../n42-rs`，HEAD `48263495dd5783ca946cb740340405b0dddfd911`。
- 依赖环境：本地 `../reth` HEAD `23316e3ff`，Rust 1.97.1；未改变依赖、锁文件或网络协议版本。

全量梳理提交和变更文件，沿高风险调用链做定向深读，结合既有七月/八月审计结论和当前代码核验。
这不是对 177 个文件逐行无缺陷的证明。重点覆盖共识安全边界、追赶失败分类、区块缓存、
执行接口、崩溃恢复、SBMT/Twig 证明以及并行 EVM 的验证顺序。脚本、UI、第三方依赖没有逐项安全审计。

## 已确认并修复

### F1 / 高：超长 SBMT 证明使手机验证路径 panic

位置：[BmtProof::verify](../crates/n42-bmt-core/src/lib.rs)，调用来自
[手机 C 接口](../crates/n42-mobile-ffi/src/lib.rs) 的 `n42_verify_state_proof` 等入口。

原实现直接以 `base_depth + sibling_index` 访问 256 位 key。分片内起始深度为 4，
对端只需在正常外层分片证明中放入 253 个 sibling，就能访问 key 的第 32 字节并 panic。
约 8 KiB 的输入即可触发；跨 `extern "C"` 边界的 panic 会终止进程。

修复：先检查 `base_depth <= 256` 且 `siblings.len() <= 256 - base_depth`，
同时保护普通折叠和非成员证明的前缀检查，避免加法溢出。合法的最深 252 层分片内证明仍可验证。

证据：新增用例在旧实现上实际报 `index out of bounds: the len is 32 but the index is 32`。
覆盖超长分片证明、`usize::MAX` 起始深度、非成员证明、最深合法证明；C 接口回归要求返回错误码 2。
本地 n42-rs 的 `crates/n42/bmt-core/src/lib.rs` 保留同一缺口，不能视为安全参考实现。

### F2 / 高：坏区块体可让合法 Gov5 区块被永久拒绝

位置：[Gov5BlockError::is_permanent](../crates/n42-network/src/gov5_block.rs)，
[block-by-hash 响应处理](../crates/n42-network/src/service.rs)。

原判断为“解码错误为永久错误，且响应头哈希等于请求哈希”。但区块哈希只认证区块头；
对端可以保留合法头，删除交易或把辅助列表改成字符串。这样的响应仍归属于原哈希，
却因 RLP/交易根错误进入 `Gov5BlockFetchFailed { permanent: true }`，
随后 participant/observer 不再请求该块，可能阻塞祖先追赶。

修复：只有匹配哈希的 `HeaderProfile` 错误属于区块头本身的确定错误。
RLP、交易根、payload 重建/哈希、错哈希响应均保持可重试。正常区块仍执行完整交易根验证。

证据：保留合法头、仅篡改辅助列表的用例在旧实现上失败。另覆盖非空交易块的交易被删除、
头哈希仍匹配、随后收到正确区块体可正常解码的情况。修正了原有“所有内容错误均永久”的错误测试前提。

### F3 / 高：执行血缘日志的半条尾记录破坏下一次恢复

位置：[append_execution_lineage_proof](../crates/n42-consensus-service/src/persistence.rs)。

恢复函数只在内存中忽略末尾不足 60 字节的记录。重启后的 append 接着这些字节写新记录，
使半条旧记录和新记录前部组成错误的完整记录，之后恢复报 checksum mismatch。

修复：追加前按 60 字节边界检查文件长度，用独立可写句柄截断残缺尾部并同步，之后再追加。
不删除任何完整记录，完整记录校验失败仍拒绝恢复；读取函数保持只读。

证据：扩展既有测试为“写完整记录 → 写半条 → 恢复 → 追加 → 再恢复”，
旧代码实际报 `execution-lineage record checksum mismatch`；修复后新旧记录均可恢复。

### F4 / 高：快照和日志缺少可依赖的目录持久化屏障

位置：[snapshot](../crates/n42-jmt/src/snapshot.rs)、[PersistentSbmt/PersistentTwig](../crates/n42-jmt/src/persistent.rs)、
[共识持久化](../crates/n42-consensus-service/src/persistence.rs)、[QMDB 持久化](../crates/n42-node/src/qmdb_state_root.rs)。

JMT/Twig 的快照在 rename 后忽略目录 open/fsync 错误，调用方可能认为快照已持久化并截断 WAL。
共识/QMDB 快照没有 rename 后的目录屏障，新建 vote-log/WAL/lineage 文件也缺少目录项持久化保证。
断电或内核崩溃时，文件内容的 fsync 不能单独保证 rename/create 已落盘。

修复：共享 `sync_parent_directory`，Unix 下严格传播目录 open/fsync 错误，正确处理裸相对文件名。
JMT/SBMT/Twig 快照统一走“写临时文件 → 文件同步 → rename → 目录同步”，错误返回后由原有
poison/检查点逻辑保留 WAL。补齐共识和 QMDB 快照、vote-log 初始化、SBMT/Twig WAL 打开、
QMDB 首条 WAL 和 lineage 首次创建的目录同步；QMDB 残尾截断也同步后再继续。

验证包含 rename 已完成后的目录同步故障注入、目录打开失败，以及既有 WAL 重放、检查点失败和
poison 恢复测试。没有进行真实断电测试；非 Unix 平台保持原来的文件同步/rename 语义，
不据此声称获得与 Unix 等同的断电保证。新建多层父目录的祖先目录持久化也没有在本轮证明。

### F5 / 中：错哈希响应会删除已缓存块并使 FIFO 膨胀

位置：[cache_gov5_served_block](../crates/n42-network/src/service.rs)。

原流程先把响应加入服务缓存，再检查请求哈希；不匹配时从 map 删除响应块，却不移除 FIFO 项。
若响应块原本已缓存，会删除仍有用的合法块；反复请求错误响应时，map 长度不增长但 FIFO 可持续增长。

修复：先解析头并检查期望哈希，再进行任何缓存变更。统一入口返回 `UnexpectedBlockHash`，
错响应不改变缓存或顺序。新增重复错响应、已缓存块保护、容量和 FIFO 一致性回归。

### F6 / 中：远程执行请求没有期限，异常响应身份未经核对

位置：[EngineApiRpcExecutionLayer](../crates/n42-el-rpc/src/lib.rs)。

原 `reqwest::Client::new()` 请求未配置总超时。外部 `resolve_payload` 有超时，
但 inline 的 FCU 调用仍可能一直等待远程 EL。JSON-RPC 解析也没有检查协议版本和响应 id。

修复：默认 8 秒请求期限，覆盖连接、响应头和响应体；提供 `with_timeout` 供调用方配置。
检查 HTTP 状态和 `jsonrpc == "2.0"`、响应 id 等于请求 id。没有增加会重复执行请求的自动重试。

JSON-RPC 错 id、缺 id、字符串 id、错/缺版本均有回归。沙箱禁止创建套接字，
本轮未用真实慢速 HTTP 服务验证超时及 keep-alive 行为，需在允许 loopback 的环境补测。

### F7 / 中：远程构建记录无界，重复领取丢失 beacon root

位置：同上，`parent_beacon` / `resolve_payload`。

原 HashMap 只在成功领取时删除。超时、废弃或未领取的构建无限积累；再次领取同一 payload id
会把已删除的 root 默认为零，非零 beacon root 因而被替换。

参考 n42-rs 的 `MAX_TRACKED_BUILDS = 16`，改为 16 项 FIFO，覆盖同 id 时不重复入队，
成功领取不删除近期元数据。未知或已淘汰 id 明确报错，不再伪造零 root。
测试覆盖废弃构建上限、更新去重、重复读取、非零 root 和 envelope 元数据保留。

## 参考 n42-rs 落地的优化与取舍

| 参照与依据 | 本仓处理 | 边界 |
|---|---|---|
| `15122c4df`，已持有区块避免重复解码 | 服务缓存对字节完全相同的 RLP 直接复用已有分配，省去交易解码、交易根重算和再次复制缓存 | 头哈希相同但字节变化的响应仍做完整校验；相等比较仍是 O(字节数) |
| `h2-el-rpc/src/engine.rs` 的 16 项构建记录 | 有界 FIFO + 保留近期 beacon root，见 F7 | 维持本仓 Prague V4 接口，不移植新分叉协议 |
| `h2-el-rpc/src/transport.rs` 的显式超时 | 请求总期限，见 F6 | 8 秒可由构造函数调整；大块运行环境需按 EL 延迟配置 |
| n42-rs 避免重复处理 payload 的方向 | envelope 先消费成 payload/sidecar，再读取元数据 | 删除的是 `Vec<Bytes>` 克隆及逐交易引用计数操作；原代码也没有深拷贝每笔交易的底层字节 |
| n42-rs 的 120ms body-fetch grace | 未直接引入 | 本仓已有单 peer fetch、轮换、冷却与跨路径取消；照搬延迟可能损害历史追赶，需分别测 live/catch-up |
| n42-rs 的线程化交易池、leader tenure、raw TCP | 未直接引入 | 属于更大执行/共识改造，需要同硬件和相同出块规则的对照测试 |

没有从 n42-rs 的 TPS 记录推算本仓收益。本轮确认的是删除重复工作与资源边界，未进行 TPS 压测。

## 复核结果与仍需验证的范围

- 共识：检查 R1/R2 持久化水位、QC 域/验证者集合、H2 extendsJustify、异步构建 view/parent 上下文。
  八月已修复的父块原子导入通知和同 hash 跨 view 的提交推进仍在；未发现可复现的新共识签名绕过。
- 执行：检查 scheduler 的前驱验证完成门槛、MVCC storage wipe、动态 coinbase 回退和 live/replay 路径分流。
  没有把实验性并行执行替换进 live 路径；已有差分单测通过不能替代执行规范全量认证。
- 网络：保持请求/响应帧、Snappy 展开、direct 来源认证、范围同步 count/step/span 的原有限制。
  1024 项 serve cache 在每块 1 MiB 时仅 RLP 就约 1 GiB；大块 direct 的单帧上限也不等于全局内存预算。
  后续应验证按字节计量的缓存/并发背压，尤其是 decode 在来源授权前运行的资源成本。
- 独立 EL 客户端仍固定 Prague Engine API 版本，并不因注释中的“ANY EL”而自动支持所有分叉。
  Cancun/Gov5/Amsterdam 协商需要独立互操作测试和接口调整。
- 未运行真实多节点网络、Gov5 跨客户端、长期负载、断电恢复或 Android/iOS 设备测试；
  未更新 RustSec 数据库，因此没有给出“依赖不存在已知漏洞”的结论。

## 验证记录

已在旧代码上复现 F1、F2、F3 的失败，再验证修复。
全工作区库单测首轮通过 1,382 项，3 项既有 ignored。补齐交易体篡改和 envelope 转换回归后，
最终重新运行受影响的 7 个库：

| 库 | 通过 | 原有 ignored | 沙箱限制过滤 |
|---|---:|---:|---:|
| n42-bmt-core | 9 | 0 | 0 |
| n42-consensus-service | 211 | 2 | 0 |
| n42-el-rpc | 10 | 0 | 0 |
| n42-jmt | 108 | 0 | 0 |
| n42-mobile-ffi | 47 | 0 | 0 |
| n42-network | 195 | 0 | 2 |
| n42-node | 175 | 1 | 2 |
| 合计 | 755 | 3 | 4 |

其余 8 个未修改库的 629 项单测在全工作区运行中通过，共覆盖 1,384 个不同库单测。
另运行 91 项集成测试，全部通过：

```sh
cargo test --offline --locked -p n42-consensus --test integration_test --test chaos_7node --quiet
# integration_test: 67 passed; chaos_7node: 12 passed
cargo test --offline --locked -p n42-node --test stream_v2_pipeline --quiet
# stream_v2_pipeline: 12 passed
```

这些是进程内共识故障模拟及真实 EVM/手机重放测试，不是实际网络七节点运行。
修改文件的 `rustfmt --check`、`git diff --check` 均通过。

全工作区所有 targets 的 Clippy：

```sh
cargo clippy --offline --locked --workspace --all-targets -- -D warnings
```

通过。依赖 `proc-macro-error2 2.0.1` 有编译器 future-incompatibility 提示，未改变依赖。

单测命令：

```sh
cargo test --offline --locked --workspace --lib --quiet -- \
  --skip transport::tests::tunes_owned_udp_socket_buffers \
  --skip transport::tests::interop_observer_completes_tcp_noise_yamux_handshake \
  --skip ingest::tests::batch_over_byte_ceiling_drops_the_connection \
  --skip ingest::tests::oversized_batch_header_is_rejected_before_allocating
```

这四项在未过滤的初次运行中均因 `Operation not permitted`（套接字创建/监听）失败，
并非已通过；保留原测试，命令行显式过滤。新增测试不依赖网络。
