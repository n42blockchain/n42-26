# QMDB 运行时证据接入四节点吞吐验收

日期：2026-09-14，接续 [devlog-149](devlog-149-qmdb-single-apply-commit-20260914.md)。
**4 节点 1M TPS 尚未实测达标，目标继续进行。** 本批补齐实际读取和节点身份的
验收门槛，保持 H2 共识、二叉树 QMDB、原有执行检查和 WAL 同步语义。

## 从实际 adapter 获取证据

新增 `n42_stateReadStatus`，数据来自已经注册到 reth provider hook 的 adapter。
没有注册时报告 `off`，不根据 launcher 环境变量猜测。CLI 将本地 BLS 公钥传入 RPC；
每次进程启动生成新的随机 instance ID。公钥和 instance ID 都是公开自报告字段，
不是远程认证或独立证书验证。

JSON-RPC 请求示例：

```json
{"jsonrpc":"2.0","id":1,"method":"n42_stateReadStatus","params":[null]}
```

将 `null` 换为区块 hash，可以查询这个精确版本已知的耐久 QMDB 根；未知版本及
WAL append 尚未完成的版本返回空根。响应还包含实际 mode、backend、coverage、
WAL 是否启用、chain/genesis/base identity，以及九项累计计数：账户读取、槽读取、
账户比较、槽比较、mismatch、读取错误、provider 错误、固定版本的 provider 数和
版本不可用次数。不存在的 key 是一次成功读取，解码失败不计为成功读取。

读取错误计数覆盖 QMDB 解码和 verify 模式中的数据库比较错误；provider 错误覆盖
adapter 准备/固定视图失败，不代表节点所有错误。计数采用 64 个按线程分配的
cache-line 隔离分片，不依赖 Prometheus recorder。快照逐计数单调，九项合起来
不是事务快照；验收不要求它们在同一瞬间满足代数关系。

## 资格脚本的实际约束

`qualify-1m-tps.sh` 在加载压测交易前做 preflight。开始和结束采样仍以所有节点
共同的 H2 commit 区间为准，采样 RPC 耗时全部进入 TPS 分母。现在还要求：

1. 每个节点实际注册 `only` 模式、`gov5_qmdb_binary` 后端和持久化 WAL store。
   默认 `off` 不变；部署时必须显式启用 only，verify/on 不能通过正式资格门槛。
2. 四个进程实例和本地验证者公钥分别唯一，公钥属于一致的 active validator roster，
   QMDB chain/genesis 与执行链匹配。进程、adapter 身份及 validator 索引顺序在窗口内稳定。
3. 每次边界用读取计数在前后包住 H2 观察，再用所观察的 committed hash 查询耐久根。
   QMDB 根必须等于该执行区块的 stateRoot。起始最早计数到结束最晚计数形成覆盖区间。
4. 每节点都必须有新增 account reads 和 pinned providers。普通转账可以没有 slot reads。
   mismatch、read/provider errors、unavailable providers 不能增加，计数不能回退。
5. 默认共同提交 inclusion TPS 至少 1,000,000。降低 TPS 阈值用于小规模测试时，
   上述架构和正确性检查仍有效。错误退出码非零，不能输出 `passed`。

边界文件升级为 schema 2；旧的 schema 1 不含读取证据，拒绝作为新资格输入。
`h2-preflight.json`、`h2-start.json`、`h2-end.json` 保留原始边界；
`h2-audit.json` 与 `summary.tsv` 保存每节点身份和计数增量。

## 验证与读取计数成本

- 完整节点 QMDB 单元测试：38 passed、5 ignored（显式测量）。包含真实 REVM 的
  only 读取、verify mismatch 和畸形槽错误计数断言；128 个短线程跨分片碰撞后计数正确。
- 真实 `BlockchainProvider` + MDBX 集成测试：1 passed。数据库故意缺账户/槽，
  注册 hook 后从 QMDB 返回正确值；检查注册前 off、注册后 only、WAL/根/公钥及计数，
  并通过实际内存 JSON-RPC dispatch 验证返回格式。
- 资格脚本：20 项离线回归通过，包括节点掉队、链分歧、同进程别名、重复公钥、
  不匹配的根、零读取、计数重置、读取错误、采样中重启、roster 变化及真实 CLI 失败退出。
- workspace all-targets clippy `-D warnings`、shell/内嵌 Python 语法检查通过。

独立 release harness 逐字编译实际读视图和计数模块；只替换无关 node mode enum。
200k 个真实编码账户，每个样本读取 1M 次，每个线程配置交替做 6 对。检查 nonce 总和和
精确计数增量。首次未限制 CPU 的结果波动较大，因此以同一二进制补做 CPUs 0–15
进程亲和性对照；没有隔离主机负载或逐线程绑核。

| 线程数，CPUs 0–15 | 不增加计数，中位数 | 增加计数，中位数 | 中位耗时变化 |
| --- | ---: | ---: | ---: |
| 1 | 233.40 ms | 239.08 ms | +2.43% |
| 16 | 14.118 ms | 14.112 ms | -0.05%，未分辨出额外成本 |

该测量只包含真实 account lookup/decode 外增加一个计数的成本，**不包含完整 provider
及其既有 metrics、EVM、持久化、网络和 H2**；负的估计值不意味着计数提高性能。
完整日志、源码、依赖锁及二进制哈希见
[实验目录](benchmarks/20260914-qmdb-read-status/)。没有将状态读取速度换算为链 TPS。

## 剩余验收边界

这些是节点 RPC 自报告和共同链检查，仍不独立验证 BLS QC、每笔交易的成功执行或
硬件掉电耐久性。实际账户/槽读取证明限定在现有 exact-block provider hook；
latest/pending RPC 路径尚未全部迁移，bytecode/proof 兼容接口仍委托 reth，原始
state table 写入继续保留。`walEnabled` 不排除 tmpfs，也不是硬件持久化认证。

联网四节点连续多窗口、真实重启/追赶、完整交易和状态核对仍未完成。当前沙箱创建
socket 返回 EPERM；本批通过内存 RPC dispatch 验证接口，没有把它标成联网压测。
之后继续减少执行路径重复计算，并把逐交易执行、恢复和水位检查纳入真实 fleet 验收。
