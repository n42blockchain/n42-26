
<!-- docs/QS_LINUX_ANSWERS_20260824.md lines 53-81 at 99f1039661f22362d91a9af2e49ecb98bc9c2d04 -->
## 阻塞项 2 — T1 / T5 / T8：Windows 最好成绩的完整配置

**[确定] T5：open-loop，不是 closed-loop。** 从未用过 `-target-depth`，benchmark 也从未开过 `txpool` namespace。
9,000,000 笔 = 3,000 senders × 3,000 tx，一次性预签后开灌。满负荷下出现 `pool full` 是预期现象，不是故障。

**[确定] T1：`-shard-senders`，你们描述的就是 canonical 策略。** 每个 sender 的完整 nonce chain 固定到一个 RPC，
不把同一条 chain 跨 RPC 广播。`-broadcast` 是对照变体，不是主路径。

**[确定] T8：最好成绩 22,487 TPS / 98.4% occupancy / 55-of-60 full blocks**，win1，v5.7.955，
2026-08-21 的 A' 轮。完整参数：

| 项 | 值 |
|---|---|
| 二进制 | `n42-v5.7.955.exe` |
| gasceil | 480,000,000 |
| block interval | 1000 ms |
| GOMAXPROCS/节点 | 5（32 线程宿主，7×5=35 故意略超） |
| pool | globalslots 300000 / globalqueue 100000 |
| senders × pertx | 3000 × 3000 |
| gasprice | 10 gwei 固定 |
| rpcbatch | 100 |
| conc | 32 |
| sharding | `-shard-senders` |
| sender-offset | 每轮全新，绝不复用 |
| decay | `-DecaySec 90` |
| 环境 | `N42_MAX_GOSSIP_MB=8`、`N42_STRESS_GASLIMIT=1`、`N42_TXINDEX_TAIL=1`、`N42_MDBX_MAPSIZE_GB=128` |
| 竞争 profiling | **关**（mutex/block 采样就在被测临界路径上） |

同轮 win2/win3 是 18,665 @89.1% 和 12,568 @53.2%——**不是退化，是 baseFee 轮内爬升**
