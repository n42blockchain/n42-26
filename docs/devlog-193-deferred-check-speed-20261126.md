# devlog-193：延迟预检查提速（2026-11-26）

分支 `feat/profile1-deferred-check-speed`，基于 origin/main `d7aec8b`。

目标：follower 的 Round-1 预检查（`DeferredChecker::check`）要在本 view 的 Decide 到达之前完成，这样导入之前就能投出票。

## 1. 测量

### 线上（S1，validator 3 的 debug 日志 `~/.cache/reth/logs/1144`，只读）

每块约 1200 笔交易，共 221 个 follower view，时间从 payload 解码完成（检查开始的时刻）算起：

| 事件 | 距解码完成（中位数） | 说明 |
|---|---|---|
| Decide 到达 | 6.9 ms | 解码本身在提案后 6.5 ms |
| 预检查票发出 | 约 20 ms | 范围 10–41 ms，只有 86 个 view 发出 |
| 导入完成 | 25.6 ms | |

检查只比导入早 4–18 ms，从未赶在 Decide 之前。

### 单机 bench（`crates/n42-node/tests/deferred_check_speed.rs`，release，`--ignored`）

条件：1200 笔 EIP-1559 转账、1200 个发送者，reth 测试 provider（mdbx）上的创世状态，254 核，每项 200 轮取中位数。

| 场景 | 总耗时 | 解码 + 发送者 | plan | 状态读 |
|---|---|---|---|---|
| 无缓存，安静 | 3.9 ms | 2.5 | 0.45 | 0.75 |
| 缓存已由入口填好，安静 | 1.7 ms | 0.9 | 0.16 | 0.6 |
| 无缓存，旁边有导入负载 | 3.1 ms | 1.6 | 0.4 | 0.95 |
| 缓存已填好，旁边有导入负载 | 3.0 ms | 1.5 | 0.3 | 1.0 |

- 「导入负载」是另一个线程在全局 rayon 池上反复恢复另一个块的 1200 个签名。
- 旧代码的发送者阶段（两遍 par_iter，先解码再恢复）为 1.2 ms（安静）/ 2.1 ms（有负载）。
- 头查找与 carried 比较、打开父状态，两项都低于 0.01 ms。

拆分（同一台机器）：

| 操作 | 耗时 |
|---|---|
| 解码 1200 笔，串行 | 0.21 ms |
| 解码 1200 笔，par_iter | 0.45 ms（rayon 唤醒开销大于收益） |
| 计算 tx hash，串行 | 约 0.45 ms |
| ECDSA 恢复，串行 | 36.7 ms（约 30 µs/笔） |
| ECDSA 恢复，254 线程并行 | 0.8 ms |
| 缓存命中 1200 次 | 0.008 ms |

### 结论

- 检查本身的计算在安静的 254 核机器上只要 2–4 ms，线上却要约 20 ms。
- 差距不在 `check_inner` 的计算量，而在环境：同机 4 个节点加压测器；导入同时在全局 rayon 池上恢复同一批 1200 个签名；导入本身 25 ms 的 CPU 也在争用。
- 旧检查有两遍 par_iter，在线上要排在导入的 rayon 任务后面等，而且它恢复的正是导入也要恢复的签名。
- 现有日志看不到 spawn_blocking 排队、检查内部各阶段和结果回到编排器的时间。这次把这些都补成了指标和日志。

## 2. 改动

### 交易入口把发送者写进 reth 的共享缓存

文件 `crates/n42-node/src/tx_bridge.rs`。

- reth 的 `SenderRecoveryCache` 原本只由 reth 自己的 p2p/RPC 入口和 engine 的 payload 转换写入。gov5 gossip 进来的交易走 `TxPoolBridge`，用的是 `try_into_recovered`，不进缓存。所以导入和检查各自都要把整块重新恢复一遍。
- 现在 secp256k1 交易改为 `cache.recover(&tx)`，规则与 `try_into_recovered` 相同，都是 `recover_signer`、要求低 s。失败不进缓存，被拒绝的结果也不变。
- 0x50 交易保持原路径。
- `bin/n42-node/src/main.rs` 把 `evm_config.inner().sender_recovery_cache` 交给 bridge。
- 附带效果：engine 导入在 payload 转换时也会命中这个缓存，省掉每块约 1200 次 ECDSA，导入时的 CPU 争用随之下降。

### 预检查先读缓存，只恢复未命中的部分

文件 `crates/n42-node/src/el/deferred.rs`。

- `DeferredChecker::with_sender_recovery_cache`：main.rs 传入同一个缓存。
- 在本线程按顺序解码全部交易。最低序号的解码失败仍然先于任何发送者错误被拒，拒绝文案不变。
- 再逐笔判断：
  - chain id 不符：与原来同一句拒绝；
  - 否则查缓存：secp256k1 查 reth 缓存，0x50 查 `AltSigSenderCache`。
- 未命中的交易：
  - 不超过 `SERIAL_RECOVERY_MAX` = 32 笔时，在本线程串行恢复，例如缓存槽冲突造成的少量未命中；
  - 超过时才放到 rayon 并行恢复。
  - 恢复走 `cache.recover` 并写回缓存，导入随后可以直接命中。
- 交易都经过 gossip 入口时，检查完全不碰 rayon 池，不会再排在导入后面。
- 语义：
  - 缓存里只有从同 hash 交易恢复（即已验签）出来的发送者，失败从不缓存，所以命中时得到的发送者与 `recover_signer` 的结果相同；
  - 其余规则（plan、nonce/余额/代码）一字未改；
  - 不会出现一张旧检查不会发的票。

### 可观测性

| 名称 | 类型 | 内容 |
|---|---|---|
| `DeferredChecker::check_timed` / `DeferredCheckTimings` | API | 各阶段耗时：carried、senders、plan、open、state，以及缓存命中数 |
| `n42_deferred_check_stage_ms{stage}` | 直方图 | 上述各阶段，另含 `queue`（在 `reth_engine.rs` 里等 spawn_blocking 线程的时间） |
| `n42_deferred_check_senders_total{source=cache\|recovered}` | 计数器 | 发送者来自缓存还是现场恢复 |
| `deferred check stages` | 日志（info，target `n42::profile1`） | 每块一行；配合已有的 `deferred check finished`（含 elapsed 和距收到块数据的时间） |

用这些可以在下一轮 S1 直接拆出线上剩余的耗时。

### 考虑过但没做

- **专用线程池**：`spawn_blocking` 的线程池 keep-alive 是 10 s，而 slot 是 8 s，正常不会排队。先用 `queue` 指标确认，有排队再换成专用线程。
- **状态读并行化**：1200 个账户在 mdbx 上读约 0.7–1.0 ms，等线上 `state` 指标出来再定。

## 3. 测试

- 新增 `deferred_check_speed::sender_cache_gives_the_same_verdicts`，用 64 笔交易、真实 provider：
  - 无缓存、首次经缓存（全部未命中）、再次经缓存（命中）三种情况都返回 Checked；
  - 四类拒绝在有缓存和无缓存时字字相同，第二遍带着第一遍留下的缓存也相同：nonce 错、他链 chain id、签名无法恢复（s 超过曲线阶）、"签名坏在 tx 1、解码坏在 tx 20"（报告 tx 20 的解码错误）；
  - 缓存是有损的（槽冲突会驱逐），断言只要求命中的发送者一定正确。
- 新增 `tx_bridge::tests::network_transactions_file_their_senders`：入口恢复结果与 `try_into_recovered` 相同并写入缓存；坏签名被拒且不写缓存。
- bench `deferred_check_on_a_1200_transaction_block`（ignored）。
- 现有 `profile1_deferred`（V5 链 + 每个错误字段 + 20 块 range）通过。
- clippy `-D warnings`（n42-network、n42-consensus-service、n42-node、n42-node-bin，all-targets）通过。
- 测试全部通过：

  | 测试目标 | 通过 |
  |---|---|
  | n42-consensus-service lib | 365 |
  | profile1_v6 | 19 |
  | n42-network lib | 264 |
  | n42-node lib | 315 |
  | n42-node 集成测试 | 全部 |

## 4. 后续

- 用这个分支的二进制跑一轮 S1 或 4b。看 `n42_deferred_check_senders_total{source}` 的命中率是否接近 100%。看 `deferred check stages` 加上 `deferred check finished` 后剩余的时间在哪：queue、state，还是结果回到编排器的通道。
- 若 Decide 仍然先到：
  - 下一步考虑把检查从「payload 解码之后」提前到「区块数据到达」时就开始；
  - 或者在 leader 广播时附带发送者列表。不过这需要协议改动，暂不做。
