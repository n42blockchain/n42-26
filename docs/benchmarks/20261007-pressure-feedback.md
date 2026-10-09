# 七节点压力测试与反馈（2026-10-07）

## 运行口径

- 七个验证者、七份独立 EL（E7），受信任预恢复的 EIP-2718 TCP 入队。
  开启现有 EVM 等价转账快速路径、并行构块与 QMDB 原生账户读。
  这不是参考 `../n42-rs` 的七验证者共享一份 EL（E1）与 Ed25519 认证帧口径。
- 每块上限 220,000 笔，发送端目标 400,000 TPS；发送目标和入队数均不作为链容量。
- 并行导入关闭，compact block 关闭；Reth persistence threshold、state masking blocks、
  memory block buffer target 均为 0，沿用本次测试模板。生产部署参数需要另测。
- 正式 A1/B/A2 使用同一组冻结二进制、预签名文件与配置。
  A 为 import single flight 关闭，B 显式开启；该开关只协调一个适配器中的同块请求。
- 预热排除在性能比较之外。每轮负载 60 秒，吞吐分母使用七节点审计的实际墙钟窗口，
  包含边界捕获开销。日志按原生区块时间戳计算的 BLOCK_ANALYSIS 不作为性能结论。
- 原始 2,000/8,000 ms 共识超时的首轮在满块下换主、停顿，且 node2 没有观测到
  QMDB provider/account read 进展，审计失败；不记为合格容量结果。
  本组诊断配置为 6,000/24,000 ms，只修改测试模板，未更改生产默认值。
- 逐笔核验所有测量窗口内的七节点回执、区块哈希、QC 与 QMDB 根。
  审计期间产生的后续空块与指标不计入负载阶段均值；指标按 ingest Unix ns 边界过滤。

## 可复现材料

- `.artifacts/pressure-20261006/source-manifest.json` 保存基线完整源码 SHA256、工作区差异
  与依赖。基线包含未提交的 Reth 2.7 / QMDB / 并行执行工作，并非纯 HEAD。
- 适配后的 Reth `3452d7d0217095446b0f2632a283e314951da273`；上游 v2.7.0
  `3d592ece6de8c4559987416a544fc215fd6d6921`。六个 Reth patch 的源码检查通过。
- 基线 n42-node SHA256：
  `ff71d95e905cd90784f435b8ce5f9c3cbcc7ce4df73da96ddeca76bb5faff824`。
- `.artifacts/pressure-20261007/manifest.json`、`claim.jsonl` 保存拓扑、配置与独占记录。
  各轮 `fleet/result-*/qualification/summary.tsv`、`h2-audit.json`、`timeline.json`
  与 `metrics-*-window-summary.json` 保存审计和阶段数据。

## 审计结果

| 轮次 | 门控 | 成功交易 | 墙钟秒 | 审计 TPS |
| --- | --- | ---: | ---: | ---: |
| warmup | 关闭，预热排除 | 2,392,512 | 61.925048 | 38,635.61 |
| a1 | 关闭 | 2,367,936 | 62.392445 | 37,952.29 |
| b | 开启 | 2,307,680 | 62.530732 | 36,904.73 |
| a2 | 关闭 | 2,299,488 | 62.602525 | 36,731.55 |

各轮七节点 QC、原生区块/回执根与 QMDB 根核验均通过；失败交易、QMDB mismatch/read/provider/unavailable 错误均为零，账户读及 pinned provider 有进展。

A 的两端范围为 36,731.55–37,952.29 TPS，均值 37,341.92。
B 相对该均值为 -1.17%。只有一组 A/B/A，没有重复试验的置信区间，
不能把这个差值归因为门控本身；本组未建立门控吞吐收益，默认继续关闭。

B 的七节点均出现门控 wait 指标，确认配置生效；六节点窗口内平均等待约 0.004–0.005 ms，
node3 为 5.27 ms。node3 的 contention counter 首末快照均为 1、差分为 0；
其他节点该 counter 序列缺失。不能把缺失序列当零，或从 2 秒快照中精确定位首个快照前的事件。

| 阶段（ms，中位数 / P90） | A1 | B | A2 |
| --- | ---: | ---: | ---: |
| 交易打包 | 205 / 223 | 208 / 224 | 209 / 222 |
| payload built | 819 / 875 | 823 / 877 | 825 / 857 |
| payload built → Engine 接收 | 384 / 404 | 376 / 408 | 386 / 403 |
| 构块起点 → 广播（累计） | 2467 / 2633 | 2556 / 2640 | 2539 / 2696 |
| 领导者规范化后验证 | 963 / 983 | 971 / 999 | 964 / 981 |
| Gov5 wire 编码 | 155 / 158 | 155 / 160 | 156 / 160 |
| execution bridge payload 压缩 | 79 / 82 | 82 / 84 | 80 / 82 |
| 跟随导入 | 1362.5 / 1483 | 1380.5 / 1507 | 1417 / 1547 |

## 阶段结论和改进

以下阶段按审计中的非空块哈希关联日志。构块到广播是累计时间；其他指标可能重叠，
不能将各中位数简单相加。`builder_finish_ms` 没有日志样本，不能据此估计封块完成或持久化等待。

A1 的 QMDB prepare/candidate compute 每节点平均约 17–33 ms，prepare/commit 锁等待接近零，
commit 计算约 18–24 ms。payload_resolve 平均约 758–1,130 ms，包含 pending payload 等待及转换，
不等于共识 seal。指标有 2 秒采样间隔，sum/count 差分覆盖首末快照之间；与审计窗口不完全同长。

A1 13 个非空块全部出现领导者 `N42_PAYLOAD_CACHE_HIT`，说明当前领导者构块结果复用已经生效；
78 条跟随导入日志均为 `compact_injected=false`。这仍是 E7 口径，不能证明跨进程只执行一次。
现有通用并行 EVM 输出指标不能自动代表此测试的转账构块及原生输出冻结路径。

A1 的 pidstat 60 秒样本显示，每节点平均 CPU 837–968%，峰值 RSS 约 4.8–5.1 GiB；
采样的 major fault 全部为零。runner 设置 `RAYON_NUM_THREADS=8`、`TOKIO_WORKER_THREADS=8`，
本机为 256 个逻辑 CPU；与参考历史窗口的资源配置不同。

### 本轮网络修复

日志中的 4.9–26 MB Gov5 区块已保留并排队直传，但仍被 Snappy 压缩后尝试发布 gossip。
接收端明确拒绝解压后大于 1 MiB 的区块；发送端也出现 `MessageTooLarge`。

改动将发送端 gossip 资格与既有接收端 1 MiB 上限对齐：先保留区块、排队直传，然后对超限
区块跳过压缩和 gossip，计入 `n42_gov5_block_gossip_skipped_total{reason="oversized"}`。
直传、按哈希补取与原有块验证照常执行。边界测试覆盖上限前、上限、上限后及 usize::MAX。

注意：timeline 的 `compression_ms` 是 execution bridge 的 payload 压缩，
不是本次跳过的网络服务 Snappy 压缩。不能把其约 80 ms 直接视作该修复的收益。
候选快照仅修改 `crates/n42-network/src/service.rs`，详见
`.artifacts/pressure-20261007-gossip/source-delta.json`。

### 下一批优先顺序

1. 细分领导者 Engine 转换、交易恢复、验证和后处理；当前
   `crates/n42-node/src/engine_validator.rs` 的 Gov5 分支先对 payload clone 调用
   `try_into_block`，再交给上游 `convert_payload_to_block`，还会克隆整块 body。
   优先消除可证明冗余的解码与复制，保留上游校验、原生字段绑定和所有失败路径。
2. 为现有账户读取和输出组装补齐实际转账路径的分段指标，区分 QMDB 视图查找、
   交易排序/打包、收益归集、输出冻结与 root/字段发布；缺失的 finish 数据不能按零处理。
3. 继续按参考架构推进共享 EL 的一次执行、换主复用、认证 Ed25519 帧与有界背压，
   另建 E1/原生帧实测口径；此处的 trusted TCP 结果不用于宣称已追平历史 3M TPS。


## 验证状态

- Python 运行器 13 项、指标采集/汇总 7 项、时间线关联 4 项测试通过。
- 冻结基线与网络候选的 release 构建均通过；候选网络库 218 项测试通过、0 失败，
  1 项已有 Snappy 测量用例忽略。新上限边界测试及大块按哈希传输回归通过。
- 网络修复提交 `c6feb81`。候选 n42-node SHA256：
  `fdb9ce4a6d55de380169a0c58ae470c7856f1a185adc99fc074e757e8f8c5de2`；
  n42-stress、Keccak helper、commit verifier 均与 A/B/A 使用的二进制相同。
- 七节点候选正确性验证通过：2,307,680 笔成功交易、0 失败，实际审计窗口
  62.317151 秒；QC、原生区块、回执根与 QMDB 根一致，QMDB 读/提供者/不匹配错误均为 0。
  后续 100 个共同区块的根/哈希核验也通过。该轮 37,031.22 TPS 发生在重编译后的首轮，
  排除在性能对照之外，不据此宣称网络修复带来吞吐提升。
- 候选窗口 30 次指标快照无抓取错误；七节点都观测到超大 gossip 跳过计数。
  整轮日志中超大 Gov5 gossip 拒收与 `MessageTooLarge` 发布警告均为 0。
  计数序列在第一次跳过时创建；首末快照差分不包含首个快照前的增量，不能直接相加作为轮内总数。
- `.artifacts/pressure-20261007-gossip/status.json`、`binary-sha256.txt`、`claim.jsonl`
  和 `fleet/result-candidate-warmup/qualification/` 保存验证材料。
  原 A/B/A 与候选测试都正常退出、独占记录通过并释放 claim；候选源码最终复核仅一份文件有差异。
- 本次没有进行故障恢复、掉电重启或 E1 / Ed25519 认证帧容量验证。
