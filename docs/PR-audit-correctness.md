# parallel-evm 硬化 + 跨 crate 正确性审计

> base: `main` ← compare: `chore/merge-reth-main-deps-upgrade`
> 本文档只覆盖该分支顶部的**正确性审计 / Block-STM 硬化**这一主题簇（7 个 commit），
> 不重复分支前段的 reth 2.3 合并、twig 引擎、BLS 批量等工作（见 `DEVLOG.md` 索引与
> `docs/devlog-67`/`-69`）。

## 概述

把 Block-STM 并行 EVM 的正确性收口，并把同一把审计标尺推到主执行路径之外的各 crate。
结果：**2 个真实正确性 bug 修复** + **5 块判定健全**（含逐条不变量核对）+ **全态
differential 回归**加固。

涉及 commit（旧→新）：

| commit | 主题 |
|--------|------|
| `51073d8` | fix(parallel-evm): 非 EOA beneficiary 的延迟 coinbase 修正 |
| `b9d17cf` | refactor(parallel-evm): 模块化 + 修复热账户上的 Block-STM soundness |
| `88d1d00` | audit(parallel-evm): mv_memory 并发复核 + 死代码清理 |
| `988f610` | audit: scheduler 收敛性 + execution_bridge 块导入（均健全） |
| `6123766` | audit(concurrency): consensus/twig 健全；修 network `set_validator_context` 静默丢弃 |
| `09b7301` | fix(execution): 状态 diff 保留 destroyed-then-recreated 活账户 |
| `987e293` | test(parallel-evm): 对 sequential 的全态 differential 回归 |

---

## 修复的真实 bug

### 1. Block-STM 在热账户上的 soundness（`b9d17cf`）

`hot_recipient` 差分测试在多次运行下偶发失败（8 跑 2 挂），暴露两个真问题：

- **validated_count 竞态**：`abort_and_reschedule` 用 load-then-store 改
  `validated_count`，与 `finish_validation` 的 CAS 竞争 → 计数错乱。改为 CAS 循环。
- **乱序并行校验不 sound**：之前的乱序校验重写会让某 tx 对中间态校验通过后**再不复检**。
  回退到 **in-order `val_cursor` 校验**（abort 时回退游标），并把读集校验从
  writer-identity 改为 **value-based**（记录读到的 `AccountSnapshot`/storage 值，重执行
  写了不同值的低位 tx 会被正确判失效）。

修复后 `hot_recipient` 30/30 绿，全套 ×30 绿。

### 2. 非 EOA / 发送者 beneficiary 的延迟 coinbase（`51073d8`）

延迟 coinbase 把 gas fee 以**可交换的 balance delta**累加（绕开版本化写，消除 coinbase
级联）。但仅当 beneficiary 是**非发送者的 EOA** 才成立：

- beneficiary 是合约：某 tx CALL 它可能**减少**余额而 nonce/code 不变，事后
  `new.saturating_sub(base)` 会饱和到 0 → 丢钱。
- beneficiary 同时是某 tx 的 sender：nonce 变化、非交换。

修复：`DeferredCoinbase::plan` 检测到不可延迟的 beneficiary（合约 / sender）→ 整块走
**sequential 回退**，结果与顺序执行严格一致。

### 3. network `set_validator_context` 静默丢弃（`6123766`）

该 async 方法原用裸 `try_send` 并 `let _ =` 丢结果。它**每次 epoch 切换**调用、驱动
Rotor relay 转发的 `my_index`/`validator_count`；命令通道（8192）瞬时打满时更新被静默丢，
relay 层用陈旧上下文转发且无任何痕迹。改为复用 `send_with_backpressure`（满则带超时
await），失败 `warn!` + drop 计数。签名保持 `()` 不变，两个调用方无需改动。

### 4. execution 状态 diff 误删 destroyed-then-recreated 活账户（`09b7301`）

`StateDiff::from_bundle_state` 对 revm 的 `DestroyedChanged` 状态（同块内 SELFDESTRUCT
后又被**重建**——EIP-6780 下可达：一笔 tx 内 create+selfdestruct，块内后续 tx 再触及该
地址）误判成 `Destroyed`。此时账户在块末**存在**（`info` 为 `Some`），但消费侧
`n42-jmt::apply_diff` 对 `Destroyed` 推 `key→None` 删叶子 → **活账户从状态树丢失，根错、
手机证明错**；且原 `debug_assert!(!(was_destroyed && info.is_some()))` 会在 debug 构建对这
个合法 revm 状态 panic。

修复：改按**块末存在性**（`current_info`）而非 `was_destroyed` 分类——`(_,false)=>
Destroyed`、`(false,true)=>Created`、`(true,true)=>Modified`；断言改为正确不变量
`Destroyed ⇒ current_info.is_none()`。重建账户走既有 Created|Modified upsert 路径。加 2 个
回归测试。

> 残留限制（EIP-6780 下不可达，记录备查）：`StateDiff` 无"整账户 storage 全擦除"标志
> （reth 用 `HashedStorage::wiped`），只逐槽 emit。但 n42 自创世即 Cancun/EIP-6780，被毁
> 账户没有已提交的历史存储槽，全部槽都在本块 bundle 内，故此限制对 n42 配置不可达。

---

## 判定健全的审计（无代码改动 / 仅防御断言）

- **n42-consensus 状态机**（`6123766`）：纯事件驱动单一所有权、一票制 fsync 先于广播、
  locked QC 单调、2f+1 门限 u64 防溢出 + bitmap 长度校验、Prepare/Commit QC 域分隔、
  changes_hash 三处绑定、Genesis QC 防回退、epoch 漂移区 bitmap 回退——逐条核对无 bug。
- **n42-twig-core 边界**（`6123766`）：prove/verify 索引数学逐位对称、shard 层 key+shard
  绑定正确；加一条 `debug_assert(value.len() <= u32::MAX)` 文档化 arena-offset 不变量
  （release 零成本）。
- **mv_memory 并发**（`88d1d00`）：单执行者保证、无 AB-BA、per-version-map RwLock 健全；
  清理死代码（`latest_*_writer`、`clear_tx` 的 `used_fallback`）。
- **scheduler 收敛 + execution_bridge 块导入**（`988f610`）：均健全。
- **n42-node orchestrator 3-way select**（`6123766`，只读）：biased 9 分支防饥饿、计时器
  每轮重建无 race、`recv`/`sleep` 取消安全、`Mutex` 不跨 `.await`、错误 log-and-continue。
  唯一可观察项是 `finalize_committed_block` 的 FCU 在 select 臂内同步、高负载可阻塞循环
  1–2s——**已知性能权衡（代码注释已述），非正确性 bug**，优化留作后续。

---

## parallel-evm 模块化（`b9d17cf`）

`lib.rs` 749 行 → ~400 行，按职责拆分：`coinbase`（延迟决策/delta/物化）、`execution`
（单 tx 执行 + 读集校验）、`output`（per-tx 输出 + 块态组装）、`worker`（rayon worker
loop）、`scheduler`（in-order 校验）、`types`、`mv_memory`、`parallel_db`。

## 全态 differential 回归（`987e293`）

把 parallel-vs-sequential 对拍从"只比 balance"升级到**字节级全态对拍**：每账户的
balance/nonce/code_hash/**每个存储槽** + 每笔 tx 的 gas/success。新增 `differential_tests`
模块（确定性 xorshift64 PRNG，无 `rand` 依赖）随机生成混合块，逐块跑 sequential（参照）+
parallel（×4 重复）：

- **counter 合约**（`slot[0]+=1` 字节码）被多 tx CALL → 重读写存储冲突，强制 abort/重执行
  级联；锁定"N 次调用后 slot0==N"。
- 热收款人（余额冲突）+ 冷收款人（无冲突）+ 延迟 coinbase（basefee 0）混入；8 种子 ×
  不同 tx/合约数 + 单 counter 高争用块。
- ×4 重复抓非确定性竞态（即修复前 `hot_recipient` flaky 那类）。

> 真实链上 tx 回放需把 reth-db/provider 接进来读 reth2k MDBX（依赖面大、与 reth pin 敏感），
> 留作更大后续；本轮用真实块"形状"的随机混合负载覆盖正确性边界。

---

## 验证

```bash
cargo test -p n42-parallel-evm          # 9 passed（含 differential）
cargo test -p n42-execution state_diff  # 21 passed（含 2 个新回归）
cargo test -p n42-twig-core             # 21 passed
cargo clippy -p n42-parallel-evm -p n42-execution -p n42-network -p n42-twig-core \
  --all-targets -- -D warnings          # 干净
```

- differential 套件 release 连跑 12 轮全绿（每轮 = 多种子 × parallel×4）。
- `hot_recipient` ×30 绿（修复前 8 跑 2 挂）。

## 后续（不在本 PR）

- B：把 parallel-evm 经自定义 `BlockExecutor` 接进 reth 生产路径（需重实现区块信封：
  系统调用/withdrawals/receipts）。
- A：Osaka/EIP-7928 BAL 并行执行（需硬分叉激活）。
- orchestrator FCU 挪出 select 热路径（性能优化）。
- 真实链上 tx 差分回放（需 reth-db/provider）。
