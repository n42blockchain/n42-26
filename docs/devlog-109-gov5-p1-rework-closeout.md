# Devlog 109: gov5 2026H1 P1 六项返工收尾

日期：2026-07-19  
任务书：`docs/codex-task-sync-from-gov5-2026H1.md` P1-1 ～ P1-6  
基线：`025d12a`（`validate/gov5-sync-p0`）

## 结果

六项 P1 均已按任务书重新审计、实现和验收。返工没有把乐观 R1 改成 order-then-execute，
也没有回滚 CommitQC。执行路径继续以 reth 的 `Valid` 为本地 head 前进底线；默认 follower
仍使用 leader 广播的 cached execution output 快速导入，并按序更新 QMDB/Twig 二叉树。
六个 follower 独立重放完整交易、重新计算二叉树承诺属于可选的 full-replay 诊断模式，
不是 90K cache-hit 吞吐基线。

当前 LtHash、proposal/header 中的 QMDB root + LtHash commitment 以及显式
`cache-lthash`/`full-replay` 开关仍是后续任务，状态以
`docs/follower-validation-modes.md` 为准；本次不把未完成能力写成已上线。

## 六项交付

| 项 | 结果 | 主提交 |
|---|---|---|
| P1-1 批量 BLS | consensus R1/R2 随机系数批量验签、坏签名逐个回退；R2 token 绑定准确 validator-change domain | `615f251`, `b8e055b` |
| P1-2 Twig staged flush | WAL fsync 前不 adopt 内存；写失败后 root/version 不前进、sink poison、重启回最后 durable 点 | `0ff6f20` |
| P1-3 state-root 硬底线 | 生产 bypass 启动硬拒绝；确定性账户/slot 探针对账，发散锁存 unhealthy 并停发手机包 | `27e7565` |
| P1-4 网络活性 | trusted validator 重连、身份 source binding、投票重发、future-timeout TC、sync 限流、epoch/restart/catch-up/执行谱系恢复 | `1e8ccb0`, `6528f60`, `ceeabf6`, `95bba66` |
| P1-5 手机栈评估 | dense/delta、跨 IDC cohort、注册表锚逐项量化；明确复用 QMDB/Twig 二叉承诺，不采用 MPT | `67d6ca5` |
| P1-6 SELFDESTRUCT | 真实 EVM 覆盖 create/destroy/recreate、未读 slot、Shanghai/Cancun EIP-6780；IDC/mobile receipts root 一致 | `434d457` |

## 审计附带修复

返工门禁没有忽略超出原六项但会破坏正确性的发现：

- Block-STM validation 领取顺序早于完成顺序，可能在低位交易尚未完成时验证高位交易：
  `71bf98e` 改为完成前缀门。
- 稀疏高 MobileIndex 在 evidence round-trip 中丢位：`3b0d4f1` 增加 v2 显式 bit length、
  v1 兼容读取与非规范输入拒绝。
- parallel-EVM 从未真正产生/传播整地址 storage wipe：`6074c71` 补齐 Destroyed/Recreated、
  MVCC wipe 与 deferred-beneficiary 顺序回退。
- benchmark 用 `2f+1` 而协议已统一为 `n-f`：`5eace39` 改为调用唯一权威 quorum 公式。

## 关键返工发现：CommitQC 子块可能携带无 CommitQC 祖先

Scenario 9 的失败现场不是随机网络抖动。node-2 停机前，block 144 已形成 PrepareQC 并被
其余节点锁定，但尚未形成 CommitQC；恢复后的 block 145 沿 block 144 构建并获得 CommitQC。
node-2 的 reth durable head 仍为 143，而旧 state sync ring 只保存有独立 CommitQC 的块，
因此只返回 145。reth 正确地持续返回 `Syncing`，因为 parent 144 不存在。

修复后的 `/n42/sync/2`：

1. live 节点在清理普通 pending cache 后仍短期保留 raw proposal 数据；
2. CommitQC 到达时从子块反向走到 exact execution-validated head，只在完整、连续、无环时保存
   “prepared ancestors + committed child”；
3. sync 接收端先验证 CommitQC，再验证 raw envelope/hash/view/parent/block-number 链确实到达
   自己的 exact head；
4. 按 execution 顺序导入每个 prepared ancestor，再走普通 CommitQC metadata 路径导入子块；
5. `committed_block_count` 对齐 payload 的真实 execution block number；每一份 StateDiff 和
   staking scan 都按真实执行顺序处理；
6. 普通单块不在 10K commit ring 重复保存大 execution output，避免高 TPS 下内存放大。

mock reth 回归明确断言导入顺序为 `144 -> 145`、本地高度 `143 -> 145`、两份 QMDB/Twig
StateDiff 均落入 FIFO sidecar。raw lineage 不完整或不能连到 exact head 时不使用其 cached
execution output，继续让 reth 的普通 payload 校验 fail closed。

## 验收记录

### 单测与静态门禁

- `n42-consensus-service`: 144/144；新增 prepared-lineage、缺父块高度校准与不完整 raw lineage
  fail-closed 测试。
- `n42-network`: 121/121；sync/2 request/response codec 与超长帧门禁全绿。
- `n42-node`: 143/143；`stream_v2_pipeline`: 9/9。
- `cargo check --workspace --all-targets`、`cargo test --workspace` 和
  `cargo clippy --workspace --all-targets -- -D warnings` 全部通过。
- 三个受影响包的独立 strict clippy：`--all-targets -- -D warnings` 通过。
- release `n42-node` 与 `e2e-test` 构建通过。

### P1-1 原命令性能门禁

```text
cargo test -p n42-consensus --test performance_bench --release \
  -- --ignored --nocapture --test-threads=1

5 passed; 0 failed; 910.53s
```

最新关键值：250K mobile receipts 288,794 ms；QC n=100/q=67 为 28.26 ms；
QC n=500/q=334 为 141.54 ms。500×500 与 100×2500 的 per-node mobile work 分别为
574 ms 和 2,883 ms，且 mobile verification 不在 consensus critical path。

### Release 实弹

- Scenario 5：3 节点高度 16/16/16；15 手机累计 240 accepted，0 rejected/errors。
- Scenario 9：3 节点、120 秒、node-2 停 20 秒，7/7 checks；最终高度 228/228/228，
  10 个 hash 样本一致，780 笔 ETH/ERC20，恢复节点补齐 78 块。
- Scenario 10：7 节点，10/10 checks；高度依次为 7/7=66、6/7=83、5/7=85、
  4/7=85（严格停机），全恢复后 138×7；区块 hash 一致，7 个 leader 全出现，
  595 accepted、0 reject。

## 兼容性

本轮已有 consensus 协议和 state-sync wire 断代变化，当前无主网，按任务书允许全验证者同时
升级。`/n42/sync/2` 明确隔离旧 bincode response；不能将新旧 validator 混跑后期待自动降级。
