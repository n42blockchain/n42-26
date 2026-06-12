# devlog-66 — 全局优化诊断:BLS 收据验证是最大可收割杠杆(24x)

## 背景:从引擎视角换到全系统视角

twig 引擎已榨到位(build 4.4M accts/s / 更新 1.8M upd/s / 节点路径 2.27M ops/s),但放回
8s slot 预算(memory: Firedancer 评估,2026-03-28)它只占 ~6%。全局账本:

| 组件 | 耗时 | 占比 | 现状 |
|------|------|------|------|
| EVM 执行 | 2000 ms | 25% | 本轮 profile(见下) |
| **BLS 验证 10K 手机收据** | 687 ms(8 线程) | 8.6% | **本轮发现 24x 空间** |
| 状态根 | 500 ms → 数十 ms | ~1% | twig 已解决 |
| 其他(channel/memcpy/网络) | <1 ms | <0.01% | 无关 |

## 发现:收据逐签验证,而同块收据签的是同一条消息

- `star_hub.rs` 每条收据一个 `spawn_blocking` 逐签 `verify_signature()`(~0.5ms/签,
  注释自述 1.5ms)。10K 手机 = 10K 次独立 pairing。
- 但 `Receipt::signing_message() = (block_hash, block_number, computed_receipts_root)` ——
  **同一块的所有诚实手机签同一消息**,聚合/批量验证直接适用。
- 而且 `n42-primitives::bls` 里 **`batch_verify`(随机系数)和 `AggregateSignature` 早就存在**
  (共识 QC 在用),收据路径没接。

## 实测(bls_receipt_bench,N=1000 同消息,1 核,9950X)

| 方案 | 时间 | 外推 10K 手机 | 安全性 |
|------|------|--------------|--------|
| 逐签 verify(当前) | 482 ms | **4.8 s** | ✓ |
| same-msg 聚合(点加+1 pairing) | 29.6 ms(16x) | 296 ms | 需 PoP 防 rogue-key |
| **batch_verify(随机系数)** | **20.2 ms(24x)** | **~200 ms** | **rogue-key 安全,免 PoP** |

`batch_verify` 又快又不需要改信任模型(随机线性组合天然防 rogue-key),是收据路径的正确选择。

## 行动项(待实施):star_hub 收据微批验证

把逐条 `spawn_blocking verify` 改为**全局微批**:
1. 各 session 把 `(receipt, session 回执通道)` 投递到一个批处理 task(mpsc);
2. 批处理按 `N≥256 或 50–100ms` 触发 `batch_verify_with_fallback`(已存在!失败时分治
   定位坏签名 → 只断坏 session);
3. 验证通过的批量转发 `HubEvent::ReceiptReceived`。

预期:10K 收据验证 4.8s(累计单核)→ ~200ms 单核,**24x,释放 ~8% slot 预算 + ~7 个核给 EVM**。
延迟代价:每收据 +50–100ms 确认延迟(收据是异步证明流,不在共识关键路径,可接受)。

注意:与 codex 的 P6(node 区域)避让 —— star_hub 在 n42-network,不冲突,但实施排在
P6 节点接线落定之后或协调分工。

## EVM 侧(25% 大头)profile 结论

n42-evm-bench(samply/ETW,17 万样本)实测:
- **裸 EVM 解释器极快**:100K tx(90% transfer + 10% ERC-20)= 81ms,**1.23M tx/s 单核**,
  仅占 4s slot 的 **2%**。理论单核 TPS 1.4-2.4M。
- 所以"EVM 执行 2000ms(25%)"的大头**不是解释器**,而是真实节点的 **state/provider 层**
  (SLOAD→trie/mdbx/缓存 miss;bench 用 CacheDB 内存态故无此项)。
- profile 另见 35.5% 线程等待(NtWaitForAlertByThreadId)= Block-STM 并行段同步空转,
  parallel-evm 的调度仍有余地。
- **结构性方向(更正)**:reth 2.3 带来 BAL 并行执行(`bal_prewarm_pool`、parallel BAL
  executor),但 **`decoded_bal` 仅在块带 BAL 时非空,而 BAL=EIP-7928 是 Osaka 特性**。
  **n42 只激活到 Cancun**(chainspec `.cancun_activated()`)→ BAL 并行执行**永不触发**。
  启用它=激活 Osaka 硬分叉(整套 ruleset + payload builder 产 BAL + consensus-breaking +
  全网 fork),**是大工程不是配置**。原"跟进配置即可"判断错误,已更正。详见下节。

## 全局优化排序(本轮诊断后)

1. **BLS 收据 batch_verify(24x,~200ms@1 核)** —— 最大可收割,行动项已列。
2. EVM state 层:reth 2.3 BAL 并行执行需 **Osaka 硬分叉**(n42 在 Cancun,永不触发)——
   非配置,是 consensus-breaking 大工程,需单独立项。替代:把已优化的 n42-parallel-evm
   (deferred coinbase)接进 reth 执行路径(也是大集成)。
3. parallel-evm 同步空转(35% wait)—— Block-STM 调度精调,中等。
4. twig/状态根:已完成(~6%→~1%)。
5. 网络/序列化:实测 17-29 Gbps 分发、<2ms,无关紧要。

## 工具

- `crates/n42-primitives/examples/bls_receipt_bench.rs`(三方案对比,BLS_BENCH_N 可调)。
- samply(ETW)+ `[profile.profiling]`(此前已加)。

## 更正:BAL 并行执行需要 Osaka(非配置开关)

调查(2026-06 本会话):
- n42 执行走 reth `new_payload` → engine-tree `payload_validator`,BAL 并行执行就在这条路上,
  且 reth 默认 `disable_bal_parallel_execution=false`(已启用)。
- 但触发条件是 `env.decoded_bal.is_some()`;`decoded_bal = payload.block_access_list()`,
  BAL=EIP-7928 是 **Osaka** 特性,payload 不带 BAL 就是 None。
- **n42-chainspec 只 `.cancun_activated()`**(无 Prague/Osaka)→ 块永不带 BAL → 并行 BAL 死路。

**启用 = 激活 Osaka 硬分叉**:整套 Osaka ruleset + payload builder 强制产 BAL
(`MissingBlockAccessList`)+ consensus-breaking(fresh genesis / 全网协调 fork)+ 在 Osaka 下
重验 HotStuff 共识 + 手机验证 + twig 状态树。**这是独立大工程,需立项规划,不在"快速优化"范围。**

替代方向(都需集成工作,非配置):
- 把 n42-parallel-evm(已 deferred coinbase + 并行 validation,Linux/jemalloc 下 scale)接进
  reth 的 block executor(自定义 EvmConfig/Executor 委托),替代 reth 顺序 executor。但现状
  `execute_block_parallel` 是 standalone,且 leader 已有 N42_PAYLOAD_CACHE 跳过重执行,主要
  受益的是 follower eager import 的重块。
- n42 真实 EVM 瓶颈缓解其实已大部分到位:leader 执行缓存(N42_PAYLOAD_CACHE,209ms→22ms)。
