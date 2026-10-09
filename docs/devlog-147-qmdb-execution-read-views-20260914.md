# QMDB 精确区块执行读取：接线、回归与成本

日期：2026-09-14。接续 [devlog-146](devlog-146-four-node-1m-upstream-audit-20260914.md)。
**4 节点 1M TPS 目标仍在进行，尚未实测达标。** 本批没有修改 H2 协议、二叉树根格式，
没有启用 deferred execution，也没有停写原执行状态表。

## 从根计算接到 EVM 读取

新增 `QmdbReadView`，从已认证的 QMDB 活跃值或已通过二叉树根校验的块操作
派生不可变索引，绑定确切 `block_hash` 和 `root`。`imbl` 结构共享索引只用于查找，
不产生另一种状态承诺。它是额外的内存索引，目前不是磁盘随机读后端。

`Gov5QmdbStateRootStore` 默认不构建这些视图；启用后在 WAL 成功后才发布。
根不匹配、WAL 编码/写入失败的块不能被读者拿到。每个 provider 固定一个 `Arc`，
账户/槽读取不访问森林锁，也不查可变 tip。缓存淘汰不会改变正在使用的旧视图；
缓存未命中时从保留的耐久分支重建，未知版本与缺少 key 分开处理。

新增 reth 补丁在 `BlockchainProvider::state_by_block_hash` 完成原有状态选择后
调用进程级一次性注册的适配器。payload builder 的父状态与 engine 的历史基底
会经过该入口；engine 自己的未持久化 overlay 继续叠在确切基底之上。
适配器错误不会被外层 historical/pending 回退吞掉。

`N42_QMDB_READS` 模式：

| 模式 | 行为 |
| --- | --- |
| `off`，默认 | 不注册适配器，不构建派生视图 |
| `verify` | 对比 QMDB 与同一个 reth provider 的账户/槽；不一致或解码错误即失败 |
| `on` | 可用版本从 QMDB 读；缺少整个版本时计数并回退 |
| `only` | 可用版本从 QMDB 读；缺少整个版本时报错 |

启动时要求已认证的 `N42_GOV5_QMDB_EXECUTION` store。账户严格解码原生 Gov5
MarshalV2，拒绝非规范整数、未知标志、截断、额外尾部；槽必须恰为 32 字节。
空槽与零槽等价；校验模式规范化 reth 显式空代码哈希与 Gov5 省略该哈希的差异，
仍比较余额、nonce 和非空代码哈希。字节码、BLOCKHASH、兼容证明/root 接口由
原 provider 提供。`latest()`、直接 historical/pending RPC 尚未全部迁移。

指标包括 provider pinned/unavailable、账户/槽读取数、解码错误、比较 matched/mismatch，
以及 `n42_qmdb_read_view_build_ms{operation=restore|commit}` 和每次更新数。
零 mismatch 必须同时检查比较数及 unavailable，不能视为自动验收通过。

## 已验证的边界

- 精确兄弟分叉、更新/删除、缓存淘汰后仍持有旧视图、重新构建旧版本。
- 16 个读线程在森林锁被持有期间仍能读取已固定的视图。
- 根不匹配和 WAL 故障不发布；fsync 延迟时旧视图可读、新视图不可见；重启可恢复。
- 实际 REVM 执行 SLOAD 返回 QMDB 的 42；备用 provider 故意提供错误余额、nonce、槽，
  交易仍正确执行且 nonce 增加，确认执行读取没有偷偷使用备用账户/槽。
- 实际 MDBX/RocksDB `BlockchainProvider` 集成：数据库缺少测试账户/槽，
  注册 `only` 后经生产入口取得 QMDB 的值；重复注册失败。
- 损坏 QMDB 值在 verify/on/only 都报错，不变成零、不回退掩盖。

节点 QMDB 单元测试 **31 passed、3 ignored**（显式测量）；生产 provider 集成测试
**1 passed**。完整 workspace all-targets check 与 clippy `-D warnings` 通过。
原有核心二叉树 62 项通过的记录见 devlog-146，本批仅增加只读活跃值迭代器。

验证沿用 `/tmp/n42-ci-repro-j5wu4xcf` 的干净固定 reth 基线
`23316e3ff8adca8c3bd5085ff0565fcae019202a`，同步本批修改并应用两个仓库补丁。
锁文件仅为 n42-node 增加已在图中的 imbl 依赖。未改动相邻 reth/gov5/rs 工作区。
补丁脚本已验证可重复执行；第二个补丁不匹配时，第一个缺失补丁也不会先被写入。

## 读索引成本：不是交易 TPS

AMD EPYC 9B45，release、thin LTO。真实账户编码，nonce 1→2、余额 1e9→2e9；
每次更新 147,000 个不同账户，保留旧视图并检查其 nonce 不变。
计时分别覆盖初始索引构建、派生索引、1,000,000 次伪随机账户查找及解码。
输入准备和 QMDB 根计算在这些计时之外；线程数是读线程数。

| 活跃账户 | 初始构建 | 派生 147k 更新 | 单线程读/s | 16 线程读/s |
| ---: | ---: | ---: | ---: | ---: |
| 200,000 | 42.243 ms | 50.626 ms | 3,875,981 | 30,430,131 |
| 2,000,000 | 910.407 ms | 170.939 ms | 1,544,569 | 18,998,595 |

这是各一次样本，没有置信区间，不含磁盘读取、完整 EVM、签名验证、网络、fsync 或 H2。
热内存读/s 不能换算为已提交 TPS。200 万账户下约 171 ms 的派生开销已经值得优先优化；
它发生在提交路径，并且首次/深回退重建还会占用森林锁。

为避免完整节点 release 链接成本，测量使用独立小 harness，以 `#[path]` 编译仓库内
同一个 `qmdb_read_view.rs`，依赖同一 twig 源文件及固定版本 alloy-primitives 1.6.1、
reth-primitives-traits 0.6.0、imbl 7.0.1、thiserror 2.0.19。完整 lock 与 harness 源文件
随日志归档；它不是节点 release 构建的全部依赖/feature 图。节点源码仍由完整 workspace
check/clippy 和上述测试验证。

日志与 harness：[本批实验目录](benchmarks/20260914-qmdb-reads/)。在干净依赖工作区
可直接运行仓库内同一基准：

```bash
cargo test --locked --offline -p n42-node --lib qmdb_
cargo test --locked --offline -p n42-node --test qmdb_provider_hook
N42_READ_BENCH_KEYS=2000000 cargo test --release --locked --offline \
  -p n42-node --lib bench_immutable_read_views -- --ignored --nocapture
```

## 剩余关键工作

1. 压低大状态集的视图派生成本与重复 value 分配，加入多轮对照和 RSS 测量。
   当前缓存上限 64 个视图，不是字节预算；共享页和外部固定读者仍可能占用大量内存，
   尚需按实际改动量/字节和读者生命周期约束，不能照搬上游按块数裁剪的做法。
2. 补齐多块 engine overlay、SELFDESTRUCT、DB 落后和全 RPC 读取路径的集成验证，
   再决定生产默认模式。此时仍不能停写 reth 原状态表。
3. 四个独立节点连续窗口验证 H2 共同提交、真实读取计数、执行正确性、磁盘耐久与重启。
   本环境创建 socket 返回 EPERM，当前无法运行真实四节点网络，不能把离线结果报成达标。
