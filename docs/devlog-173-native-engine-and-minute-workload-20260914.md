# 原生 Engine 路径与一分钟负载

日期：2026-09-14 至 15（America/New_York，日志 UTC 为 9 月 15 日）。
目标仍是四节点 H2、二叉树 QMDB-only、成功提交 1M TPS；本轮没有网络 TPS 结果。

## 原生第一块执行检查

原 `parallel_qmdb_builder` 测试验证了在线 builder、并行 follower 和 QMDB 根，但最后
导入的是 Ethereum 头/验证器，并未经过原生 payload 转换。本轮将共用场景移入
`tests/common/parallel_qmdb_builder.rs`，保留 Ethereum 对照，新增独立测试进程
`native_parallel_qmdb_builder`，避免 write-once provider registry 在同进程互相干扰。

原生场景记住完整 Cancun 创世头（不含 mobileRegistryRoot），运行在线 builder，
将真实执行输出转换成原生 QMDB/收据根，然后走：

```text
normalize_execution_payload_for_gov5_h2
→ encode_gov5_block_rlp → decode_gov5_block_rlp
→ N42EngineValidator(Gov5H2)
→ BasicEngineValidator / N42Consensus(Gov5H2)
→ 并行 import + QMDB state-root strategy
```

验证首块保留 Cancun/withdrawals 标记、mobile root 为零、原生哈希通过 wire/payload
转换保持一致。有效块从 `BlockOrPayload::Payload` 进入实际 Engine 交易恢复和执行路径。
保留独立余额/nonce/费用根 oracle、完整状态与 revert 对拍，以及错误根/重复 nonce 拒绝，
检查每笔交易进入本地并行执行，QMDB 根按原生块哈希发布，Engine validation 不推进 forkchoice。
Ethereum 与原生场景的 Debug、Release 测试和相关 Clippy 均通过。

此测试是 4096 笔带交易执行集成；pool admission 使用既有 mock validator，后续真实恢复
签名。它不包含 H2 投票/CommitQC、实际四进程网络、forkchoice 提交或性能测量，不能用来
证明四节点目标。生产运行仍须保留这些环节。

## 大文件暴露的顺序问题

上一轮纠正了收款人索引，但文件仍按发送者连续保存。22 万笔文件每个发送者仅 44 笔，
整个文件覆盖全部账户；7200 万笔文件每个发送者连续 14400 笔。如果按原顺序注入，
每个 RPC 的前 55000 笔只涉及 4 个发送者，四个 RPC 合计仅 16 个发送者，
并非预期的 5000 个。收款人索引正确并不等于每个块的负载分布正确。

独立收款人模式现在按发送者轮转：同组每个发送者先发布第 0 笔，再发布第 1 笔，依次类推。
在线、内存和文件路径保持相同顺序，单发送者 nonce 顺序不变。
文件签名仍是最多 16 个线程、每任务 1024 条、最多一个任务/线程暂存结果，按输入顺序
收集输出，保留原子发布。未启用独立收款人模式时保留旧实验顺序。
新测试覆盖多轮、1024 条任务边界、不等长发送者和空组；9 项 Rust 回归及 Clippy 通过。

## 启动器池容量

复核完整启动参数又发现旧七节点空块配置被沿用：启动器无条件设置 `N42_LOW_MEMORY=1`，
使 pool 走固定 5000 笔/10 MiB、单账户 1024 条的分支，即使设置 220000 的区块上限，
也不会进入自适应池容量计算。新四节点现在关闭该模式，由已有逻辑按区块上限配置
660000 笔的 subpool、单账户 16384 条；旧七节点空块模式保持不变。

新增显式 `--disable-tx-forward`，用于 per-node-continuous 压测，避免仅在 stress 进程
设置同名环境变量却未传入四个节点。它不关闭原生区块 gossip。两个选择均写入 run.json，
八项启动器测试通过，覆盖四节点高容量参数与七节点原参数。

## 可复用命令与范围

新的分钟负载用于尚未执行过交易的 chain 941004，每个发送者从 nonce 0 开始。
如果启动小负载已经在链上执行，应先联网读取 nonce 重签，不能直接复用零 nonce 文件。
本轮准备文件的命令：

```bash
N42_CHAIN_ID=941004 target/release/n42-stress \
  --presign-genesis .artifacts/native-four-20260914/artifacts/genesis.json \
  --recipients-file .artifacts/native-four-20260914/recipients.json \
  --presign-save .artifacts/native-four-20260914/presigned-minute-72m-interleaved.bin \
  --presign 72000000 --accounts 5000 \
  --rpc http://127.0.0.1:23400,http://127.0.0.1:23401,http://127.0.0.1:23402,http://127.0.0.1:23403
```

在允许 TCP/UDP 的执行环境，先启动并验证空块轮转：

```bash
python3 scripts/chain94-fleet.py --runtime "$PWD/.artifacts/native-four-20260914" start \
  --binary "$PWD/target/release/n42-node" --fast-transfers --parallel-build --parallel-import \
  --disable-tx-forward --qmdb-reads only
python3 scripts/chain94-fleet.py --runtime "$PWD/.artifacts/native-four-20260914" verify --seconds 30 --min-blocks 8
```

然后运行一分钟验收：

```bash
N42_CHAIN_ID=941004 \
N42_BENCH_RPC_BASE=23400 N42_BENCH_INGEST_BASE=34400 N42_BENCH_METRICS_BASE=23600 \
N42_BENCH_TRUSTED_CONFIG="$PWD/.artifacts/native-four-20260914/trusted-config.json" \
N42_PRESIGNED_TXS="$PWD/.artifacts/native-four-20260914/presigned-minute-72m-interleaved.bin" \
N42_BENCH_DATA_DIR="$PWD/.artifacts/native-four-20260914" \
N42_BENCH_WAVE_TXS=220000 N42_MAX_TXS_PER_BLOCK=220000 \
N42_FAST_TRANSFER=1 N42_PARALLEL_BUILD=1 N42_PARALLEL_IMPORT=1 \
N42_BENCH_ARTIFACT_DIR="$PWD/.artifacts/native-four-20260914/qualification-minute" \
bash scripts/qualify-1m-tps.sh
```

上述环境参数记录测试意图；节点实际参数仍以启动器 run.json、二进制和 RPC 审计为准。
本轮还补建此前缺少的 Release `n42-verify-commit`，没有关闭签名、收据或状态根验证。
最终门槛仍是四节点共同 H2 committed 的成功交易与持久化 QMDB-only 证据。

完整材料检查、旧顺序失败复现、轮转文件审计和环境执行结果见
[证据 manifest](benchmarks/20260914-native-engine-minute-workload/manifest.json)。

## 实物与最新上游结果

轮转文件已生成：72000000 笔，四组各 18000000 笔，10149233021 字节。
完整 N42T 格式加载通过；逐笔验证各 RPC 的前 55000 笔，共 220000 个 secp256k1 签名，
覆盖全部 5000 个发送者及 147000 个收款人，每个发送者 nonce 0–43 连续。
这不是对余下 71780000 个签名的逐笔审计，它们仍须经过节点正常验签与执行。
同一审计在旧 account-major 分布文件上失败，实测只有 16 个发送者，已将旧文件改名为
`presigned-minute-72m-rejected-account-major.bin` 防止误选。

新版生成 wall 311.15 秒、峰值 RSS 159620 KiB；完整加载加首批签名审计 44.05 秒、
峰值 RSS 13239236 KiB。以上均为输入准备/检查成本，不能称作 TPS 成绩。

真正执行 `qualify-1m-tps.sh` 后，文件 SHA/chain ID/6000 万笔容量门槛通过；
H2 RPC preflight 随即返回 `Operation not permitted`。启动器同样在创建 TCP socket 时
被拒绝，直接探测 TCP/UDP 均为 errno 1。没有启动四节点、没有提交 QC、没有出块或
一分钟计时窗口；目标仍未达到。

重新读取两个仓库的本地 origin/main：rs 仍为 `466c1839791afadba7f1dd4d48c5518440dcafba`；
Gov5 新增 `074f9c8f22a7755de3bbe3e4455dec6a32b2a4d2`，只有两篇运行记录更新，已逐行补读。
35zzp 完整 B 段均值 87.9k，对照 90.6k；follower body 中位数降到 15 ms（p90 42），
但第二窗口退化、内存 watchdog 停机，不能从阶段耗时改善宣称总体 TPS 提升。
记录指出 `/tmp` 增长到 35 GB，其中本目标的干净构建副本约 4.8 GB。
本轮将自己的构建副本迁移到工作区磁盘 `.artifacts/ci-repro-j5wu4xcf`，
保留 `/tmp/n42-ci-repro-j5wu4xcf` 为兼容符号链接，并核对相关源码摘要。
不改其他会话的数据；没有复跑或宣称 Gov5 的受控 A/B。未尝试刷新远端。
