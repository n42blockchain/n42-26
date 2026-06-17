# Devlog 76: TPS 瓶颈全景图（历史 ~90K vs 近期 12K，含异步时序）

> 日期：2026-06-17
> 起因：历史记录有 ~9 万 TPS，近期 7 节点真机 profile 只有 12-13K。是 EVM、网络、还是
> 交易批量发送、多节点的瓶颈？本文对账历史、拆解瓶颈、画异步时序图。
> 数据来源：`devlog-49`（LAN cap sweep）、`devlog-50`/`90K-cap-timing-analysis`（90K）、
> `devlog-73`（7 节点共识 A/B）、`devlog-74`（EVM/state profile）、`devlog-75`（leader 火焰图）。

## 1. 历史对账：~90K 不是被 EVM 撑起来的，12K 也不是回退

| 记录 | TPS | workload | slot | 平台 | tx/block | 注入 | **真正的限制** |
|------|----:|----------|------|------|---------:|------|----------------|
| **峰值** | **91,214** | 简单转账 | 1.0s 固定 | **Ubuntu 单机 7 节点** | 90-95K | **TCP 122K** | compact-block P2P 传播 + 串行后处理哈希（RLP/tx_root/keccak ~920ms）。**EVM 仅 122ms** |
| LAN 极限 | 39-40K | 转账 | ~1.2s | LAN 7 节点 | 48K | RPC 20K | **RPC 注入封顶**（链能处理 40K，RPC 只灌进 20K）→ 后来上 TCP 破解 |
| 近期真机 | 12-13K | **合约重(ERC-20)** | 2.0s | **macOS** | 48K | TCP | **EVM 执行(state-heavy 605ms) + finish/root keccak(613ms) + import 尾** |

**结论：12K 不是性能回退，是完全不同的一档负载** —— 合约重(每 tx ~65-100k gas、状态读写重)
+ 2s slot + macOS，vs 历史的简单转账(~21k gas、EVM 极轻) + 1s slot + Ubuntu。**同口径下链本身能跑 ~90K。**

## 2. 瓶颈到底是哪个？—— 四个都真实，但"谁主导"看负载

| 候选瓶颈 | 是不是真的 | 何时主导 | 证据 |
|----------|-----------|----------|------|
| **① 交易批量发送** | ✅ **真** | RPC 注入时永远卡 20K | devlog-49：JSON-RPC ~20K tx/s 封顶；链处理 40K。**要 90K 必须 TCP 122K 批量注入** |
| **② 网络 / 多节点传播** | ✅ **真** | 高 tx/block + 简单负载 | 90K 转账时**主瓶颈就是 compact-block P2P 广播**(leader→6 follower、90K tx_hash、follower 从 pool 匹配重组)。随 tx/block 和节点数涨 |
| **③ EVM 执行** | ⚠️ 看负载 | **合约重**时主导 | 简单转账 EVM 仅 122ms/90K；合约重 605ms/48K(SLOAD/SSTORE journal + keccak)。devlog-75 火焰图 |
| **④ 串行后处理(finish/root)** | ✅ **真** | 大块时都吃 | RLP 解码 + tx_root + state_root keccak：90K 时 ~920ms，48K 合约时 613ms(keccak 占 57.8%)。部分已修(cache-hit fast path + deferred state root) |

**一句话**：
- **简单转账想冲高**：瓶颈是 **① 注入(必须批量/TCP) → ② 网络传播 → ④ 串行哈希**，**EVM 不是瓶颈**；
- **合约重**：瓶颈是 **③ EVM 执行 + ④ finish/root**(devlog-74/75)；
- **共识/FCU/多节点投票**：~400ms(devlog-73)、FCU p95 12-14ms —— **不是瓶颈**(远在 slot 内)。

## 3. 异步时序图：一个 slot 的流水线（并行重叠是关键）

链的核心优化是**把 import / 共识 / 下一块构建尽量并行**，让关键路径只剩"构建 + 传播 + 投票"。

```mermaid
sequenceDiagram
    autonumber
    participant S as Stress(注入器)
    participant L as Leader
    participant F as Followers ×6
    participant E as reth EL

    Note over S,F: ① 批量注入(TCP 122K / RPC 20K封顶)填 tx pool
    S->>L: txs → pool
    S->>F: txs → pool

    Note over L,E: ② Leader 构建 payload(关键路径)
    L->>E: 拉 pool + EVM 执行 + 组装
    Note right of L: 转账: build≈439ms(EVM 122ms)<br/>合约重: pack p95≈1.05s(EVM 809ms)

    Note over L,F: ③ 广播 compact block(网络瓶颈点)
    L->>F: CompactBlock(header + tx_hashes)
    Note right of F: follower 从本地 pool 匹配重组<br/>90K tx 时这步≈1s(主瓶颈)

    par 异步重叠①: eager import 与共识并行
        L->>E: eager new_payload(无 FCU)
        F->>E: eager new_payload(无 FCU)
        Note over E: 块提前进 reth engine tree<br/>(import_headroom)
    and 异步重叠②: 共识投票
        F->>L: Vote
        L->>L: 聚合 BLS → QC(2f+1)
    end

    Note over L,F: ④ Commit(共识完成 ~400ms 累计)
    L->>F: Commit/Decide

    par 异步重叠③: finalize 与下一块并行
        L->>E: finalize FCU(p95 12-14ms;<br/>Stage 8 可移出热路径)
    and
        L->>E: 触发下一块构建
    end
    Note over L,F: ⑤ 进入下一 slot
```

**异步重叠点**（这些不在关键路径，是性能关键）：
- **eager import 与共识并行**：块在投票期间就 `new_payload` 进 reth(`import_headroom`)，commit 后 FCU 秒过(Case A)；
- **finalize FCU 与下一块并行**(Stage 8，flag-gated)：FCU 移出 select 热路径；
- **state root deferred**：用 B256::ZERO 占位，不在构建关键路径。

## 4. slot 预算时间线（两档负载对比）

```
简单转账档(1s slot, Ubuntu, 90K cap)  —— EVM 不是瓶颈
0ms        439ms                          ~1465ms      ~1000ms slot
|--build----|----compact P2P 广播(主瓶颈)----|  + 串行后处理 920ms(已修)
 EVM 122ms                                   ‖ eager import / 投票 并行其中

合约重档(2s slot, macOS, 48K cap)  —— EVM 是瓶颈
0ms                1050ms              1600ms        2000ms slot
|----pack(EVM 809ms)----|--finish/root--|--投票--|  commit累计~400ms
                          563ms(keccak)         ‖ import 尾 cache-miss p95 2.2s
```

## 5. 结论与方向

- **链的吞吐天花板很高**(同口径转账 ~90K)；近期 12K 是"合约重 + 2s slot + mac"的另一档，非回退。
- **想再压吞吐，按目标选杠杆**：
  1. **简单转账冲峰**：保证 **TCP 批量注入**(别用 RPC，20K 封顶)+ 优化 **compact block 传播**(已有直推+gossip 兜底)+ **串行后处理哈希**(cache-hit fast path 已修大部);
  2. **合约重提速**：**EVM 执行**(parallel-evm,但 devlog-75 测出上限仅 ~1.5x、且热点是 state 而非解释器)+ **finish/root keccak**(merkle 固有,难砍);
  3. **多节点不是瓶颈**(共识 ~400ms、FCU 12-14ms),除非节点数 ×× 后 P2P 扇出成本上来。
- **measure-first 的回报**：parallel-evm(B)只对合约重有 ~1.5x、对转账无用(转账 EVM 才 122ms);真正的高 TPS 杠杆历史上是**注入 + 网络 + 串行哈希**,不是 EVM。
