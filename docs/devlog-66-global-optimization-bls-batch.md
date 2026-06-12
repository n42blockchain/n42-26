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

## EVM 侧(25% 大头)profile

n42-evm-bench(samply/ETW 采样)结论见下节补充。reth 2.3 合并带来 upstream 的
**BAL 并行执行**(`bal_prewarm_pool`、parallel BAL executor)——EVM 侧的结构性提速来自
upstream 演进,n42 跟进配置即可。

## 工具

- `crates/n42-primitives/examples/bls_receipt_bench.rs`(三方案对比,BLS_BENCH_N 可调)。
- samply(ETW)+ `[profile.profiling]`(此前已加)。
