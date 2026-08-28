# Devlog 139：恢复 zstd 与 QUIC/libp2p 直推审计

日期：2026-08-24

## 结论

本轮不移除 zstd，并将 `N42_PAYLOAD_ZSTD=1` 明确写入 testnet 默认启动环境。
一分钟对照数据表明，binary-v1 已经显著减少原始序列化数据和 CPU 时间，但压缩后的
网络包只减少约 4%；如果关闭 zstd，包会越过 GossipSub/直推上限并使链快速失速。

直推异常不在 zstd、认证、codec 或 QUIC 开流阶段。根因是组合 `NetworkBehaviour` 中
`connection_limits` 排在有状态的 request-response 行为之后：每个 peer 同时发起的候选连接
会先被 request-response 记入 `connected`，再被最后的连接限制拒绝。被拒绝的 handler 从未
安装进 Swarm，但它们的 `ConnectionId` 留在 request-response 中；后续请求轮询到这些失效 ID
时被 Swarm 静默消费，不触发 codec，也没有 failure/timeout。诊断时每个 peer 错误缓存 6-9 条
连接，43 次 dispatch 只有 1 次进入 handler。将 `connection_limits` 移到组合行为第一位后，
每个 peer 只缓存 1 条有效连接，正式一分钟复测达到 204 dispatch / 204 ACK，failure、认证拒绝
和覆盖排队均为 0。

后续 round40--round52 继续处理线程过量、大块复制、提交 cadence 和 sender drain。7 节点同机运行时，不再让每个节点都按
整台 256 线程主机建立一套线程池；执行块 envelope 也改为在 leader feedback、待执行缓存、
最近块缓存和直推起源之间共享同一个 `Arc<Vec<u8>>`。state diff 与 staking 又从提交热路径
解耦。当前仅直推、zstd 开启的一分钟严格记录达到
**9,390,144 tx / 60.001004 s = 156,499.78 TPS**。round52 的 equal-tip sender-run merge
相对 round50 再提高 3.05%，距 1M 为 6.39 倍。
round46 的 follower binary-v1 解码进一步共享解压 backing allocation，整轮少复制 8.30 GiB；
其严格结果 152,410 TPS 与记录相差 0.51%，两轮均提交 58 个交易块。

主机 UDP 接收缓冲仍是独立的次要瓶颈。应用请求 16 MiB，受主机 4 MiB
`rmem_max/wmem_max` 限制，Linux `getsockopt` 实际报告 8 MiB。一分钟运行的 69 秒资源采样窗口
仍新增 9,781 个 `UdpRcvbufErrors`，约占接收 UDP 数据报加错误总数的 0.59%；QUIC 重传保证了
本轮直推全部成功，但现场仍应提高内核上限。

## 一分钟 A/B 数据

共同形状：7 节点、163,000 tx/block、持续 TCP ingest、60 秒压测窗口、zstd level 1、
直推 fanout 6、GossipSub 可靠兜底。round29/30 的历史窗口 TPS 按压测起止时间内 leader
发出的交易块统计并固定除以 60 秒；`n42-stress` 结束后的 `BLOCK_ANALYSIS` 会包含尾部区块，
不能用于这组配对比较。该历史口径不等于窗口内已收到 `Decide` 的严格提交，后文有复核。

| 运行 | 执行负载格式 | 历史窗口 TPS（leader 提案） | 相对 JSON |
| --- | --- | ---: | ---: |
| round29 | legacy JSON + zstd | 85,050 | 基线 |
| round30 | binary-v1 + zstd | 90,783 | +6.74% |

数据量与通信资源：

| 指标 | JSON | binary-v1 | 变化 |
| --- | ---: | ---: | ---: |
| payload 原始数据 | 39,585,688 B | 20,038,045 B | -49.38% |
| payload zstd 后 | 14,943,334 B | 14,295,454 B | -4.34% |
| 完整传播 envelope | 15,606,245 B | 14,957,470 B | -4.16% |
| 归一化源端通信量/leader 提案 tx | 4,262 B | 4,015 B | -5.79% |

这里的“归一化源端通信量”按 leader 的 6 份直推加 1 份 GossipSub 起源副本计算，不等于
交换机端口的最终 mesh 总流量。它适合比较 codec，不应当被解释为物理网络精确账单。

日志样本中的 codec CPU 时间也明显下降：

| 阶段 | JSON | binary-v1 |
| --- | ---: | ---: |
| leader serialize | 50.5 ms | 4.03 ms |
| leader zstd compress | 55.1 ms | 32.2 ms |
| follower zstd decompress | 30.5 ms | 11.35 ms |
| follower deserialize | 24.7 ms | 13.08 ms |

这解释了 binary-v1 的 TPS 收益，也解释了为何不能因为原始数据减半就预测网络带宽也减半：
JSON 的冗余本来就容易被 zstd 压掉，最终 envelope 只减少约 4%。

## 为什么恢复 zstd

| 运行 | 被关闭的压缩层 | 大包 envelope | 窗口吞吐表现 | 结果 |
| --- | --- | ---: | ---: | --- |
| round32 | payload 外层 zstd | 20,697,396 B | 约 2.67k TPS | 超过 16 MiB GossipSub 上限，链失速 |
| round31 | block payload 各压缩层 | 44,864,336 B | 约 267 TPS | 同时超过 16 MiB GossipSub 和 32 MiB direct 上限 |

因此 zstd 不是当前应删除的 CPU 开销。正确顺序是保留 level 1，并继续用 binary-v1 减少
压缩前的分配、序列化和解压/反序列化时间。`N42_PAYLOAD_ZSTD=0` 只保留为隔离 A/B 开关，
正常 testnet/生产形状默认使用 `1`。

## 直推路径证据

以下为各运行完整日志中的计数，不把“已进入 libp2p 队列”误当成“远端已收到”：

| 运行 | dispatch | recv | ACK | failure | auth reject |
| --- | ---: | ---: | ---: | ---: | ---: |
| round29 JSON | 47 | 5 | 5 | 0 | 0 |
| round30 binary-v1 | 42 | 0 | 0 | 0 | 0 |
| round32 payload no-zstd | 24 | 3 | 3 | 0 | 0 |
| round33 40/96 MiB QUIC 窗口 | 39 | 2 | 2 | 0 | 0 |

round33 中两个 ACK 只有一个是约 14.95 MB 的完整负载，另一个是约 2 MB 的小负载。
所以把 `max_stream_data`/`max_connection_data` 从 10/15 MB 调到 40/96 MiB 后，完整直推
仍未恢复，流控窗口不是充分条件。

`failure=0` 也不代表请求健康。当前 libp2p request-response 的请求超时在子流协商完成后
才进入 worker；等待 outbound substream 的请求可长期留在 handler 队列而不产生 timeout。
本轮已有的 per-peer 策略因此继续保持“一条在途 + 一条最新覆盖”，防止每个 15 MB 区块
无限堆积；ACK latency 从 `send_request` 前开始计时，可覆盖应用看到的排队时间。

进一步分层诊断得到决定性证据：

| 运行 | dispatch | handler enqueue | emit substream | codec write/read | ACK | 结论 |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| round36，QUIC streams=4096 | 43 | 未插桩 | 未插桩 | 1 | 1 | 增加子流上限无效 |
| round37，行为/命令公平性 | 43 | 未插桩 | 未插桩 | 1 | 1 | 单独调整 poll 公平性无效 |
| libp2p 根因诊断 | 43 | 1 | 1 | 1 | 1 | 42 次在 Swarm→handler 前丢失 |
| connection-limit 修复短压 | 78 | 78 | 78 | 78 | 78 | 失效 ConnectionId 已消除 |
| round38 正式 60 秒 | 204 | 正式包不含临时插桩 | 正式包不含临时插桩 | 204 | 204 | 端到端全部成功 |

诊断版在 request-response 内记录了连接选择：修复前每个 peer 的
`connection_count=6..9`，其中只有一条是 Swarm 中的有效连接；修复后 78 个诊断请求的
`connection_count` 全部为 1，且所有阶段一一对应。临时第三方依赖插桩在正式构建前已经移除。

round38 的 60 秒 leader 提案窗口直推时延：

| 阶段 | 样本 | 平均 | p50 | p95 | 最大 |
| --- | ---: | ---: | ---: | ---: | ---: |
| codec write 完成 | 204 | 28.32 ms | 26 ms | 49 ms | 246 ms |
| codec read 完成 | 204 | 88.69 ms | 80 ms | 208 ms | 578 ms |
| dispatch 到远端 ACK | 204 | 398.61 ms | 357 ms | 661 ms | 878 ms |

因此 10 秒 timeout 在本轮有充分余量；本轮没有子流持续排队，也没有因为认证映射缺失而
回退。`failure=0` 现在与完整的 write/read/recv/ACK 计数一致，而不再是静默丢失造成的假健康。

## 认证映射审计

入站 block-direct 仍只接受已经用验证过的共识签名完成 BLS/PeerId 映射的验证者。没有为了
提高性能而放宽认证。Identify 的 validator index 只负责路由，不能替代签名认证。

本轮改动：

- 重复的同一认证映射降为 debug，不再以 info 洪泛日志；
- 新增 `n42_authenticated_validator_peers` gauge 和首次 promotion counter；
- Identify 记录对端是否广告 `/n42/block-direct/2`；
- 每个大包 dispatch 同时记录 `advertised_support` 与发送端看到的
  `authenticated_remote`，从而区分协议不支持、认证状态和 QUIC 拥塞。

## 修复

1. 将 `connection_limits` 放在组合 `NetworkBehaviour` 第一位，使多余连接在任何有状态的
   request-response 行为缓存其 `ConnectionId` 之前被拒绝。这是直推恢复的根因修复，也避免
   consensus-direct、state-sync 和 Gov5 RPC 出现同类失效连接缓存。
2. QUIC 默认流窗口提高为 40 MiB/stream、96 MiB/connection，并保留环境变量覆盖。
3. block-direct 已协商子流的 timeout 从 30 秒改为默认 10 秒，可用
   `N42_BLOCK_DIRECT_REQUEST_TIMEOUT_MS` 调整（1-60 秒）。这不会假装解决协商前排队，
   但能更快释放真正卡住的读写 worker。
4. 节点在 QUIC listener 建立后将 `SO_RCVBUF`/`SO_SNDBUF` 请求为 16 MiB，并记录内核
   实际值；可用 `N42_QUIC_UDP_SOCKET_BUFFER_BYTES` 调整。libp2p 当前不暴露这两个 socket
   参数，因此 Linux 实现对本进程相同 UDP 端口的 descriptor 原位设置，不更换 QUIC/TLS。
5. 当前主机 `rmem_max/wmem_max` 只有 4 MiB，应用请求 16 MiB 会被内核截断。现场部署应先
   设置至少：

   ```text
   net.core.rmem_max = 16777216
   net.core.wmem_max = 16777216
   ```

   Linux `getsockopt` 报告值可能是请求值的两倍，这是内核的 bookkeeping 语义；以节点启动
   日志中的 `receive_bytes`/`send_bytes` 和压测前后 `UdpRcvbufErrors` 增量为验收依据。

6. 网络服务循环每轮最多处理 8 条 priority command，可靠/数据命令的积压改为非阻塞回压，
   并优先轮询 Swarm；block-direct/consensus-direct 置于 GossipSub 前。round37 证明这些公平性
   调整不能单独修复失效连接 ID，但保留后可防止高负载命令源长期饿死 Swarm 和关键子行为。

## round38：正式一分钟验收

形状：EPYC 9B45 128C/256T、7 节点、163,000 tx/block、binary-v1、zstd level 1、直推
fanout 6、GossipSub 同时兜底、每节点持续 TCP ingest、交易转发关闭。历史统计窗口为
`02:15:30.521326Z..02:16:30.523491Z`。

| 指标 | 结果 |
| --- | ---: |
| 60 秒窗口 leader 提案 | 5,437,000 tx |
| leader 提案墙钟 TPS | 90,616.67（按固定 60 秒；实际窗口为 90,613.40） |
| 复核后的窗口内提交 | 5,437,000 tx，90,613.40 TPS |
| `n42-stress` 活跃区块时间戳 TPS | 151,027.8 |
| 注入 | 7,143,662 tx，错误 0；完成阶段 116,339 tx/s |
| 交易区块 | 34（首块 58k，其余 33 个 163k 满块） |
| 直推 | 204 dispatch / write / read / recv / ACK；failure/reject/coalesce=0 |
| 认证与能力 | 42/42 promotion；42/42 Identify 广告 direct；auth reject=0 |

必须区分 TPS 口径：151,028 是工具用活跃区块 header timestamp 计算的指标；90,617 是
leader 提案窗口指标，完整的严格提交复核为 90,613。前两者不能互相替代，本轮没有
突破 10 万。
直推修复解决的是可靠性和静默丢请求，不是当前的出块/执行 cadence 瓶颈。

### 数据量、压缩与通信资源

leader 提案窗口内 34 个交易区块的传播账户：

| 指标 | 60 秒总量 | 平均速率/比例 |
| --- | ---: | ---: |
| binary-v1 payload 原始 | 668,381,821 B | 122.94 B/leader 提案 tx |
| payload zstd 后 | 402,449,906 B | 原始的 60.21%，节省 39.79% |
| execution output | 22,320,602 B | 4.11 B/leader 提案 tx |
| 完整 envelope（单份） | 424,773,262 B | 78.12 B/leader 提案 tx |
| 6 份直推 | 2,548,639,572 B | 42.48 MB/s |
| 1 份 GossipSub 起源 | 424,773,262 B | 7.08 MB/s |
| leader 逻辑起源总量 | 2,973,412,834 B | 49.56 MB/s，546.88 B/tx |

`leader 逻辑起源总量` 是 6 份 direct 加 1 份 gossip publish，不含 GossipSub mesh 后续转发、
QUIC/IP/UDP framing、ACK、共识消息或 TCP ingest。主机 loopback 在 69 秒资源窗口内单向计数
增加 18.72 GiB（291.36 MB/s）；Linux loopback 同一字节同时计入 RX 和 TX，所以若机械相加
则是 37.44 GiB/582.72 MB/s，不能当作两倍有效载荷。

codec 时间随整机 CPU 竞争而变化，本轮 leader 提案窗口为：leader serialize 平均 4.32 ms，zstd
compress 59.85 ms，follower decompress 10.50 ms，deserialize 11.43 ms。zstd 的 39.79%
payload 节省仍远大于去掉它的收益；关闭 zstd 会直接越过传播上限，不能作为提速方案。

### CPU、内存、SSD 与 UDP

| 69 秒采样项 | 结果 | 判断 |
| --- | ---: | --- |
| 7 节点注入期平均 CPU | 15,404% = 154.0 cores | 256 逻辑 CPU 的 60.2% |
| 7 节点 1 秒峰值 CPU | 23,389% = 233.9 cores | 256 逻辑 CPU 的 91.4% |
| 7 节点 CPU 时间 | 10,019.4 core-seconds | 全采样期平均 145.2 cores |
| 节点合计 RSS | 注入期均值 26.9 GiB、峰值 39.1 GiB、结束 40.9 GiB | 内存容量足够，但工作集很大 |
| stress 客户端最大 RSS | 6.29 GiB | 含 30M 预签名交易索引/重分组 |
| 节点线程合计 | 14,893 → 16,780 | 约 2,100-2,400 threads/node，明显过度并发 |
| 节点 write bytes | +8.21 GiB | 约 127.7 MiB/s |
| `/data` NVMe 实际写入 | +8.53 GiB，约 132.7 MB/s | 约 1,507 writes/s，device busy 仅 3.7% |
| UDP 收/发 | +1,637,328 / +1,640,975 datagrams | 约 23.7k datagrams/s |
| `UdpRcvbufErrors` | +9,781 | 约 0.59%，仍需提高 host socket 上限 |

SSD 远未饱和；更快 SSD 不会按硬件标称倍数直接变成 TPS。CPU 总量也不是简单“还有 40%
空闲”：7 个验证者重复执行同一块，关键路径受单节点执行、共识轮转、NUMA/调度和每节点
两千多个线程影响。当前最值得优先处理的是线程来源/线程池上限、单块关键路径和严格墙钟
cadence，而不是删除 zstd 或继续盲目放大 QUIC 子流数。

## round39：仅直推的一分钟隔离测试

新增显式基准开关 `N42_BLOCK_DIRECT_ONLY=1`。它只关闭执行块完整 envelope 的 GossipSub
副本，不关闭共识 proposal/vote 等必要的小消息。关闭条件不是“已经把请求放进本地队列”，
而是配置了有限且非零的 fanout，并且当前能解析出完整的目标集合；本轮要求 6/6。启动期或
节点掉线导致目标不足时仍走 GossipSub，避免为了测性能制造静默丢块。正常默认值仍为 `0`。

形状与 round38 相同：EPYC 9B45 128C/256T、7 节点、163,000 tx/block、binary-v1、
zstd level 1、每节点持续 TCP ingest、交易转发关闭。差异只有整块传播从
`6 direct + 1 GossipSub origin` 改成 `6 direct + 0 GossipSub origin`。正式注入窗口为
`03:06:54.983610Z..03:07:54.984511Z`。

| 指标 | round39 结果 | 相对 round38 |
| --- | ---: | ---: |
| 60 秒窗口 leader 提案 | 5,815,000 tx | +378,000 tx |
| leader 提案墙钟 TPS | 96,916.67（实际窗口 96,915.21） | +6.95% |
| 复核后的窗口内提交 | 5,652,000 tx，94,198.59 TPS | round38 同口径为 90,613.40 TPS，+3.96% |
| `n42-stress` 活跃区块时间戳 TPS | 153,026.3 | 仅活跃区块口径，不代替提交 TPS |
| 注入 | 7,435,662 tx，错误 0 | 完成阶段 121,494 tx/s |
| 交易区块 | 37 | 前两块 22k + 88k，之后 35 个 163k 满块 |
| 直推 | 222 dispatch/write/read/recv/ACK | failure/reject/coalesce=0 |
| 认证与能力 | 42/42 promotion，42/42 capability | 注入期 auth reject=0 |

最后一个 leader 提案窗口内区块在 `03:07:54.931Z` 发起 6 个请求，read/recv 在窗口内完成，6 个
ACK 在 `03:07:55.005Z..03:07:55.020Z` 返回。因此按请求归属是 222/222 成功；若机械地在
窗口终点截断 ACK 日志会得到 216，属于边界误差。222 个请求的端到端 ACK 平均 74.01 ms、
p50 69 ms、p95 102 ms、最大 192 ms；write 平均 12.15 ms、p95 21 ms，read 平均
33.54 ms、p95 45 ms。相较 round38 的 ACK 平均 398.61 ms、p95 661 ms，去掉竞争的整块
GossipSub 路径明显缩短了直推尾延迟。

### 精确数据量与通信渠道

leader 提案窗口内 37 个交易块的传播账户：

| 指标 | 60 秒总量 | 平均速率/比例 |
| --- | ---: | ---: |
| binary-v1 payload 原始 | 714,851,400 B | 122.93 B/leader 提案 tx |
| payload zstd 后 | 430,476,793 B | 原始的 60.22%，节省 39.78% |
| execution output | 24,074,198 B | 4.14 B/leader 提案 tx |
| 完整 envelope（单份） | 454,553,988 B | 78.17 B/leader 提案 tx |
| 6 份直推 | 2,727,323,928 B | 45.46 MB/s |
| 整块 GossipSub 起源 | 0 B | 0 MB/s |
| leader 逻辑起源总量 | 2,727,323,928 B | 45.46 MB/s，469.02 B/tx |

如果保留 round38 的一份 GossipSub 起源，本轮相同区块将是 3,181,877,916 B，即
53.03 MB/s。仅直推准确省下 454,553,988 B（7.58 MB/s），leader 源端完整块数据减少
14.29%。虽然 leader 提案量提高 6.95%，逻辑起源总量仍比 round38 低 8.28%，每笔传播字节低
14.24%。这证明整块双通道确实在消耗资源，而不是只存在于配置上。

leader 提案窗口内 `N42_DIRECT_ONLY` 37 次，`gossip_origin_copies=0` 37/37；block GossipSub
fallback、状态同步 fan-out 和 Gov5 block-fetch 请求均为 0。状态同步和 Gov5 协议仍保留，
因为健康链上它们没有数据流量，物理删除不会带来性能收益，却会破坏落后节点恢复能力。

codec 仍保持 zstd：leader serialize 平均 4.16 ms，compress 57.08 ms；216 个在窗口终点
前完成的 follower 样本中 decompress 平均 9.34 ms、deserialize 8.87 ms。压缩仍节省
39.78%，不能因为关闭了 GossipSub 就去掉 zstd。

### 主机资源

资源采样器为 `03:06:39Z..03:07:50Z`，覆盖注入窗口前 15.98 秒和注入的前 55.02 秒，
没有覆盖最后约 4.98 秒。因此上面的逻辑传播字节是完整 leader 提案窗口数据，而主机总量明确标为
71 秒部分覆盖窗口，不能与 round38 的 69 秒完整覆盖总量做无修正的绝对 A/B。

| 采样项 | round39 结果 | 判断 |
| --- | ---: | --- |
| 7 节点注入子窗口平均 CPU | 16,304% = 163.04 cores | 55 个 1 秒样本；峰值 244.72 cores |
| 节点合计 RSS | 子窗口均值 25.23 GiB、峰值 37.08 GiB | 71 秒采样终点 38.15 GiB |
| stress 客户端最大 RSS | 6.30 GiB | 30M 预签名交易文件 |
| 节点线程合计 | 14,893 → 16,791 | 线程过多的问题没有消失 |
| 节点 write 速率 | 注入子窗口 122.11 MiB/s | 71 秒差分 +7.05 GiB |
| `/data` NVMe | 71 秒 +7.40 GiB，111.92 MB/s | 约 1,803 writes/s，device busy 4.50% |
| loopback 单向计数 | 71 秒 +3.712 GiB，56.13 MB/s | RX+TX 机械相加 7.423 GiB |
| UDP 收/发 | +456,133 / +450,637 datagrams | `RcvbufErrors` 增量 0 |

直推专用隔离证明网络重复传播是一个真实损耗：leader 提案 TPS 从 90.6k 提升到 96.9k，直推
p95 ACK 从 661 ms 降到 102 ms，而且 UDP receive-buffer error 在采样段内没有增加。但
它还没有突破该历史口径的 100k：差 185,000 tx/分钟，即 3.08%。本轮已有 37 个交易块，但最初两块
合计只有 110k；若 37 个块都满载则是 6.031M tx、100,516.7 TPS。下一步应优先消除测量开始
时的池填充缺口并稳定出块 cadence，同时继续限制每节点约 2,400 个线程，而不是恢复整块
GossipSub、关闭 zstd 或继续扩大 QUIC 并发。

## 历史一分钟口径复核

在 round42 最终核算时，重新用 block hash 将所有 leader serialize 与 validator-0 的提交
事件逐块关联。validator-0 作为 follower 时记录 `received Decide, committing block`，但它自己
作为 leader 时记录 `block committed!`；只解析前一种消息会周期性漏掉 validator-0 领导的每个
view。合并两种提交事件后的复核结果如下：

| 运行 | leader 提案 TPS | `Decide` 提交 TPS | 窗口末流水线差额 |
| --- | ---: | ---: | ---: |
| round38 | 5,437,000 tx / 90,613.40 | 5,437,000 tx / 90,613.40 | 0 |
| round39 | 5,815,000 tx / 96,915.21 | 5,652,000 tx / 94,198.59 | 163,000 tx（1 满块，边界后提交） |
| round42 | 8,383,000 tx / 139,713.08 | 8,220,000 tx / 136,996.49 | 163,000 tx（1 满块，边界后约 53 ms 提交） |

表内均使用各自实际窗口长度，不再固定除以 60。round42 对 round39 的同口径增益分别是：
leader 提案 +44.16%，严格提交 +45.43%。最终记录采用窗口内实际提交 TPS；窗口边界后的
尾块不计入，但 validator-0 的 self-leader 提交也不能漏计。

## round40--round42：线程与复制深度优化

### round40 基线采样与根因

round40 没有先猜 CPU、内存或 SSD，而是直接按 Linux thread `comm` 对 7 个节点采样。
未限制线程时，空载已有 14,893 个节点线程，负载后为 16,791 个，即每节点约
2,100--2,400 个。单节点空载的主要组成是：`tokio` 516、CPU Rayon 512、RPC Rayon 512、
blocking 256、global Rayon 256、storage 32、pool-validation 相关 19。

根因有两层：

1. 7 个验证者在同一台 256 逻辑 CPU 主机上运行，但每个进程都认为自己独占整机，分别按
   256 线程扩展 runtime/Rayon；这不是 EPYC 核心不足，而是进程级自动配置没有全局预算。
2. 交易池验证另外建立一个 `RuntimeBuilder`。它实际只取 Tokio handle，却连带建立默认的
   CPU/RPC/storage Rayon 池，单节点重复约 525 个线程。

大量线程在 round40 的空载 CPU delta 中为 0，却仍增加调度实体、栈虚拟地址、futex 唤醒、
NUMA 漂移和 LLC/TLB 压力。硬件从 32 线程桌面 CPU 升级为 256 线程 EPYC 后，如果 7 个节点
各自再次乘以 256，反而会把更多时间花在调度和缓存失效上，所以不能按逻辑核数量推算 TPS。

### 程序改动

1. `scripts/testnet.sh` 现在按 `online CPUs / validator count` 自动分配同机测试预算，并把自动
   单节点预算封顶为 32。round42 的每节点配置为 CPU Rayon 32、Tokio worker 16、RPC 4、
   storage 4、global Rayon 32、Tokio blocking 上限 64、Eth blocking 16、prewarm 16。
   `N42_TESTNET_*` 环境变量保留了逐项覆盖能力，独占单节点运行不受固定值束缚。
2. 交易池验证 runtime 显式使用 1 CPU + 1 RPC + 1 storage 辅助 Rayon 线程，并把所有惰性
   worker 类型也设为 1；验证本身的 Tokio 线程不变。
3. 共识执行块数据引入 `SharedBlockData = Arc<Vec<u8>>`。leader 序列化后立即转为共享所有权，
   leader feedback、pending cache、recent cache、后台 import queue 和 direct origin 都只克隆
   `Arc`。只有真的启用 GossipSub fallback 或构造低频状态同步 wire message 时才复制 `Vec`。
4. 新增 `n42_block_data_copy_avoided_bytes_total{site=...}`，分别记录 `recent_cache`、
   `leader_feedback` 和 `direct_origin`，并增加单元测试用 `Arc::ptr_eq` 验证 pending/recent
   两个缓存确实共用同一分配，而不是只改了类型名。

优化后空载线程为 2,872，负载后为 3,093；相对 round40 分别下降 80.72% 和 81.58%。其中
负载后的 CPU Rayon 为 231（每节点主池 32 + 验证辅助 1）、RPC 35、storage 35、global
Rayon 224。仍有约 2,025 个归类为 Tokio runtime 的线程，其中大部分采样期没有工作；这是
后续可继续审计的线程来源，但它没有阻止本轮先取得吞吐和 CPU 效率收益。

### round42 严格一分钟结果

形状与 round39 保持一致：EPYC 9B45 128C/256T、7 节点、163,000 tx/block、binary-v1、
zstd level 1、每节点持续 TCP ingest、交易转发关闭、直推 fanout 6，并通过
`N42_BLOCK_DIRECT_ONLY=1` 关闭完整执行块的 GossipSub 副本。严格注入窗口为
`04:06:25.811864Z..04:07:25.813403Z`，实际墙钟 60.001539 秒。

| 指标 | round42 | 相对 round39 |
| --- | ---: | ---: |
| 严格窗口内实际提交 | 8,220,000 tx | +2,568,000 tx |
| 严格墙钟 TPS | **136,996.49** | **+45.43%** |
| 已提交交易区块 | 52（16k + 54k + 50 个 163k） | round39 同口径 36 块，+16 块 |
| 窗口内 leader 提案 | 8,383,000 tx / 53 块，139,713.08 TPS | round39 同口径 +44.16%；仅最后 1 块在窗口后提交 |
| TCP 注入 | 9,866,000 tx，错误 0 | 完成阶段 151,648 tx/s |
| stress 活跃时间戳指标 | 166,326.5 TPS | 排空后 50 个满块，不代替严格墙钟 TPS |

严格结果按 validator-0 在窗口内的 follower `Decide` 或 self-leader `block committed!` 时间与
leader 的区块 hash/交易数关联，不按提案时间，也不使用 stress 排空后的 `BLOCK_ANALYSIS`。
固定除以 60 秒时是 137,000 TPS；这里使用实际 60.001539 秒墙钟。当前比 100k 高 37.00%，
但距离 1M 仍差 7.30 倍。

### 直推、认证与压缩

| 项目 | 结果 |
| --- | ---: |
| 注入窗口发起的大块直推 | 318 dispatch / 318 ACK / 318 accepted |
| ACK latency | 平均 58.79 ms，p50 53 ms，p95 83 ms，最大 120 ms |
| 整轮交易大块直推 | 396 dispatch/write/read/recv/ACK，396 accepted |
| 负载期 failure/timeout/coalesce/`accepted=false` | 0 |
| validator promotion | 42/42 |
| 大块 dispatch 的能力/认证标志 | `advertised_support=true`、`authenticated_remote=true` |

指标中另有启动认证完成前的 5 个小 envelope 拒绝，共 3,651 B；它们不是交易大块，且启动
阶段 direct-only 会因目标集合不完整而保留 GossipSub fallback。所有约 12.7 MB 的交易大块均
在 10 秒 timeout 内完成，因此不能把启动期的 722/763 B 探测与压测直推失败混为一谈。

严格提交的 52 个交易块数据量：

| 指标 | 60 秒总量 | 速率/比例 |
| --- | ---: | ---: |
| binary-v1 payload 原始 | 1,010,508,984 B | 122.93 B/tx |
| payload zstd 后 | 608,377,212 B | 原始的 60.21%，节省 39.79% |
| execution output | 33,944,155 B | 4.13 B/tx |
| 完整 envelope（单份） | 642,325,579 B | 78.14 B/tx |
| 6 份直推 | 3,853,953,474 B | 64.23 MB/s |
| 完整块 GossipSub 起源 | 0 B | 0 MB/s |
| leader 逻辑起源总量 | 3,853,953,474 B | 468.85 B/tx |

leader serialize 平均 4.36 ms、zstd compress 平均 57.64 ms；270 个 follower 样本的
decompress/deserialization 分别平均 7.12/7.18 ms。zstd 仍以可控 CPU 成本减少 39.79%
payload，因此保持开启。

按严格提交 envelope 计算，7 个节点 recent cache、1 次 leader feedback 和 1 次 direct
origin 共避免 `642,325,579 * 9 = 5,780,930,211 B`（5.38 GiB）大块复制。最终 metrics
抓取还包含窗口尾部提案/排空，三个 site 累计 6,956,186,889 B（6.48 GiB）。这两个数字分别
是“严格已提交集合”和“截至抓取时整个进程运行”的口径。

### CPU、内存、通信与 SSD

| 采样项 | round42 结果 | 判断 |
| --- | ---: | --- |
| 7 节点注入期平均 CPU | 6,337% = 63.37 cores | 59 个 1 秒样本；峰值 79.63 cores |
| 近似 CPU/吞吐效率 | 463 core-s / 百万 committed tx | round39 约 1,731，下降 73.28%；采样边界不完全相同 |
| 5 秒线程热点 | CPU pool 47.55 cores；Tokio 8.00；persistence 1.69 | 优先继续优化执行池和提交 cadence |
| 节点合计 RSS | 注入均值 25.59 GiB、峰值 38.64 GiB | 容量充足，仍要控制每块临时分配 |
| 节点线程 | 2,872 → 3,093 | 较未限制基线负载态下降 81.58% |
| 节点 write bytes | 129.543 秒 +20.57 GiB，162.62 MiB/s | 含注入前后，不是严格 60 秒速率 |
| `/data` NVMe | 同窗口 +21.13 GiB，175.11 MB/s | 3,126 writes/s，device busy 22.38% |
| loopback 单向计数 | 同窗口 +7.020 GiB，58.19 MB/s | RX+TX 机械相加为 14.04 GiB |
| UDP 收/发 | +1,285,565 / +1,278,448 datagrams | `UdpRcvbufErrors` +858 |

这里最重要的硬件结论不是“EPYC 应该比 32 线程 CPU 快 8 倍”。共识的 7 个验证者会重复
执行和保存同一块，leader/commit cadence、单块依赖以及直推完成时间不能按总核心数线性拆分；
原先的线程自动扩展又把硬件优势消耗在调度与 NUMA 上。更快内存改善大块复制和工作集驻留，
但只有去掉复制或增加真正并行的独立工作才会兑现；更快 SSD 也不会在设备忙碌度 22.38% 时
直接变成 TPS。CPU 采样与历史吞吐窗口并非完全同边界，不能构造严格的每交易 CPU 比值；
但此次平均节点 CPU 从 round39 的 163.04 降到 63.37 cores，同时 `Decide` TPS 同口径提高
45.43%，足以证明之前首先缺的是并发治理，而不是更多线程。

当前严格交易块 broadcast-to-commit 平均 103.98 ms（p95 161.7 ms），build-to-commit
平均 903.84 ms；相邻交易块提交间隔平均 1,152.95 ms、p50 1,087 ms、p95 1,930 ms、最大
2,224 ms。下一阶段应优先缩短构建和提交间隔、
审计剩余默认名 Tokio runtime、降低执行池的内存分配/状态写放大，并为 1M TPS 设计多块或
分片级并行；仅继续增加线程、内存容量或 SSD 规格无法补齐 7.30 倍差距。

## round44--round46：提交 cadence、异步侧车与零拷贝解码

### 程序结构调整

round42 以后没有继续增加线程，而是把提交处理拆成更小的责任边界：

1. `new_payload(Valid)` 立即发送 eager-import completion；state diff 通过独立
   `state_diff_ready` channel 稍后到达。辅助 QMDB/Twig diff 不再延长执行验证、FCU 或下一 leader
   的关键路径；晚到 diff 会补上已经提交但暂停的 sidecar gap。
2. staking 先用已经执行验证过的 `has_staking_target` 分类。绝大多数普通转账只推进一个空扫描，
   只有命中目标地址的块才解压/转回历史 JSON；分类未到时按 execution block number 排队，不能越过
   durable watermark，也不在 CommitQC handler 中扫描 163K 笔交易。
3. `CommittedBlock` ring 只保存 `payload_len` 和共享的 `Arc<Vec<u8>>` block envelope。仅在低频
   state-sync 真正发送历史块时才 materialize payload；leader feedback、pending/recent cache、
   本地 redrive 与直推起源共享同一分配。
4. hot follower/leader 路径把已经解出的 view、payload length 直接交给缓存，删除一次完整 bincode
   反序列化；`None` 形式的 eager metadata 刷新也不会抹掉先到达的 state diff。
5. round46 又把 binary-v1 解码从逐交易 `Bytes::copy_from_slice` 改为一个解压 backing allocation
   加 163K 个 `Bytes::slice`。新增指针范围单测确认 transaction body 确实引用原 allocation，并用
   `n42_execution_payload_decode_copy_avoided_bytes_total` 记录少复制的字节。

### round44 严格一分钟记录

配置保持与 round42 相同：7 节点、163K tx/block、binary-v1、zstd、6/6 authenticated direct、
`N42_BLOCK_DIRECT_ONLY=1`，完整执行块不走 GossipSub。窗口为
`05:03:32.024948Z..05:04:32.026268Z`，实际 60.001320 秒。

| 指标 | round44 | 相对 round42 |
| --- | ---: | ---: |
| 严格窗口实际提交 | 9,192,000 tx / 58 个交易块 | round42 为 8,220,000 / 52 块 |
| 严格提交 TPS | **153,196.63** | **+11.83%** |
| 窗口内 leader 提案 | 9,355,000 tx / 59 块，155,913.24 TPS | 最后 163K 在边界后提交 |
| 交易块提交间隔 | avg 1,033.36 / p50 1,026.3 / p95 1,219.3 / max 1,546 ms | round42 为 1,155.23 / 1,096 / 1,450.8 / 2,060 ms |
| TCP 注入 | 10,810,000 tx，错误 0 | 完成阶段 163,525 tx/s |

提交路径把 spike 移走，而不是把它隐藏到另一个同步函数中。以下为严格窗口内所有 validator
的 commit-stage 样本（round42 `n=377`，round44 `n=417`）：

| 阶段 | round42 avg / p95 / max | round44 avg / p95 / max |
| --- | ---: | ---: |
| lineage/metadata | 107.94 / 220 / 345 ms | 122.09 / 207 / 302 ms |
| committed store | 26.46 / 42 / 86 ms | 4.53 / 7 / 40 ms |
| state-tree sidecar | 15.08 / 120 / 194 ms | 0.13 / 1 / 4 ms |
| staking | 23.99 / 173 / 288 ms | 0 / 0 / 0 ms |
| 总提交 service path | 176.27 / 533 / 822 ms | **128.92 / 218 / 348 ms** |

总路径平均下降 26.86%，p95 下降 59.10%，最大值下降 57.66%。lineage/metadata 仍是约 122 ms
的第一瓶颈；builder 本身没有因此变成并行执行器，严格块的 build-start→broadcast 仍平均
788.95 ms、p95 852 ms。

### round44 数据量、直推和资源

严格提交的 58 个交易块：

| 指标 | 60 秒总量 | 速率/比例 |
| --- | ---: | ---: |
| binary-v1 payload 原始 | 1,130,000,167 B | 122.93 B/tx |
| payload zstd 后 | 680,254,475 B | 原始的 60.20%，节省 39.80% |
| execution output | 37,802,843 B | 4.11 B/tx |
| 完整 envelope（单份） | 718,062,016 B | 78.12 B/tx |
| 6 份 direct origin | 4,308,372,096 B | 71.80 MB/s，468.71 B/tx |
| 完整块 GossipSub origin | 0 B | 0 MB/s |

58 次传播均记录 `direct_only_active=true`、`direct_copies=6`、`gossip_origin_copies=0`。压测时间
窗内可记录的大包 ACK 为 348/348 accepted，平均 56.38 ms、p50 53、p95 76、p99 83、最大
97 ms；failure、timeout、coalesce 均为 0。42/42 validator peer promotion 完成，说明本轮收益
不是关闭认证或放弃直推。按 7 个 recent cache、1 个 leader feedback、1 个 direct origin 计算，
严格集合少做 `718,062,016 * 9 = 6,462,558,144 B`（6.02 GiB）热路径内存复制。

round44 的 pidstat 覆盖窗口后 41 秒：7 节点平均 72.28 cores，范围 47.37--80.56 cores；合计
RSS 平均 32.03 GiB、末点 41.13 GiB，线程约 3,069。另一个 52.464 秒系统 checkpoint 只部分
覆盖注入窗口，因此只能作为近似资源量：loopback 单向 91.92 MB/s，NVMe 写入 297.45 MB/s，
device busy 8.70%，UDP 收/发约 8.55K/8.50K datagrams/s，`RcvbufErrors` 仅增加 197。SSD 仍远未
饱和；256 硬件线程也只用了约 72 个 core-equivalent。

### round45 负面对照：不能粗暴把 150 ms 设为零

局部 stage 数据显示 metadata wait 占提交时间后，曾把默认
`N42_EAGER_COMMIT_METADATA_WAIT_MS` 从 150 改成 0 做单变量一分钟 A/B。结果不是提高，而是：

| 指标 | round44，150 ms grace | round45，0 ms grace |
| --- | ---: | ---: |
| 严格提交 TPS | 153,196.63 | 144,495.29（-5.68%） |
| async FCU 状态日志 | 710 Valid / 0 Syncing | 403 Valid / **313 Syncing** |
| build-start→broadcast p95 | 852 ms | 933 ms |
| 交易块提交间隔 avg | 1,033.36 ms | 1,079.45 ms |
| commit service total avg / p95 | 128.92 / 218 ms | 36.99 / 54 ms |

这组数据说明“局部 commit handler 更快”不等于链 cadence 更快。零等待使 FCU 大量跑在匹配的
`new_payload` 前面，丢失 eager validation 与下一块构建的流水线重叠，回退解码也与执行池竞争。
因此默认已经恢复为 150 ms；环境变量仍可实验，但在提交状态机重构前不能把 0 当优化默认值。

### round46 零拷贝 follower 解码复测

round46 保留 round44 的 150 ms grace，只改变 binary-v1 owned decode。窗口为
`05:29:36.618086Z..05:30:36.620539Z`，实际 60.002453 秒。

| 指标 | 结果 |
| --- | ---: |
| 严格提交 | 9,145,000 tx / 58 交易块，**152,410.44 TPS** |
| 与 round44 | -0.51%；两轮块数相同，差额来自起始 partial block（47K tx） |
| 提交间隔 | avg 1,043.68 / p50 1,040 / p95 1,198 / max 1,591 ms |
| commit service total | avg 124.46 / p95 210 / max 312 ms |
| async FCU | 712 Valid / 0 Syncing |
| 大包直推 | 342/342 ACK；平均 55.96、p95 75、最大 99 ms；failure/coalesce=0 |
| follower decode 少复制 | metrics 抓取合计 8,911,106,007 B（8.30 GiB） |

严格传播集合仍是 58 次 6/6 direct、0 GossipSub origin；单份 envelope 714,882,219 B，6 份逻辑
origin 为 4,289,293,314 B。零拷贝没有制造显著 TPS 回归，又删掉了可直接计数的 8.30 GiB
瞬时 transaction-body memcpy，因此保留。

## EPYC 9B45 硬件匹配与 1M 架构路线

当前主机是 1 socket、128 physical cores / 256 SMT threads、16 个 32 MiB L3 domain（512 MiB
总 L3）、约 140 GiB RAM。固件/内核只暴露 1 个 NUMA node，所以简单 `numactl --membind`
不能制造真正的内存局部性；也不能因为看到 256 threads 就让 7 个进程各建 256 线程池。

当前 3.5B gas limit 在 21K transfer 下理论上限 166,666 tx/block，实测满块 163K。要达到
1M TPS 必须每秒提交约 6.13 个满块，即约 **163 ms/block**；round44 实际约 1,033 ms/block，
相差 6.34 倍，与端到端 6.53 倍 TPS 缺口一致。若改成单块 1M transfer，则需要约 21B gas，
并产生约 78 MB 的单份压缩 envelope，已超过当前 32 MiB direct frame 上限。这不是换 SSD 或
继续加 Tokio thread 能解决的量级。

建议按以下顺序改架构：

1. **提交异步状态机（P0）**：把 `CommitQC observed`、`ExecutionReady`、`Finalized` 做成显式的
   ordered pending-commit 状态。CommitQC 到达只持久化 QC/hash/共享 envelope 后返回；匹配的
   eager `new_payload(Valid)` completion 再驱动 FCU、execution head、sidecar/staking watermark 和
   next build。这样才能删除 150 ms 同步 grace 而不重现 round45 的 313 次 Syncing FCU。必须为
   crash recovery、prepared ancestor lineage 和 out-of-order completion 补状态机测试。
2. **sender-sharded 并行构建/执行（P0）**：当前 full-block transaction packing 约 429--441 ms，
   其中 EVM 317--324 ms、pool/iterator 约 111--117 ms；finish 约 187 ms，assemble/root 约
   166--170 ms，zstd 约 57 ms。`../reth/crates/ethereum/payload/src/lib.rs` 仍以串行 while-loop
   选择和执行交易。先批量 drain txpool、预恢复 sender，按 sender/访问集分 shard 并行执行，
   冲突检测后确定性 merge；receipts/bloom、transaction root、state diff 与压缩并行流水。目标不是
   让一个串行循环拥有更多 worker，而是让 16 个 L3 domain 都有独立工作。
3. **多 lane 而非无限增大单块（P1）**：在一个共识块内放 6--8 个 sender shard/lane，各自有
   有序输入和执行摘要，最后确定性合并 state/receipt root；或允许连续 prepared blocks 在版本化
   状态上形成受控 pipeline。简单 transfer 目标至少 6.5 倍并行度，复杂合约需按冲突率降级，
   不能承诺线性加速。
4. **chunked direct transfer（P1）**：保留验证者认证和 zstd，把单次 request-response 大 `Vec`
   改成 manifest + 2--4 MiB chunks、共享 buffer pool、per-peer 长寿命 QUIC data lane、ACK bitmap
   与显式 credit/backpressure。按当前 78.12 B/tx，1M TPS 已是约 78 MB/s 单份 block payload、
   约 469 MB/s 的六份 leader logical origin，还不含协议头和接收端；继续使用 32 MiB 单帧会先撞墙。
5. **CCD-aware 调度只做受控 A/B（P1）**：同机 7 节点可实验每节点两个 L3 domain（16 physical /
   32 SMT），预留两个 domain 给 ingest/OS，并优先 physical core；同时记录 leader/follower p95、
   LLC miss、CPU migration。不要默认硬绑，因为 leader 轮转且当前平均只用 72 cores，错误 cpuset
   可能把 leader 限制在 16 physical cores。生产若是每台 EPYC 只跑一个 validator，应让并行执行器
   使用全部 16 L3 domain，而不是照搬同机 7 进程预算。
6. **内存生命周期（P1）**：round46 已去掉 envelope 和 follower transaction-body 两层大复制；
   下一步针对 txpool→builder→execution cache 使用 slab/arena、`Bytes` slice 和批量回收，逐项记录
   allocation bytes、RSS high-water、cache retention。round44/46 末端 7 节点仍约 41 GiB RSS，
   说明容量够用但临时对象和 pool retention 仍大。

短期 15--20 万 TPS 可以继续靠状态机、builder batch 和尾延迟治理；1M 必须同时完成并行执行、
163 ms 级多 lane cadence 和 chunked authenticated direct transfer。zstd、状态同步恢复协议与认证
映射都应保留，它们已有数据证明不是当前应删除的功能。

## round47--round52：五项架构改造的实现状态

本轮把上述路线写进程序，而不是把 163 ms 当成已经达到的结果。当前状态如下：

| 项目 | 已实现 | 仍受限的部分 |
| --- | --- | --- |
| 异步提交状态机 | `Committed`、`ExecutionReady`、`Finalized` 三态；按 view 排序；同一时间最多一个 canonical FCU；150 ms 仅作为异步 deadline，不再阻塞 consensus loop；生产默认异步开启 | crash recovery 仍从已有共识/执行 lineage 恢复，不持久化进程内 pending map |
| sender-sharded drain | pending pool 做 immutable snapshot；8-lane 无锁 local sender map + reduce；按 sender 并行 nonce/hash sort/fee suffix；`tip → sender → hash` 确定性 merge；同 sender/同 tip run 批量释放；invalid tx 截断同 sender suffix | reth canonical payload builder 的逐交易 EVM loop 仍是串行，不能把独立的 Block-STM engine 冒充已经接管生产 execution |
| 6--8 execution lanes | `N42_EXECUTION_LANES` 统一限制在 6--8，默认 8；同时约束 txpool preparation 和 Block-STM worker，线程名可采样 | round52 的 canonical EVM 没有调用 Block-STM，因此 8 lanes 本轮只真实作用于 txpool snapshot；163 ms cadence 尚未达到 |
| QUIC chunk data lane | 协议升级为 `/n42/block-direct/3`；持久 QUIC peer connection 上，每块一个 request substream，先发 manifest，再连续写 2--4 MiB chunks，最后一次 ACK；chunk 直接引用原 `Arc`；256 MiB 重建上限、digest、validator auth、per-peer FIFO/credit 和有界 retry 均保留 | 应用 substream 生命周期是一块，不跨多个块永久复用；“长寿命”指 QUIC peer connection 与单块内连续 chunk stream，不应写成永久单流 |
| CCD/L3 cpuset | 默认 `N42_CPUSET_AB_MODE=off`；`l3` treatment 才从 sysfs/allowed CPU 构造互不重叠 L3 domain，输出 `cpu-affinity.tsv` 后用 `taskset`；支持 topology-only probe | 没有 A/B 数据前不会把 affinity 设为生产默认 |

状态机覆盖两条关键回归测试：CommitQC 先到时不能跳过 `ExecutionReady`，以及多个 ready child 即使
completion 乱序也必须按 commit view 顺序 Finalized。FCU `Valid/Syncing/Accepted/Invalid/error`、三态
transition、deadline fallback 都有独立 metric。显式 `N42_ASYNC_FINALIZE_FCU=0` 仍是回滚开关。

sender-sharded preparation 的 round50 metrics 证明 7 节点都实际进入了 8-lane path：窗口内 61 次
snapshot，累计约 3,352 ms，即平均约 **54.95 ms/build**。这个数与 leader 日志的 pool overhead
52.16 ms 一致；它解决确定性和线程上限，但没有消掉这 52 ms，也没有并行化随后约 235 ms 的
canonical EVM。`n42_parallel_evm_blocks_total` 没有样本，因此当前不能声称“并行 execution 已上线”。
把 Block-STM 输出接进 reth 前必须补齐 system calls、custom precompile、invalid-descendant 语义、
receipts/bloom、requests、state/bundle/root 和 payload cache 的逐字节差分验证；否则 TPS 会提高但链语义
不再等价。本轮选择保留这个 correctness gate。

### round50 严格一分钟结果

正式窗口：`07:15:27.880943Z..07:16:27.882787Z`，60.001845 秒。形状为 7 节点同机、
EPYC 9B45、scheduler-managed control（不绑 cpuset）、163,000 tx/block、8 execution lanes、
sender-sharded drain、异步 FCU、direct-only、4 MiB chunk、zstd 开启、mobile packet benchmark
side channel 关闭。RPC 在窗口边界取块高并逐块计数，不用 leader proposal 或 stress 活跃区间代替提交。

| 指标 | round50 |
| --- | ---: |
| 严格提交 | **9,112,456 tx / 58 blocks / 60.001845 s** |
| 严格 TPS | **151,869.60** |
| 与 round44 记录 | -0.87%；异步状态机没有制造吞吐回归，但本轮不是单变量 A/B |
| 区块构成 | 55 个 163K 满块 + 147,456 tx partial + 2 个空块 |
| inter-block commit | avg 1,026.91 / p50 1,042 / p95 1,277 / max 1,504 ms（406 个 validator 样本） |
| async FCU | 406 `Valid` / 0 `Syncing` / 0 other；406 Finalized；窗口末 pending=0 |
| sender-sharded snapshot | 61 次，平均 54.95 ms；各节点 lane gauge=8 |
| mobile packet | 0 条相关日志 |

1M TPS 在 163K 满块下需要约 6.13 block/s，即约 163 ms/block。round50 严格块率只有
0.967 block/s；日志 cadence 比目标慢 **6.30 倍**，TPS 距 1M 为 **6.59 倍**。因此“8 lanes”和
“target cadence=163 ms”目前是受控配置与目标，不是达标声明。

55 个满块 leader 样本进一步定位了剩余串行链：

| leader 阶段 | 平均 | p95 | 判断 |
| --- | ---: | ---: | --- |
| transaction packing | 287.40 ms | 388 ms | 已超过 163 ms 总预算 |
| 其中 canonical EVM | 234.71 ms | 333 ms | 首要并行执行接入点 |
| pool overhead | 52.16 ms | 59 ms | sender snapshot/merge 仍需降分配与 sort 成本 |
| finish/assemble 主体 | 约 190.54 / 169.55 ms | 221 / 181 ms | 单是 assemble 已接近或超过总预算 |
| compact execution serialize | 41.20 ms | 43 ms | 可与 receipts/root/envelope pipeline 重叠 |
| zstd compress | 33.18 ms | 36 ms | 应流水化，不应删除 |
| build start → broadcast | 738.42 ms | 965 ms | 不是共识投票或 FCU 能单独修好的量级 |

follower 满块 330 个样本：block-data 到 accepted 平均 175.84 ms，其中 `new_payload` 79.86 ms、
zstd decompress 7.37 ms、binary deserialize 1.91 ms。网络到达、cache inject、调度等待占剩余时间。

### round50 数据量、QUIC 与主机资源

本轮 transport counter 从 gate 前快照到 end marker 后快照，包含一个边界传播块，所以 ACK 是
59 blocks × 6 peers，而严格 RPC 集合是 58 blocks。边界差异保留，不把 transport counter 强行裁成
提交集合。

| 指标 | 60 秒增量 |
| --- | ---: |
| direct logical sent / received | 5,022,685,884 / 5,022,683,607 B，即各 83.709 MB/s |
| chunk payload sent / received | 4,990,998,120 / 4,990,998,120 B |
| chunked transfers | 336 send / 336 receive；平均 14.854 MB/transfer |
| chunks | 1,338 send / 1,338 receive，平均 3.98 chunks/large transfer |
| ACK | 354，平均 75.788 ms |
| failure / retry / queue / overflow | 全部 0 |
| digest mismatch / non-validator auth reject | 全部 0 |
| loopback | 单向 8,014,392,152 B，133.569 MB/s |
| `/data` NVMe | 写 21,601,562,112 B，360.015 MB/s；device busy 约 8.63% |
| 7 节点 CPU | 平均 67.99 cores、1 秒峰值 83.27 cores |
| 7 节点 RSS | 平均 25.738 GiB、峰值 40.757 GiB |

`eno2np1` 同期另有 5.297 GB RX；本地 7 节点直推走 loopback，无法从这些快照证明该物理口流量
属于本轮，所以不计入链通信账单。NVMe busy 不到 9%，CPU 也没有耗尽 128 个 physical cores；更快
SSD/内存不会把当前 151.9K 自动放大到 1M，关键仍是单块串行依赖和跨阶段流水。

55 个满块的 payload 原始数据合计 1,102,092,465 B，zstd 后 786,269,562 B，节省 **28.66%**；
完整 envelope 平均 14,965,374 B，即 91.81 B/tx。按这个 level-1/交易样本外推，1M TPS 单份是
约 91.8 MB/s，六份 leader origin 约 550.9 MB/s。历史约 78.1 B/tx 的压缩形状对应用户提出的
约 78 MB/s；两者都说明 32 MiB 单帧不可继续扩容。新协议用 4 MiB chunks 已在 336 次大传输中
零失败验证；zstd 保持开启，生产代码默认 level 3，资格测试必须把实际 level 写进 `config.tsv`。

一分钟复现脚本为 `scripts/qualify-1m-tps.sh`。它等待 stress ready gate 后才同时取 metrics、RPC block
height、`/proc`、pidstat 与 iostat 快照；duration 从 Rust 的 start marker 开始，不再把 30M 预签名
文件装载时间混进 60 秒。新增的 config/summary 字段会记录 zstd level、lanes、sharded drain、async
FCU、chunk size、三态 transition、FCU outcome、sender-drain 平均时间和 parallel-EVM 实际块数。

### round51--round52：用分段数据继续压 sender drain

round50 的 54.95 ms 是“全 pool 并行 sort，再逐交易 heap merge”的基线。round51 改为每个 Rayon
lane 先建立自己的 sender map，无共享 sender lock；reduce 后按 sender 并行 nonce/hash sort 和 fee
suffix 截断，最后仍以同一个 `(effective tip, sender, hash)` total order merge。严格结果为
8,953,552 tx / 60.001038 s = 149,223.28 TPS，说明只做分组不足以提高 cadence。

round51 新增 phase histogram 后，61 次 snapshot 的平均耗时为：group 7.080 ms、prepare 3.126 ms、
merge **42.537 ms**、total 52.743 ms。真正热点不是分组或 sort，而是 163K 笔交易逐笔在约 5,000
sender head 上 pop/push heap。

round52 保持 total order 不变，只把“同 sender、同 effective tip”的连续 nonce run 一次释放。这个
变换是等价的：heap key 中 sender 先于 hash；当前 head 能胜出时，同 sender/同 tip 的下一 nonce 仍
胜过刚才所有 loser，直到 tip 改变才需要重新入 heap。新增 equal-tip 与 variable-tip 顺序测试保护
这个条件。

| 指标 | round50 全局 sort | round51 lane-local | round52 sender-run batch |
| --- | ---: | ---: | ---: |
| 严格提交 TPS | 151,869.60 | 149,223.28 | **156,499.78** |
| committed blocks / tx | 58 / 9,112,456 | 57 / 8,953,552 | 60 / 9,390,144 |
| sender drain | 54.95 ms | 52.743 ms | **19.210 ms** |
| group / prepare / merge | 未分段 | 7.080 / 3.126 / 42.537 ms | 7.823 / 3.371 / **8.016 ms** |
| build start → broadcast | 738.42 ms | 731.04 ms | **701.60 ms** |
| commit cadence | 1,026.91 ms | 1,031.88 ms | **989.23 ms** |

round52 的 62 次 snapshot 只执行 4,201 次 heap run，并在 run 内批量释放 24,915,191 个后继。
相对 round51，merge 降 81.2%、total drain 降 63.6%、严格 TPS 提高 4.88%；相对 round50 TPS
提高 3.05%，相对此前 round44 严格记录提高 2.16%。这证明减少算法操作数有效，但也量化了上限：
drain 已只剩约 19 ms，canonical EVM/finish/assemble 和流水 cadence 仍占绝大多数。

round52 的精确窗口为 `07:43:38.764634Z..07:44:38.765638Z`：57 个 163K 满块、一个
99,144 tx partial 和两个空块。420 个 validator 状态机样本全部完成
`Committed → ExecutionReady → Finalized`，FCU 为 420 Valid / 0 Syncing / 0 other，retryable=0，
窗口结束 pending=0。canonical parallel-EVM block 仍为 0。

| round52 资源/通信 | 60 秒结果 |
| --- | ---: |
| direct logical send / receive | 各 5,175,014,562 B，86.249 MB/s |
| chunk transfer | 348 large transfers / 1,380 chunks；平均 14.829 MB/transfer |
| ACK | 360，平均 76.617 ms |
| failure / queue / retry / digest / auth | 全部 0 |
| loopback | 单向 8,328,248,946 B，138.802 MB/s |
| 7 节点 CPU | 平均 69.49 cores，峰值 92.97 cores |
| 7 节点 RSS | 平均 26.233 GiB，峰值 40.399 GiB |
| `/data` NVMe | 写 16,832,316,416 B，280.534 MB/s；busy 7.59% |

满块 leader 样本仍显示 packing 301.70 ms（EVM 244.02 ms、pool/other 57.14 ms），finish/assemble
约 185.47/165.86 ms，zstd 32.38 ms，build→broadcast 701.60 ms。inter-block cadence 平均
989.23 ms，仍是 163 ms 目标的 6.07 倍；端到端 TPS 距 1M 为 6.39 倍。因此下一步不能继续只抠
19 ms drain，而应把 canonical execution、receipt/state merge 和 assemble 接进可验证的并行 lane。

### gov5 GossipSub MsgID 作用域修正

协议审计补充确认：gov5 的 `common/hash.Hash` 实际使用 KeccakState，不是其旧注释所写的
SHA-256；精确算法为 `Keccak256(genesis_hash || topic || data)[:20]`。N42-26 原来计算
`Keccak256(data || topic)`，虽不妨碍 GossipSub 节点互通，却没有 chain/genesis 作用域。
由于 seen-cache 会跨 topic、在正式应用验证前登记 MsgID，这会留下跨 topic 重放预污染窗口。

现已把 native、gov5 observer 和 gov5 participant 三种 swarm 的真实 genesis 显式绑定进
message-ID closure，并加入 5 个由 gov5 Go 哈希函数生成的固定向量，以及同 payload 在不同
topic/genesis 下 ID 必须不同的性质测试。此处顺序以向量和 Go 源码为准，是
`genesis || topic || data`，不是 `topic || genesis || data`；测试失败信息也明确禁止按错误注释
改回 SHA-256。

### 2026-08-28 同步审计：完成队列与直推摘要去重

本轮只做静态审计和轻量测试，没有运行重负载压测。发现并修复两处可证明的额外工作：

1. CommitQC 为提前取得执行 lineage，会 drain eager-import 完成通道；旧实现克隆可能很大的
   `StateDiff` 后再 `try_send` 回同一个 256 深度通道。并发 producer 抢满通道时完成事件会丢失，
   同步 FCU rescue 直接 drain 时还会跳过 H2 的认证后置动作。现在 state diff 只移动一次，轻量字段
   就地完成投票/抓取退休和三态推进，所有 ready lifecycle 最后只做一次有序 drive，不再回灌通道。
2. QUIC v3 的不可变 `Arc<Vec<u8>>` 原来在 leader 端每个 peer 由 service 和 codec 各计算一次
   BLAKE3，receiver codec 验证后 service 又计算一次。6-peer、15 MiB envelope 对应 leader 每块
   180 MiB 总摘要扫描，其中相对一次必要计算的重复扫描为 165 MiB。现在以 Weak 身份缓存同一 Arc
   的 transfer ID，跨 fanout/重试只计算一次；receiver 保留 codec 的完整摘要校验，之后只重查
   manifest 结构。

新增观测量：`n42_block_direct_digest_computations_total`、
`n42_block_direct_digest_computed_bytes_total`、
`n42_block_direct_digest_cache_hits_total` 和
`n42_block_direct_digest_rehash_avoided_bytes_total{site}`。上述字节数是按代码路径计算的静态值，
不是 TPS 提升声明；下一轮一分钟 A/B 应同时比较 hash bytes、swarm poll 延迟、build→broadcast 和
commit cadence。

## 验收结果

round38 对原验收条件的结果：

- 通过：`N42_PAYLOAD_ZSTD=1`，大块 envelope 约 12.7 MB，没有 20/45 MB 越限；
- 通过：42 个远端映射和 capability 全部完成，auth reject 为 0；
- 通过：完整 ACK 204/204，p95 661 ms，无持续排队；
- 已量化：`UdpRcvbufErrors` +9,781，而不是引用累计值；
- round38 部分通过：leader 提案和严格提交窗口均为 90,613，未超过 100k；
  活跃链指标 151k 不冒充墙钟 TPS。
- round42 通过：严格提交增至 136,996 TPS；leader 提案同口径为 139,713 TPS。
- round44 通过：sidecar/staking 移出提交热路径后，严格提交增至当时记录 153,197 TPS；
  direct-only、认证和 zstd 均保持正常。
- round45 否决：0 ms eager grace 使 313 次 FCU 返回 Syncing，TPS 回退 5.68%，默认恢复 150 ms。
- round46 通过：严格 152,410 TPS、712 次 FCU 全部 Valid，零拷贝 follower decode 少复制
  8.30 GiB，直推大包 342/342 ACK。
- round50 通过：异步提交状态机与 QUIC v3 chunk transport 在严格 60 秒窗口稳定工作；406 次 FCU
  全部 Valid，336 次大传输、1,338 个 chunks 无失败/重试/认证/摘要错误；严格 151,870 TPS。
- round50 未达成：canonical parallel execution 与 163 ms cadence。当前 1,027 ms cadence 和
  `n42_parallel_evm_blocks_total=0` 是继续接 reth execution/receipt/state merge 的硬证据。
- round52 通过：equal-tip sender-run batching 将 drain 52.74→19.21 ms，严格 TPS 达到新记录
  156,499.78；状态机 420/420 Valid，直推 348 次大传输、1,380 chunks，所有错误为 0。
- round52 仍未达成：canonical parallel execution 与 163 ms cadence；当前 989 ms/block，
  `n42_parallel_evm_blocks_total=0`。

原始数据目录：

- `/data/n42-bench-artifacts-20260823/round29-json-control`
- `/data/n42-bench-artifacts-20260823/round30-binary-v1`
- `/data/n42-bench-artifacts-20260823/round31-binary-nozstd`
- `/data/n42-bench-artifacts-20260823/round32-payload-nozstd`
- `/data/n42-bench-artifacts-20260824/round33-quic-window-isolation`
- `/data/n42-bench-artifacts-20260824/round34-quic-udp-buffer-fix`
- `/data/n42-bench-direct-stream4096-20260824`
- `/data/n42-bench-direct-scheduler-fix-20260824`
- `/data/n42-bench-direct-libp2p-diag2-20260824`
- `/data/n42-bench-direct-connection-limit-fix-diag-20260824`
- `/data/n42-bench-artifacts-20260824/round38-zstd-direct-fixed-60s`
- `/data/n42-bench-artifacts-20260824/round39-direct-only-smoke`
- `/data/n42-bench-artifacts-20260824/round39-direct-only-60s`
- `/data/n42-bench-artifacts-20260824/round40-thread-profile`
- `/data/n42-bench-artifacts-20260824/round41-runtime-copy-smoke`
- `/data/n42-bench-artifacts-20260824/round42-runtime-copy-60s`
- `/data/n42-bench-artifacts-20260824/round44-cadence-direct-only-60s`
- `/data/n42-bench-artifacts-20260824/round45-zero-commit-wait-direct-only-60s`
- `/data/n42-bench-artifacts-20260824/round46-zero-copy-direct-only-60s`
- `/data/n42-bench-artifacts-20260824/round50-async-mobileoff-exact-60s-valid`
- `/data/n42-bench-artifacts-20260824/round51-lane-local-sender-exact-60s-valid`
- `/data/n42-bench-artifacts-20260824/round52-sender-run-batch-exact-60s`
