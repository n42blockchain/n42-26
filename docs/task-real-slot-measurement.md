# 任务:在真实节点量 8s slot 分解(stop optimizing blind)

## 状态更新（2026-06-13 · 改为 macOS 执行）

本任务在 **macOS/Darwin 上进行**。step 5/6 已证明 4 节点 testnet 能在 mac 上 build + run
(`devlog-70`),所以"必须 Linux"的旧判断只对 `perf` 这一个工具成立——**采样改用 mac 原生的
samply / Xcode Instruments(`xctrace` Time Profiler)即可出 leader flamegraph**,其余(testnet、
metrics、tracing span、workload)在 mac 上一样能跑。

仍然成立的纪律:**不要用 CacheDB 微基准或 mac 预检凑 EVM/state-root/BLS/consensus/network 百分比**;
数字必须来自真实 testnet(≥4 节点 + 手机 sim + 合约重 workload)的实测。

## 状态更新（2026-06-13 · bounded mac pilot 完成）

见 `docs/devlog-71-real-slot-profile.md`。已完成一次真实 4-node macOS testnet pilot:

- 纯转账:192k tx post-drain 全部上链,11 个 tx-bearing blocks,最大 24k tx/block;
- 合约重:clean contract-only 链 fresh presign,120k tx post-drain 全部上链,14 个 tx-bearing blocks;
- mobile sim:4 phones;
- Twig:`N42_TWIG=1`;
- 采样:`samply attach` 被 macOS `task_for_pid` 权限挡住,改用 `/usr/bin/sample`;release binary 未符号化,所以这次不声称函数级 flamegraph 热点。

仍需后续补齐:用 `--profile profiling` 或 `samply record -- <node ...>` 直接 launch 出符号化
leader flamegraph,并跑更长 steady-state(4/7 节点,数百块)。

## 背景与目标

所有现有 EVM/状态/BLS 微基准都在 **CacheDB 内存态**跑,测不到真实节点的
**state-access(trie/mdbx/缓存 miss)、witness 生成、手机桥接、网络** 的真实开销。
8s slot 预算那张表(EVM 2000ms / BLS 687ms / 状态根 500ms)是 **2026-03 的老估计**,
经过这一轮优化(BLS→~200ms、状态根→twig、reth 2.3 prewarm/cache/并行 state root 默认生效)
**真实瓶颈早就变了,但没人在真实节点上量过**。

**目标:把"猜"变"测"** —— 在真实 testnet 上跑真实负载,产出**当前真实的 slot 阶段分解**,
据此决定下一步优化方向(而不是继续优化 CacheDB 微基准)。

## 前置（基线对齐，务必）

- n42-26 HEAD 最新(`git pull --rebase`;当前分支 tip `21d59e2` 或其后代)。
- `../reth` 在 `chore/merge-upstream-fc2cc1e` @ `449ecfdce`(jit 默认关)。Step 0:
  `git -C ../reth log -1 --oneline` 确认;**切勿降级 Cargo.toml 的 revm/alloy/reth-* pin**。
- 构建(macOS):`cargo build --release -p n42-node-bin -p e2e-test -p n42-stress -p n42-mobile-sim`。
  (step 5/6 已验证 node bin 在 mac 能 build/run。)

## 测什么（slot 阶段分解）

一个 slot（leader 视角）的真实墙钟分解，至少拆出：

| 阶段 | 怎么量 |
|------|--------|
| **EVM 执行**（payload build / new_payload）| reth 已有 tracing span（`parallel_execute`、reth engine span);或加计时日志 |
| **state-access 内部**（execute 中 SLOAD→trie/mdbx/cache）| reth prewarm/cache 命中率 metrics;`n42_execution_block_ms` 直方图 |
| **状态根计算** | reth 并行 state root span / n42 twig(`N42_TWIG=1`) `root()` 计时 |
| **BLS 收据验证**（follower/leader 聚合）| 已加的 `n42_mobile_receipt_*` metrics + receipt_batch flush 计时 |
| **witness/mobile packet 生成** | `n42_execution_block_ms`(packet loop 的 execute_one)+ zstd 压缩计时;**它 background,单列** |
| **共识**(投票/QC 聚合/网络) | 共识 span;实测应 <1ms |
| **区块传播/序列化** | 已有 compact block 计时(缓存命中 ~3ms);压缩吞吐 |

关键区分:**slot 关键路径(阻塞下一块)** vs **background(witness/packet/手机)**。只有关键路径决定 8s 能否达标。

## 怎么测（建议方法）

1. **负载**:用 `n42-stress`(TCP 注入,122K tx/s)灌不同 workload:
   - 纯转账(48K-cap,简单);
   - 合约重(ERC-20/DeFi,多 SLOAD/SSTORE)—— 这才暴露 state-access 真实开销。
   两种都测,对比。
2. **节点规模**:先 4-7 节点(`./scripts/testnet.sh --nodes 7`),后可 21 节点。每节点挂若干
   `n42-mobile-sim` 模拟手机(`./scripts/mobile-sim.sh`),量手机桥接 + 收据验证在真实并发下的开销。
3. **采集**:
   - **metrics**:抓所有 `n42_*` 直方图 + reth engine metrics(确认 testnet metrics endpoint;
     Prometheus 抓或直接 curl `/metrics` 离线聚合 p50/p95/p99);
   - 或 **tracing JSON 日志**(`--log.stdout.format json`)抓 span 时长,离线聚合 p50/p95/p99;
   - **leader 进程采样(macOS)** —— 任选其一出 flamegraph 看真实热点函数:
     - **samply**(跨平台,mac 支持):`samply record -p <leader_pid>` 或 `samply record -- <node bin> ...`,
       浏览器看火焰图 / 导出;
     - **Xcode Instruments Time Profiler**:`xcrun xctrace record --template 'Time Profiler'
       --attach <leader_pid> --output leader.trace`(或 `--launch -- <node bin> ...`),
       Instruments 打开看 self-time;`cargo-instruments` 可简化;
     - release build 记得带符号(`[profile.release] debug = 1` 或 `--profile profiling`)以便符号化。
4. **稳态**:跑够长(≥数百块)取稳态,排除冷启。记录 CPU 利用率 + 每核负载。

## 产出（交付）

`docs/devlog-NN-real-slot-profile.md`,含:
1. **真实 slot 阶段分解表**(两种 workload,p50/p95,关键路径 vs background),对照 2026-03 老估计;
2. **leader 进程 flamegraph**(macOS samply / Instruments,合约重 workload)—— self-time top 函数;
3. **新瓶颈识别**:经过本轮优化后,8s slot 现在卡在哪?(预期:state-access 仍是大头但已被
   prewarm 缓解?BLS 已降到边际?witness background 占多少核?)
4. **下一步优化建议**:基于实测,而非猜测。

## 验收

- 数据来自真实 testnet（≥4 节点 + 手机 sim），非微基准；
- 关键路径 / background 明确区分；
- flamegraph 是 leader 进程合约重 workload 的真实采样(macOS samply/Instruments);
- 结论可指导"下一个该优化什么"。

## 注意

- 这与 P6（twig 节点接入）独立但互补:step3/P6 已落地,顺带量 twig vs SBMT 的真实状态根开销
  (`N42_TWIG=1` vs 默认)。
- 分工:测量/testnet/flamegraph 归 codex（macOS）;Claude（Windows）据结果做后续代码优化。
- macOS 采样权限:`xctrace`/samply 对正在运行的进程采样可能需要在本机授权(开发者工具权限);
  若 attach 受限,改用 `--launch`/`samply record -- <bin>` 直接拉起被测进程。
