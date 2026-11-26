# devlog-192：同步标准 v1.1 的 bodies_by_range/2 与 commit_qc/1（2026-11-26）

分支 `feat/profile1-sync-v11-range`。规范：N42-SYNC-SPEC.md §5.0、§5.1、§5.2、§6；字节参照为 gov5
`internal/testdata/sync-v11` 的黄金向量（复制到 `crates/n42-network/tests/fixtures/sync-v11/`，sha256 见同目录 README）。

## 设计决策

- 新模块 `crates/n42-network/src/sync_v11.rs`：两个协议的线格式、服务端、客户端检查、codec 都在这里；
  `gov5_rpc.rs` 只把成帧辅助函数改为 `pub(crate)`，并给 `Gov5CanonicalBlockReader` 加可选的按高度读头部
  （`with_header_reader`），HEADERS_ONLY 与 commit_qc 不必读整块。
- `/2` 与 `/1` 在 TCP 互通 swarm 上并列注册（`transport.rs`）。`/2` 服务端沿用 `/1` 的访问控制、并发（v1+v2 合计
  每对端 2、总数 8）与按块配额；超额回 `0x04`（消息 `"1000"`）。按字节预算逐块判定，首块总是发送，结尾恰好一个 END。
- 已提交头 = reader 的 `best_block_number()`：n42 的 canonical head 只随提交的 forkchoice update 前进。
  低于最早块体或中途缺块：END reason 4；读失败或解码失败发生在首块之前时回 `0x02`，之后则 END reason 4。
- 客户端选择在网络层做，追块状态机不变：对端 Identify 列出 `/2` 且一小时内未被标成只会 `/1` 时发 `/2`，
  否则发 `/1`；`/2` 协商失败（UnsupportedProtocols）则标记该对端 1 小时只用 `/1` 并立刻以 `/1` 重发同一请求。
  `check_range_v2_reply` 检查 next、served_bytes、reason、块数与 context 字节；不一致是可证明故障
  （`Gov5RangeOutcome::Faulty` → `RangeReply::Fault`，计分并重新排队）；缺 END 是软故障（退避、不计分）。
- commit_qc：服务端持有的 QC = 已提交链头部 `Extra` 里嵌入的 QC + 编排器在每次提交时交给网络层的已提交 QC
  （`note_committed_qc`，gov5 QC wire）。返回能覆盖请求高度的最低 QC 及其块到请求高度的头部（≤256），否则返回
  最高 QC 不带头部；`(0,0)` 返回最高 QC 及其块头。客户端 `check_commit_qc_reply` 检查哈希链，
  `range_catchup::verify_commit_qc_reply` 再按 §3.3 严格验 QC（commit 域、QC 自身 epoch 的集合、无回退）。
  编排器收到已验证的回复时，若 range 追块在运行就作为严格锚点注入；目前没有自动发起 commit_qc 请求的路径。

## "QC wire"

即 gov5 `encodeQC` 形式，n42-26 的 `h2_wire::encode_h2_qc`/`decode_h2_qc`：
`view u64 ‖ u32 32 ‖ hash ‖ u32 len ‖ aggSig(96) ‖ u32 len ‖ bitmap(u16 n ‖ ⌈n/8⌉)`。向量里的 QC（view 9，块 9）
就是块 10 头部 `Extra` 中嵌入的那份；它在 gov5 夹具的确定性验证者
（`sha256("n42-profile1-test-validator-<i>")`，首字节清零）下通过 H2-v4 commit 域聚合签名验证。

## 测试

- `n42-network` `sync_v11::tests`：两份向量的每个用例，服务端在由向量块重建的链（块体 7..10、头部 6..10、
  已提交头 10）上重放，比较结果码、context 字节、payload 与 END；客户端读取向量 response_wire 并判定；
  请求编码逐字节等于 `request_payload`。另有 END 不一致、违规线格式、长度先检后分配等负例。
- `n42-consensus-service` `range_catchup::tests`：向量 QC 的 BLS 验证与伪造签名的故障判定；`/2` 故障回复计分。

## 后续

- （已由下节"评审修正"补上自动 commit_qc 与按 Status 开放 `/2`）`/2` 的多批预取流水线（目前逐批串行读）、按 Status 对公众对端开放
  range 服务（W4）均未做。

## 评审修正（同日）

独立评审提出六项，均已处理（仍未提交）。

### 1. 服务端的头必须是已提交高度（HIGH）

问题：leader 的构建 FCU 以 LockedQC（只有 prepare 证书、未提交）块为父，ahead-build 还会继续在其上构建，
reth 的 `best_block_number()` 因此可能超过已提交块；`/1`、`/2`、commit_qc 以它为头，就会把未提交块当作已提交下发。

方案：`Gov5CanonicalBlockReader` 增加已提交尖端（`gov5_rpc.rs`）。
- `with_committed_resolver(hash -> Option<number>)`：节点层提供"该 hash 是否 canonical、在哪个高度"。
- `note_committed(hash)`：各克隆共享一个 `Mutex<Gov5CommittedTip { latest, confirmed }>`。
- `committed_head()`：最新报告的已提交块一旦在存储中 canonical 即确认，否则用上一个已确认块（并复核仍 canonical）；
  只升不降，绝不超前于已提交块；未装 resolver（observer，其 forkchoice 只跟随已验证的 Decide）时退回 storage head。
- 推进点：`consensus_loop::handle_block_committed` 已有的 `note_committed_qc`（网络层解出 QC 的 block_hash
  再 `note_committed`）；range 追块导入严格锚点以下的块后 `note_committed_block`（新 `NetworkCommand::NoteCommittedBlock`）。
- 启动：`main.rs` 新增 `gov5_committed_block_reader`，H2-v4 参与者用持久化快照的 `last_committed_qc.block_hash`
  初始化（零 hash 或无快照时用创世块），不取 reth 头。
- 使用点：`/2` 的 `range_v2_response`、commit_qc 的 `serve_commit_qc`、`/1` 的未来高度检查与流式写出
  （`end = min(请求末尾, 已提交头)`；起点高于已提交头仍按旧行为返回空流）。
- 曾考虑共享 `AtomicU64` 高度：但高度本身无法区分"已提交且已 canonical"与"已提交但 reth 还没跟上"，
  且重组后会过时；按 hash 解析再确认更稳妥。

### 2. `/2` 门槛（MEDIUM）
`/2` 改为对 `gov5_chain_authenticated_peers`（完成匹配 Status 的对端，含非信任对端）开放，配额与并发上限不变；`/1` 门槛不变。

### 3. 超时对齐（MED-LOW）
`/2` 传输超时 60 s → 30 s（与 `/1` 相同）。追块请求超时 35 s 后会重新分配区间并退避对端，传输层晚于此只会把迟到的
诚实回复变成被丢弃的回复；30 s 先结束流。64 MiB 默认预算下相当于 2.1 MiB/s 的下限。未选择把两者都提到 60 s，
因为那会让一个慢对端占住区间更久，拖慢整个追块。

### 4. 零块 END 与 RATE_LIMITED 不退避、不计分（MED-LOW）
新 `RangeV2Verdict::Unavailable { reason, retry_after }` → `Gov5RangeOutcome::Unavailable { canonical_end, retry_after }`
→ `RangeReply::Unavailable`：区间重新入队，`failures` 不增长、不计分；暂停固定为服务端的 `retry_after_ms`
（RATE_LIMITED 错误块的消息解析为毫秒，缺省 1 s，上限 60 s），否则用 2 s 基础值（防止单对端被立即重复请求，但不升级）。
CANONICAL_END 还把该对端的高度提示降到 `start-1`，使选对端时优先其他对端。`/1` 不变。

### 5. 客户端读上限（LOW）
客户端总是发 `max_bytes=0`（64 MiB），读上限由 128 MiB 改为 64 MiB；多于一个块且总字节超过请求预算判为故障，
只有单个首块可以独自超出（它本身也受 64 MiB 读上限约束）。

### 6. commit_qc 客户端调用
已接入，改动小：`RangeCatchup::tail_anchor_request` 在 `head+1` 已缓冲但未锚定时（§3.4 第 6 步）返回
`(peer, head+1, hash)`，优先选提供该块的对端，每 5 s 至多一次；`drive_range_catchup` 调用
`request_gov5_commit_qc`。网络层按 Identify 记录支持 `/rpc/commit_qc/1` 的对端（`gov5_commit_qc_peers`），
不支持的立即以 `unsupported` 失败，编排器据此 `note_commit_qc_unsupported`，本轮不再向它请求。
回复仍经 `verify_commit_qc_reply` 严格验证后作为锚点注入。

### 测试
- `sync_v11::tests`：`served_head_follows_the_committed_tip_not_the_storage_head`（storage 头 10、已提交 9：
  `/2` 只给 7..9 且 END=CANONICAL_END，块 10 的 commit_qc 返回 0x03，较低/未知 hash 不使头回退）、
  `bodies_by_range_2_client_does_not_penalise_rate_limiting`、`bodies_by_range_2_client_faults_a_reply_over_its_budget`；
  向量回复中零块的 canonical_end/not_available/empty_range 现判为 `Unavailable`。
- `range_catchup::tests`：`an_empty_v2_reply_is_neither_scored_nor_escalated`、`an_unanchored_tail_asks_for_a_commit_qc`。
- clippy `-D warnings` 通过；n42-network 264、n42-consensus-service 362、n42-node 314 及全部集成测试通过。

### 仍未做
- Status 的 `current_height` 仍取 storage head（只是选对端的提示），如需一致可改为 `committed_head`。
- `/2` 多批预取流水线、Profile-1 follower 的已提交推进依赖其 `BlockCommitted` 输出（未在真实节点上验证）。

## 混合实链停滞（v3 清库重启，2026-11-26 09:13Z）

### 现象
v3 空库重启，链头约 59，range 追块在 09:13:28 启动（head=0, target=59），1..57 一秒内导入，之后停在 57 约 6 分钟，
直到 09:19:17 才导入 58（`received=58`），随后反向遍历放出 59..81。期间既无 "range catch-up stopped"，也无停滞超时。

### 根因
1. **定时器饿死（主因）**：`RangeCatchup::next_deadline(now)` 在无在途请求、无 kick 时返回 `now + 1s`，而编排器 select
   循环每处理一个事件就重新计算这个 deadline（`orchestrator/mod.rs` run 循环 `range_catchup_timer`）。实链每秒约 10 条
   交易 gossip，`now + 1s` 永远在下一事件之后，poll 永远不跑。poll 是唯一做重试、`CaughtUp` 判定（head ≥ goal−2 连续三次）
   和停滞超时的地方，所以三者全部失效。日志佐证：09:19:16.5 交易流停止，0.9 s 后 58 即被取到。
2. 触发条件：服务器只服务已提交块，其已提交头（57）比本地暂存块（59，反向遍历拿到）低 2；58 请求得到"空"答复后该对端被暂停，
   客户端里没有别的可用对端（n42-rs 从未被请求，即不在 range 对端表中），于是无在途请求、无 kick，进入上面的饿死状态。
   若 poll 正常运行，3 次 poll（约 3 s）就会以 CaughtUp 交给反向遍历按 hash 取 58。
3. 停滞超时还有第二个问题：进展判定计入 `anchored_total`，而 live 提交在缺口之上不断加锚点，即使 poll 运行也永不判停滞。
4. `tail_anchor_request` 只在 head+1 已缓冲时触发；本例 58 根本没有块体，所以从未发 commit_qc（这不是本例的瓶颈，缺的是块体）。

### 修复
- `next_deadline` 以上次 poll 为基准（`last_poll + 1s`），不随事件滑动；激活时清零。
- 进展 = head 前进或收到块体（`stats.blocks_received`），不再计锚点；`stall_timeout` 90 s → 45 s（大于 35 s 请求超时）。
- `/1` 的干净空答复与 `/2` CANONICAL_END 同样处理：固定 2 s 暂停、高度提示降到 `start−1`，不指数退避。
- `tail_anchor_request`：已缓冲但只有松散锚点（enforce 下不可执行）时也发 commit_qc。
- info 日志：`range request sent`（peer、`/1` 或 `/2`、start、count）、`range reply`（块数、END next/reason、
  nothing served / refused / failed / inconsistent）、`commit_qc request sent` / `commit_qc reply|refused|request failed`。

### 测试
- `range_catchup::tests::a_tail_beyond_the_servers_committed_head_is_handed_off_under_steady_traffic`：
  按 select 循环的方式（每 100 ms 一个无关事件、每次重算 deadline、答复立即驱动）跑实链场景：空库 head 0、target 59、
  唯一服务器已提交头 57、暂存 59。修复后 ≤5 s 以 CaughtUp 交给反向遍历；在原代码上 600 s 模拟时间内不停（已验证复现）。
- `range_catchup::tests::a_trailing_server_is_asked_again_until_a_later_qc_anchors_the_tail`：暂存块更高（63），服务器已提交头
  59、8 s 后升到 63，9 s 时一个后续 Decide 锚定 63；修复后尾部被取到并放行、CaughtUp，无计分；原代码同样不停。
- `orchestrator::tests::a_fresh_node_whose_peers_trail_the_target_by_two_reaches_the_live_head`：编排器端到端，三个服务器都落后
  2 块，客户端停止后反向遍历按 hash 取缺块，节点到达 live 头（50），每个高度导入一次。
