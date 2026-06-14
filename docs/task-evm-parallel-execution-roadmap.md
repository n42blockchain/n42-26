# EVM 并行执行路线图:B(n42-parallel-evm 接 reth)+ A(Osaka/BAL)

## 0. 前置事实(本会话调查,2026-06)

| 事实 | 证据 | 含义 |
|------|------|------|
| n42 执行走 reth `new_payload`→engine-tree `payload_validator` | execution_bridge.rs | 在 reth 的执行/验证框架内 |
| n42 用 reth **默认 TreeConfig**(`builder.node(n42_node).launch()`,无自定义) | main.rs:713 | 默认优化全开 |
| **transaction prewarming 默认开**(`disable_prewarming=false`) | reth config.rs:221 | **state-access 投机预热已生效**(Cancun,无需 Osaka) |
| state cache + 并行 state root(deferred trie)默认开 | reth config.rs | 已生效 |
| BAL 并行执行只在块带 BAL 时触发,BAL=EIP-7928=Osaka | payload_validator.rs | **n42 在 Cancun → 永不触发** |
| leader 执行已缓存(N42_PAYLOAD_CACHE,209→22ms) | payload/lib.rs | leader 不重执行 |

**关键结论**:reth 2.3 的 state-access 优化(prewarm/cache/并行 state root)**对 n42 已全部生效**。
剩余可榨的是 **tx 执行本身的并行**(prewarm 是预热 cache,执行仍顺序)。这正是 B 的目标。
A(Osaka/BAL)是在 prewarm 之上再叠并行执行,边际收益,但代价是整个硬分叉升级。

---

## B. 把 n42-parallel-evm 接进 reth 的 block executor

### 目标
follower eager-import 重块(合约多)的 tx 执行从顺序变 Block-STM 并行(已 deferred coinbase
2257x + 并行 validation + jemalloc 下 scale)。leader 走缓存不受影响。

### 难点
reth 的 `BlockExecutor` 不止跑 tx —— 还有 system calls(beacon root/EIP-2935)、withdrawals、
EIP-7685 requests、receipts root、gas accounting。n42-parallel-evm 只跑 tx loop。直接替换=重写
block 级逻辑,高风险(共识正确性)。

### 方案(分阶段,低风险优先)
1. **B0 — 影子验证(无共识风险)**:在 follower import 后,异步用 n42-parallel-evm 重跑同一块,
   对拍 reth 顺序执行的 state root + receipts。CI/metrics 收集"并行结果==顺序结果"证据 +
   实测重块加速比。**不进共识路径,纯观测**。先做这个建立信心 + 真实重块数据。
2. **B1 — 自定义 EvmConfig 的 tx-loop 并行**:实现 n42 的 `ConfigureEvm`/`BlockExecutor` 包装
   reth 的 `EthEvmConfig`,只把内部 `execute_transactions` 换成 parallel_execute,其余(system
   call/withdrawal/requests/receipts)仍委托 reth。需研究 reth 2.3 BlockExecutor trait 的
   execute_transactions 是否可单独覆写。
3. **B2 — 接入 + gating**:`N42_PARALLEL_EVM_EXEC=1` env 开关(默认关),只对 tx 数 ≥ 阈值 +
   合约占比高的块走并行。leader 缓存路径不变。
4. **B3 — E2E**:多节点 fresh-genesis,合约重 workload,验证 root 跨节点一致 + 加速比 +
   崩溃恢复。**需 testnet(Linux/jemalloc)** → 交接或 WSL。

### 验收
parallel 执行的 state root/receipts 与 reth 顺序逐字段一致(B0 对拍);加速仅在合约重块体现;
共识正确性零回归;env 默认关,稳定回退。

---

## A. 激活 Osaka 硬分叉(解锁 BAL 并行执行)

### 范围(consensus-breaking,大工程)
1. **chainspec**:`.cancun_activated()` → 加 Prague + Osaka 激活(整套 ruleset,非仅 BAL:
   EIP-7702/7623/2537/... + 7928 BAL)。
2. **payload builder**:Osaka 强制产 BAL(`MissingBlockAccessList`),n42 payload 路径要产出
   + RLP 编码 BlockAccessList。
3. **共识/手机/twig 重验**:HotStuff 区块格式、手机验证、twig 状态树在 Osaka ruleset 下全验。
4. **fresh genesis 或全网协调 fork**:Osaka 改区块有效性,旧链不兼容。
5. **E2E**:多节点 Osaka 出块,BAL 并行执行触发(`decoded_bal.is_some()`),验证一致性。

### 评估
- **收益**:BAL 并行执行 + BAL-derived 并行 state root,但**叠加在已生效的 prewarm 之上**,
  边际(prewarm 已预热 cache,执行延迟大头已缓解)。
- **代价**:整套 Osaka ruleset 的兼容性风险(对 n42 定制共识 + 手机验证 + EIP-4895 奖励等)、
  consensus-breaking fork、大量 E2E。
- **建议**:**A 的主要驱动应是"n42 要不要跟进 Osaka 协议"这个产品决策,而非"为了 BAL 并行执行"**。
  仅为并行执行,B 性价比远高于 A。若产品上要 Osaka,则 BAL 并行是顺带红利,届时 reth 已支持
  (默认开),只需 chainspec + payload builder 适配。

### 顺序建议
**先 B(B0 影子验证起步,无风险拿数据),A 作为独立产品级硬分叉升级单独立项。** 二者不冲突:
B 在 Cancun 就能做;A 落地后 B 的并行执行与 BAL 并行可二选一或互补。

---

## 与 codex 协调

codex 正在做 P6 step3(orchestrator/main.rs/RPC 节点接线)。B1/B2 会动 n42-execution +
可能 n42-node 的 EvmConfig 装配 → **需与 codex 避让**(它动 orchestrator/RPC,我动 EvmConfig/
executor)。B0 只在 n42-execution + 观测,冲突面小,适合先做。
