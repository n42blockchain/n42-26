# 四节点独立收款人负载与预签名文件

日期：2026-09-14（America/New_York；工具日志 UTC 已到 9 月 15 日）。
目标继续是四节点 H2、二叉树 QMDB-only 读取、成功提交 1M TPS。
本轮完成负载生成和输入验收，没有新的整链 TPS 成绩。

## 复用与修正

沿用 `n42-stress` 的真实 EIP-1559 / secp256k1 签名、N42T v2 文件和 ingest 流程，
接入 devlog171 已生成的 5000 个发送者与 147000 个独立收款人。
旧负载按 `nonce % recipients.len()` 选收款人，同 nonce 的发送者集中转入同一个账户，
且默认所有发送者也是收款人。这与 devlog165 记录的 rs 收款人索引教训直接相关。

新增 `--recipients-file`，检查非空、唯一、与发送者不重叠，使用：

```text
recipient = (nonce * total_senders + global_sender_index) % recipient_count
```

全局索引在创建账户时固定，RPC 子集和不同 batch 大小不改变它。用 u128 中间值避免
nonce 乘法溢出。在线签名、内存预签名、文件预签名共用同一个签名函数。
未提供收款人文件时保留历史映射，以便识别旧实验；不把旧文件标成新负载。

原预签名实现每个发送者创建一个 OS 线程，文件模式还累计全部签名后才写盘。
现在使用独立 Rayon 池，最多 16 个线程；文件模式每个任务最多 1024 笔，最多保留
一个任务/线程的结果，顺序收集以维持每个发送者的连续 nonce。写入同目录临时文件，
flush、fsync 后 rename，最后同步目录。旧目标文件保留到完整新文件发布时。
这是应用 Gov5 的“完整发布新文件、避免旧尾部混用”经验，不改 N42T 格式。

两个加载器现在检查总数、分组数界限、零长度交易和 EOF，拒绝截断、额外尾部及错误 chain ID。
这些是格式验证；实际交易签名仍由节点验证，生成负载另有逐笔离线审计。
nonce 查询改为拒绝错误、缺失和非法 RPC 返回；全部成功才更新本地 nonce，
初始同步失败不再继续拿零 nonce 签名。既有运行中子集重同步策略未重写。

## 新创世预签名

新增 `--presign-genesis`，仅用于显式指定的全新、零 nonce、纯转账创世负载，必须配合
`--presign-save` 与 `--recipients-file`。检查 chain ID、所有涉及账户存在、非空、无代码、
零 nonce，并检查发送者余额足够支付请求笔数的最坏费用预算。记录 genesis 和收款人文件
BLAKE3。此模式不查询网络，也不证明某条运行中的链仍在创世状态。
已有链在发送交易后应联网同步 nonce 重签，不能再直接重放零 nonce 文件。

已实际执行：

```bash
N42_CHAIN_ID=941004 target/release/n42-stress \
  --presign-genesis .artifacts/native-four-20260914/artifacts/genesis.json \
  --recipients-file .artifacts/native-four-20260914/recipients.json \
  --presign-save .artifacts/native-four-20260914/presigned-startup-220k.bin \
  --presign 220000 --accounts 5000 \
  --rpc http://127.0.0.1:23400,http://127.0.0.1:23401,http://127.0.0.1:23402,http://127.0.0.1:23403
```

文件 **30,577,467 字节**，四个 RPC 组各 55,000 笔。Release 审计逐笔解码、恢复
secp256k1 签名，校验 chain 941004、发送者记录、组别、gas/value、收款人和 nonce。
5000 个发送者各 44 笔，nonce 0–43；147000 个收款人全部覆盖，每人收到 1 或 2 笔。
生成命令 wall 1.21 秒、峰值 RSS 162576 KiB，仅说明负载准备开销，不是执行/共识 TPS。
文件位于忽略目录，证据归档不含共识私钥或大交易文件。

## 验收门槛与结果

`qualify-1m-tps.sh` 在联系节点之前运行新的负载预检查，核对可信配置与环境 chain ID、
N42T v2 头、最小文件尺寸，以及交易数至少满足 `ceil(minimum_tps * duration)`。
记录文件 SHA-256、大小、组数及交易数；该检查不代替 stress 的完整格式加载或节点执行。
默认 60 秒 / 1M 要求至少 6000 万笔；实际验证 22 万启动负载被拒绝。
原默认 3000 万笔文件同样不足以支撑这一门槛。按 1.2M/s 的提交目标准备一分钟负载
应至少 7200 万笔；本轮没有生成它，也没有将启动文件用于一分钟验收。

- Rust 8 项回归通过，覆盖三条签名路径、RPC 子集、余数/1024 分片、替换较小文件、
  格式损坏、分布、离线创世限制与 nonce 查询失败。
- 单独启用的 Release 实物审计通过，逐笔检查全部 220000 笔，32.53 秒。
- Python 2 项负载预检查测试通过；真实 22 万文件的小容量检查通过、1M 一分钟检查拒绝。
- stress all-targets Clippy `-D warnings`、修改文件格式、Python 编译、shell 语法、diff 检查通过。
- 当前节点、创世生成器与 stress 的 Release 构建均成功；二进制摘要在证据 manifest。
- 使用当前节点 Release 再次执行四节点启动，仍在 `socket(AF_INET, ...)` 被 EPERM 拒绝，
  尚未启动子进程，没有四节点 H2 提交或根/收据/TPS 运行证明。

[本轮证据](benchmarks/20260914-independent-recipient-workload/manifest.json) 固定源码、二进制、
负载摘要和日志。后续进入带交易整链验证；先定位任何原生第一块/执行桥接缺陷，再在允许
TCP/UDP 的环境运行足量负载、连续窗口和重启追赶。当前 H2 与 QMDB-only 约束不变，
不得拿签名速度或启动交易数充当 1M TPS 成绩。
