# Devlog 144 — Gov5 数据基线上的纯 Rust 七节点独立舰队

日期：2026-09-04 UTC。

## 范围与隔离

按用户确认，在本机部署七个 N42-26 验证节点，从 Gov5 冻结历史快照副本继续出块。
不是七节点 Go/Rust 混合网，不是空 genesis 测试网，也没有替换运行中的 Gov5 验证者。

运行目录：`/home/n42/src/n42/N42-26/.artifacts/chain94-fleet-20260904`。
源码整合基线：`main@7bcbe56` + 本地 `perf/erigon-borrow-20260828@9629353`。
保留 main 的三个月审计修复；QMDB WAL 合并冲突按“新锁外 WAL 写入/失败 poison + 严格目录 fsync”处理。
参照本地 Gov5 的 `scripts/qs/deploy-7node.sh`、`scripts/qs/qs-env.sh`、
`docs/QS_FLEET_DEPLOYMENT.md` 和 `docs/QS_LINUX_FLEET_20260828.md`。

| 项目 | 本舰队 |
|---|---|
| chainId | 94 |
| genesis hash | `0xa2d2ff5d814552bb9a113b68ad7ed2b824fbb52caed42dbe573068845b57be99` |
| 起点高度 | 13,560,375（冻结历史快照，不是部署时 Gov5 最新高度） |
| 起点 hash | `0x0e37dae9d0cbf1c8e09c335654dc4cae3e18760dade40039e0e693368cc796d7` |
| 起点 QMDB root | `0xa697c095ee299396deee1f7c63f0f6e50bd7143023e2461e84b09edab2e8495d` |
| 执行状态 | 6,120,111 个账户、37,383 个 storage slots（导出 manifest） |
| QMDB | 6,157,495 个 live entries、63,349,357 个历史 slots、30,933 twigs |
| 委员会 | Gov5 原七个 BLS 验证者，f=2、quorum=5；legacy signing |
| 移动委员会参数 | pool 200,000 / committee 512 / ramp 1,000,000，保留原 seed |
| 奖励 | 按 Gov5 配置，beneficiary 与 faucet 各 1 ETH/block，经 withdrawals 执行 |
| 后续启动间隔 | 显式取 consensus 配置的 3,000 ms |
| RPC | `127.0.0.1:22400..22406` |
| Engine / metrics | `127.0.0.1:22500..22506` / `22600..22606` |
| 共识 TCP + QUIC | `127.0.0.1:32400..32406` |
| Reth P2P / StarHub | `127.0.0.1:30400..30406` / `9740..9746` |

每个节点有独立的 Reth DB、QMDB base/WAL、共识状态和端口。七份 Reth 模板文件与源冻结模板逐文件
SHA-256 相同；leaf-form、genesis、genesis range、源共识快照也记录摘要。
`manifest.json` 记录来源、摘要、基线和公网可见的 P2P 身份，不含私钥。

P2P 使用本轮新生成的 Ed25519 身份，更新运行时的 validator→PeerId 绑定，BLS 委员会及已签名的
QC 不变。所有服务绑定 loopback；关闭 mDNS/DHT/自动连接和 Reth discovery，Reth peers 上限为零，
共识静态 peer 列表仅包含本舰队。原 `/data/blockchain/qs-node*` 未被本任务打开、停机、重置或覆盖。
不得把此副本连接到原 Gov5 网：相同 BLS 委员会、相同链身份的独立分叉不是可合并的数据。

## 补齐的实现

1. **原生 leader header**：原有生产转换仅支持早期 pre-Shanghai devnet，会清空提款、Cancun 和
   `parentBeaconBlockRoot`。新路径从已认证原生父头选择链 94 形状，保留已执行奖励和委员会根，
   写入 QMDB/native receipts/rewards commitments，使用 23 字段原生编码计算哈希并完整广播奖励体。
   非空 requests、后续未实现的 header commitments、非零移动注册根拒绝生产，不静默丢弃。
2. **候选根与导入根一致**：leader 使用执行器记录的 restored slots；Prague 时加入 Gov5 的空
   system-caller leaf，与 follower 的 QMDB 导入规则相同。归一化后的 payload 仍必须先通过
   `new_payload(Valid)` 才能发送 Proposal。没有启用 trusted-state-root 模式。
3. **本机绑定**：新增 `N42_LISTEN_IP`，控制共识 TCP/QUIC 和 StarHub；默认行为保持原来的全接口绑定。
4. **完整创世前缀**：首轮实机启动发现 leaf-form reader 固定只保留前 64 个叶子，本快照 genesis
   含 2,762 个账户，导致校验必然失败。按可信 genesis 的实际条目数读取跨 twig 的完整连续前缀，
   没有放宽成只比较前 64 个叶子。新增 3,000 叶、跨 twig 回归。
5. **重启时间戳恢复**：实机整队重启暴露 `last_committed_timestamp=0` 的进程内初始化问题；快链
   的父块时间戳可能领先墙钟，导致所有 leader 的 FCU 都报 invalid timestamp。现在从已认证执行头
   初始化时间戳下限；不篡改已存储区块/QC、不重置 vote watermark。新增首块时间戳严格大于父块回归。
6. **持久化**：新引入的 QMDB base 文件 rename 后也严格同步父目录，沿用审计后的错误传播原则。

## 部署与运维

工具：[`scripts/chain94-fleet.py`](../scripts/chain94-fleet.py)。`prepare` 需要 Python `cryptography`；
节点运行只需已构建二进制。私钥仅从用户指定的本机文件读取，按节点写入权限 0600 的文件，通过
`@file` 传给节点，不打印、不放进命令行参数、不提交版本库。

```bash
cargo build --offline --locked --release -p n42-node-bin --bin n42-node -j 16

# 仅用于一个不存在的新 runtime；不会覆盖已有舰队。
python3 scripts/chain94-fleet.py --runtime .artifacts/chain94-fleet-NEW prepare \
  --source /data/blockchain/mixed-fleet/n42-26-qs \
  --leaf /home/n42/src/n42/N42-26-wt-erigon/target/chain94-record/snapshot-13560375.leafform.qmdb \
  --validators /home/n42/qs-validators.md

python3 scripts/chain94-fleet.py --runtime .artifacts/chain94-fleet-20260904 start
python3 scripts/chain94-fleet.py --runtime .artifacts/chain94-fleet-20260904 status
python3 scripts/chain94-fleet.py --runtime .artifacts/chain94-fleet-20260904 verify --seconds 60 --min-blocks 7
python3 scripts/chain94-fleet.py --runtime .artifacts/chain94-fleet-20260904 stop
```

`start` 先检查全部端口、冻结工件摘要与已有 PID；TCP 预检允许 TIME_WAIT，但拒绝实际监听冲突。
`stop` 核对 PID starttime 和准确 datadir 后只发 SIGINT，等待最多 60 秒，不升级为 SIGKILL，不删除数据。
中断的 `prepare` 可用 `--resume-prepare` 校验后续作；已经存在 manifest 或 PID 的舰队不能使用此选项。

`verify` 取七个 RPC 的 CommitQC 对应执行块，比较共同高度的 hash/stateRoot/receiptsRoot/transactionsRoot，
要求持续推进且窗口内七个 beneficiary 都实际出块。对于 QC 发布稍早于异步 FCU 的窗口，仅等待同一个
hash 最多两秒；不会换成另一个 latest head 来掩盖失败。结果保存在运行目录的 `verification*.json`。

## 实测记录

- 首轮：2026-09-04 19:45:59–19:46:59 UTC，60 秒，13 次取样。
  共同已提交高度 **13,560,904 → 13,561,317（+413）**，所有取样的四项承诺一致，七个 leader 全覆盖。
  该轮未显式传入 interval，使用了 N42 默认快速 cadence；属于空交易功能测试，不是 Gov5 性能对照或 TPS。
- 随后在高度 **13,561,449** 优雅停机，七节点执行头一致；没有清理数据。
- 第一次恢复验证命中上述时间戳缺陷；保留日志和状态，修复后继续验证。
- 修复后恢复：2026-09-04 19:57:25–19:58:25 UTC，显式 3,000 ms，60 秒、13 次取样。
  共同已提交高度 **13,561,455 → 13,561,475（+20）**，七个验证者都实际出块，
  全部取样的区块哈希、状态根、收据根、交易根一致，`verification-1788551905.json` 为 PASS。
  末块 hash：`0x793c51ef61989b97dd1c2e893806d56125ac24fed1dd97eb3589870d9a7a902a`；
  state root：`0x35061f44fcfdcbbc4bdcad30cd580055486cf523550d154f61fcb0beb571ad16`。
- 本轮二进制 SHA-256：`3413b6a9edb2ac6f7632ee41110974d078f0aa99a0c43cc6482b6548d865c375`。
  `run.json` 保存该摘要、启动时间、间隔和各日志的本轮起始 offset。
- 本轮启动后的日志检查：七节点均无 ERROR、invalid timestamp 或 root mismatch。
  仍有宿主 QUIC buffer 上限提示（未修改 sysctl）、重复投票抑制日志、父块领先墙钟时的
  timestamp bump 提示，以及 header 归一化后主动清除旧哈希缓存的 eviction WARN；不是零告警运行。
- `ss -lntup` 核验全部 49 个专属监听端点仅绑定 `127.0.0.1`。验收结束后七节点保持运行，
  未创建系统自启动服务。
- 2026-09-04 22:17:07 UTC，按用户要求通过部署工具向本舰队发送 SIGINT，七节点全部优雅退出。
  随后确认全部专属端口已释放；数据目录、快照、密钥和验收日志保留，原 Gov5 舰队未受影响。
  用户同时要求提交并推送本阶段源码；运行数据与密钥继续由 `.artifacts` 忽略规则排除。

## 验证与限制

- 全工作区库单测一次运行：1,466 passed、7 ignored；四项 socket 测试当时显式过滤，随后在允许
  loopback 的环境补跑 transport（10 passed）和 ingest（2 passed），覆盖全部四项。
- 新增恢复时间戳、Gov5 奖励来源两项回归均通过，去重后累计 1,472 项库测试通过。
- 集成测试：consensus integration 67、chaos_7node 12、restored_slots 3、stream_v2_pipeline 12，
  合计 94 项通过。
- 原生 header/reward/body 往返与篡改拒绝测试、跨 twig 创世前缀测试通过。
- 部署工具五项回归通过：PID 复用保护、错 datadir 不发信号、固定 QC hash 等待 FCU、超时拒绝、共同高度根分歧检测。
- 全工作区全 targets Clippy `-D warnings` 通过；依赖 proc-macro-error2 的 future-incompatibility 提示仍在。
- 本轮使用冻结执行状态与 QMDB 状态继续链，不提供快照前 1,356 万块的完整可查询 Reth 历史；模板中的
  历史占位 headers 不是原链历史归档，不能据此声称历史 RPC 数据完整。
- 当前功能验收无交易压测、无手机设备流量、无真实多主机网络/断电测试。移动委员会 evidence 是模拟池的
  密码学重建，不代表真实手机 attestations。
- EOF 仍 fail-closed，不支持完整 EOF 执行；移动注册根生产仅支持已认证的零根父块，非零拒绝。
  后续 fork、非空执行 requests、任意历史合约负载仍需独立兼容性验收。
- QMDB hollow tree 不提供 sealed twig 的完整历史证明；base 重写和长期 WAL 保留仍需长期运行验证。
