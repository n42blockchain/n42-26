# devlog-69 — 跨 crate 并发/正确性审计(consensus / twig / network)

承接 devlog-67/68:parallel-evm 的 Block-STM 并发正确性修完后,把同样的审计标尺
推到主执行路径之外的三块——HotStuff-2 状态机、twig 引擎边界、network 并发。结论:
**共识状态机与 twig 引擎均判定健全**(各加一条防御性断言/不改逻辑),network 修掉一处
真实的静默丢弃。

## 1. n42-consensus 状态机 —— 健全

逐条核对 HotStuff-2 的安全/活性不变量,无 bug:

- **纯事件驱动**:`ConsensusEngine::process_event`(`protocol/state_machine.rs:666`)是纯函数,
  引擎结构体单一所有权,不跨线程共享;唯一对外边界是单向 `mpsc::Sender<EngineOutput>`,
  `Arc<dyn VoteLogWriter>` 初始化后只读。无 `Arc<Mutex>`/`Arc<RwLock>`。
- **一票制(防双投)**:`last_voted_view` 落盘(`vote_log` fsync **先于** 签名广播,
  `proposal.rs:446-493`),崩溃恢复保留;`record_vote` 在签名前调用。
- **locked QC 单调**:`update_locked_qc` 只进不退(`round.rs:159`);`is_safe_to_vote`
  校验 `justify_qc.view >= locked_qc.view`。
- **2f+1 门限**:`quorum_size = 2f+1`(u64 算术防溢出);bitmap 长度==验证者集大小校验,
  防截断绕过;签名两次验证(纵深防御)。
- **域分隔**:Prepare QC 签 `view‖block_hash`(40B),Commit QC 签
  `"commit"‖view‖block_hash‖changes_hash`(78B),防跨域重放。
- **changes_hash 绑定**:proposal/commit-vote/commit-QC 三处都绑 validator_changes 哈希,
  防拜占庭 leader 聚合后替换验证者变更(`state_machine.rs` 对应测试已覆盖)。
- **Genesis QC 防回退** + **epoch 漂移区**用 bitmap 大小回退选历史集,均已处理。
- 热路径无 `unwrap/expect/TODO`;3 处 `debug_assert` 仅开发期。

**结论:无改动**。安全规则正确落实,落盘防双投跨崩溃有效。

## 2. n42-twig-core 边界 —— 健全(+1 防御断言)

证明索引数学逐位核对,`prove` 与 `verify` 对称:

- twig 路径按 `slot % TWIG_SIZE` 走 11 位(`TWIG_HEIGHT=11`,`2^11=2048`);
  upper 路径按 `slot / TWIG_SIZE`;shard 路径按 `shard_index`(4 位)。
- 单 twig 退化(`up_cap=1`)时 upper_path 为空,`root()` 直接返回 twig root,`verify` 一致。
- `ShardedTwigProof::verify_for_key` 绑定 key+shard(audit #11),拒绝"用 B 的合法证明
  应答 A 的查询"。
- 分片内单线程(`&mut self`),分片间各拥有自身数据,无共享可变 —— 无并发问题。

唯一边界 nit:`vlen: value.len() as u32` 对 ≥4GiB 值会静默截断(account 72B / storage 32B
永不触及,极低危)。加一条 `debug_assert!(value.len() <= u32::MAX)` 把不变量写进代码,release
零成本。21 个测试全过。

## 3. n42-network 并发 —— 1 处真实修复

整体健全:主事件循环 biased `select!` 防饥饿;`validator_peer_map`/`sessions`/
`session_senders` 三把 `RwLock` 均"读集合→释放锁→再发送",无锁跨 `.await`,无 AB-BA;
锁中毒 `unwrap_or_else(into_inner)` 优雅降级;所有 channel 有界(命令 8192 / 优先 2048 /
backlog 上限);receipt 微批 timer 用 `MissedTickBehavior::Delay`,无丢唤醒/饥饿。

**修复:`set_validator_context` 静默丢弃**(`service.rs:376`)。该 async 方法原用裸
`try_send` 并 `let _ =` 丢结果。它在**每次 epoch 切换**调用,驱动 Rotor relay 转发的
`my_index`/`validator_count`;命令通道(8192)瞬时打满时更新会被静默丢,relay 层就用陈旧
上下文转发且**无任何痕迹**。改为复用 `send_with_backpressure`(try_send 满则带超时 await),
失败时 `warn!` + `n42_network_set_validator_context_drops_total` 计数。两个调用方
(`orchestrator/mod.rs:742`、`consensus_loop.rs:260`)只 `.await` 忽略返回,签名保持 `()` 不变。

(另一处 flagged:phone 断连时 `sessions`/`session_senders` 两次 remove 非原子 —— 但二者之间
无 panic 源,正常 `break` 后必然双双执行,实际不可达,不改。)

## 验证

- `cargo clippy -p n42-network -p n42-twig-core --all-targets -- -D warnings`:干净。
- `cargo test -p n42-twig-core`:21 passed。
