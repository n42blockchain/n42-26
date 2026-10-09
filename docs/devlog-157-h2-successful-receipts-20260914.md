# H2 成功交易吞吐与 Gov5 回执根核验

日期：2026-09-14，接续 [devlog-156](devlog-156-qmdb-borrowed-operations-20260914.md)。
继续保持 H2 共识与二叉树 QMDB 状态读取。**4 节点 1M TPS 尚未实测达标。**
本轮补验收口径，不提供新的性能数字。

## 包含交易不等于执行成功

此前资格脚本已要求不同验证者/进程、QMDB only + WAL、真实读取进展以及共同 H2
提交边界，但最终阈值仍用 `strict_committed_tps`，即区块包含交易数除以观察时间。
失败回执也计入，因此即便包含吞吐达标，执行成功吞吐仍可能不足。

现在边界/结果 schema 升为 3，最终通过条件改为 `successful_committed_tps`。
`N42_BENCH_MIN_COMMITTED_TPS` / `--min-tps` 保留参数名，含义改为成功提交 TPS，默认
仍为 1,000,000。原包含数和 `strict_committed_tps` 保留作诊断，不参与通过判断。
拒绝旧 schema 边界，避免旧测量被当作已具备回执证据。

一个明确的离线反例：共同提交 9 笔，观察 63 秒；其中 3 笔失败，且失败状态合法进入
回执根。包含 TPS 为 0.142857，高于示例阈值 0.1；成功 TPS 只有 0.095238。
新的真实 CLI 路径返回非零并保存 `below_target`，不因包含吞吐较高而通过。
这是合成测试，不是实际 fleet 测量。

## 按真实 Gov5 格式重算回执根

审阅当前 `crates/n42-consensus/src/receipt.rs` 确认：Gov5 replay-v2 的 native
receipt root 是连续 RLP `[status, cumulativeGasUsed, logs]` 的 Keccak-256。
它不是 Ethereum receipt trie，不包含 receipt bloom 或 typed envelope；空输入为
Keccak 空串。使用 SHA3-256 或以太坊 receipt trie 会算出另一种根。

对共同提交区间内的每个块、每个验证节点，审计现在执行：

1. 按已核对的区块 hash 获取区块和 `eth_getBlockReceipts`；检查各节点的完整交易
   hash 顺序、区块 hash/高度/父 hash、状态/回执/交易根及 gasUsed 一致。
2. 回执数量必须等于交易数，每条回执的 block hash/number、transaction hash/index
   必须对应同一位置。拒绝缺失、额外、乱序、重复身份或非 0/1 的状态。
3. 累积 gas 不回退且在 u64 内，gasUsed 等于相邻差额，最终累积 gas 等于区块 gasUsed。
4. 核对日志的块/交易身份、连续 log index、removed=false、地址/topic/data 编码。
   重算 native 回执根并对比报告的已提交头部 receiptsRoot，然后统计成功与失败。

四个节点各自核验，不将其成功数相加。结果逐块记录成功/失败数、重算根及已核验
节点数，并保存 Keccak helper 二进制 SHA-256；helper 在审计期间变化会拒绝结果。
RPC 缺数据、回执损坏或辅助程序失败均失败，不退回包含吞吐口径。

Python 负责独立 RLP 编码，新增小程序 `n42-keccak` 使用已有 tiny-keccak 依赖，
以 64 KiB 缓冲流式读取 stdin，输出 32-byte 原始摘要。没有新增根 manifest/lock
依赖，也没有启动网络服务。每条 RLP 回执逐步写入管道，避免额外拼接整块编码副本。

## 使用与开销边界

```sh
cargo build --release --bin n42-keccak
N42_BENCH_NODES=4 N42_BENCH_MIN_COMMITTED_TPS=1000000 \
  bash scripts/qualify-1m-tps.sh
```

可用 `N42_KECCAK_BIN` 指向同一 helper 的其他构建路径。压测脚本先检查可执行文件，
再用 `capture --verify-receipts` 核验每个节点最近提交块，确认回执 RPC 和根格式可用
后才投递负载。正常开始/结束边界捕获不取全量回执；结束负载后逐块进行完整审计。
吞吐分母仍覆盖全部开始/结束边界 RPC 时间，未把节点 TPS 求和或忽略慢节点。

完整回执 RPC 开销可能很大。审计按块和节点顺序处理，不积累整个窗口的回执，但
Python/RPC 仍会物化单块完整 JSON；服务器响应大小限制、回执保留和内存需求必须
满足实际区块规模。本轮没有测量 1M TPS 规模下的审计耗时/内存，也没有通过采样
或跳过回执让大块勉强通过。审计新增开销在测量窗口之后，不代表链吞吐优化。

## 验证与明确未覆盖项

- Python 离线测试：**30 passed**，包括真实 CLI 阈值、预检失败、四节点逐一篡改
  状态、丢失/额外/错序回执、gas/日志错误、旧 schema、helper 错误和跨 64 KiB 流。
- 生产 Rust native receipt root 的两项 fixture 测试通过。新增合成样本含三个状态
  `[成功, 失败, 成功]`、两个 topic 和 80-byte 日志；Python 与 Rust 都得到
  `0x91e7d4d76b7fe8687ce49e001e8438a779fbbc7b59b797b47f2dc7cd9ddadd9e`。
- workspace all-targets clippy `-D warnings`、shell/Python 语法、`git diff --check` 通过。
- [源码、fixture、日志与合成示例报告](benchmarks/20260914-h2-receipt-audit/)。Rust 验证仍
  使用固定补丁 reth `23316e3ff8adca8c3bd5085ff0565fcae019202a`。

本轮证明回执内容与 **RPC 报告的提交头部回执根**一致，不是独立的整链证明。
尚未独立验证 BLS 证书、重算头部 hash/交易 trie root、重放全部 EVM 交易、核对
负载预期业务状态或证明硬件耐久；QMDB only 的覆盖范围仍是 exact-block provider。
也尚未完成真实四节点连续窗口及重启追赶。上述剩余项不能由离线合成报告替代，
目标保持进行中。
