# Devlog 140 — 2026-08 月度代码审计与修复

日期：2026-08-31（UTC）

## 范围与方法

- 基线：`c970f5895bce519af72765f91b83d9297e07909d`（2026-07-31 之前的最后提交）。
- 审计至：2026-08-31 的 `main`。
- 月内共 389 个提交；排除 merge、文档和脚本后，重点复核 35 个生产代码提交。
- 高风险面：H2-v4 投票安全、异步提交状态机、Gov5 范围追赶、QUIC/direct
  认证与排队、历史/实时 EVM 路径分流、QMDB/WAL、网络帧与内存上限。
- 本轮只做静态审计、单元测试、Clippy 和编译检查；未启动链、重负载或压测。

## 已确认并修复

### 1. H2 `extendsJustify` 在追赶与竞态路径可退化为父块未知

严重度：高。

`fb9c99d` 增加了“提案必须是 justify QC 所指块的子块”规则，但父块元数据通过
`remember_parent` 与 `BlockImported` 两次独立调用写入。只有 eager-import 主路径调用了
前者；Gov5 canonical range 追赶、历史同步及重试仅发送 `BlockImported`。此外提交处理会
预先 drain eager completion，再延后释放 H2 投票，扩大了两次更新之间的竞态面。

修复：

- 新增原子事件 `BlockImportedWithParent { block_hash, parent_hash }`；删除公开的
  `remember_parent`。
- 所有经过 execution `Valid` 的路径都在同一事件中携带真实父块：leader/follower eager
  import、历史同步、同步重试、Gov5 多批 canonical range 追赶。
- 原生模式仅缓存 payload 的乐观通知继续使用不带父块的 `BlockImported`，语义明确分离。
- 父块缓存与 import-evidence 共用同一 FIFO 生命周期，防止延迟元数据形成孤儿项或无界增长。
- 回归覆盖“先导入、后收到 proposal 的 sibling 仍不得投票”和父块缓存随导入证据淘汰。

### 2. 同一执行块跨 view 重用时，后续异步提交可能永远停在 `Committed`

严重度：高（活性）。

异步提交以 view 为 key，但 `mark_async_commit_execution_ready(block_hash)` 只更新第一个
匹配 hash。超时/重提案可让同一 execution block 出现在多个 view；一次执行完成后，后续
view 不会再收到第二次完成通知，因此确定性提交队列会永久被最早的未完成项挡住。

修复：一次 execution-valid completion 原子提升所有相同 block hash 的 `Committed`
生命周期，仍由 `drive_async_commits` 按 view 顺序逐一 Finalize。新增跨两个 view 重用相同
block 的回归测试。

### 3. Gov5 直推子流接受未绑定 PeerId

严重度：中高（资源 DoS；后续密码学校验仍保护一致性）。

`/rpc/block_push/1/ssz_snappy` 与 `/rpc/hotstuff_direct/1` 不经过 GossipSub mesh/scoring，
此前任何已连接 peer 都可进入区块缓存、消息解码及后续 BLS 验证路径。

修复：入站 Gov5 block push 与 HotStuff direct 仅接受以下任一来源：

- 运维显式 trusted peer；
- Noise PeerId 已绑定当前 validator index 的 routed peer；
- 已由有效单验证者签名晋升的 authenticated validator peer。

保留 routed-but-not-yet-authenticated 入口，使验证者的第一条有效签名消息仍能完成 BLS
认证映射，不形成启动死锁。

### 4. MsgID 在大消息上多做一次完整 payload 复制

严重度：性能。

Gov5 MsgID 的 Keccak 前像过去先拼成 `Vec(genesis || topic || data)`。对接近 16 MiB 的区块
gossip，每条消息会额外分配并复制完整 payload。现在改为增量 Keccak absorb 三个 slice，
不再构造连续前像；5 个 Gov5 原生测试向量保持完全一致。

## 交叉核验结论

- MsgID 的真实实现为 `Keccak256(genesis || topic || data)[:20]`。本机
  `../N42-gov5/internal/p2p/message_id.go`、`../n42-rs` 和两仓相同的 5 个向量一致；Gov5
  注释中写 SHA-256 是注释错误。先前“topic + genesis + data”的文字描述不准确，代码不应
  据此改序。
- 历史 PEVM、顺序历史回放和 live sequential EVM 的分类与调用点一致；PEVM 不会静默落入
  canonical Engine API，live payload build 也只允许 `LIVE_SEQUENTIAL`。
- Gov5 `bodies_by_range` 当前仅入站启用；请求的 count/step/span/overflow、每批 1024、单块
  64 MiB、canonical 连续性、状态认证、并发与令牌桶限制均存在。codec 的 outbound
  `read_response` 仍以 `Vec` 聚合整批，但生产 transport 未开放 outbound，暂不引入会破坏
  Gov5 理论上限的任意总字节阈值。

## 验证

- `cargo test -p n42-consensus --lib`：225 passed。
- `cargo test -p n42-consensus-service --lib`：211 passed，2 ignored（测量项）。
- `cargo test -p n42-network --lib`：192 passed。
- `cargo clippy -p n42-consensus -p n42-consensus-service -p n42-network --all-targets -- -D warnings`：通过。
