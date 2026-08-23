# Gov5 × N42 共建网络、加入舰队与混合节点出块手册

本文面向 Gov5（Go）与 N42（Rust/reth）联合运维，说明如何冻结一套双方可复核的网络资料、
让 Rust 节点先安全跟随再加入验证者舰队，并验证 Go/Rust 混合节点能够轮值出块、形成 QC、
执行相同交易并持续稳定运行。

本文不是通用公网部署模板。命令以仓库现有的七验证者资格脚本为准，端口和路径仅是已验证
拓扑的示例；上生产前必须替换为本次网络的清单，并由 Gov5 与 N42 双方签字确认。

## 1. 已验证基线

2026-08-17 至 2026-08-18 的正式运行采用 5 个 Gov5 验证者和 2 个 Rust 验证者：

| 槽位 | 客户端 | HTTP RPC | 共识/P2P | 说明 |
|---:|---|---:|---:|---|
| 0 | Rust | 29545 | 19780/QUIC | Rust 原生身份 |
| 1–5 | Gov5 | 28501–28505 | 30301–30305/TCP | 五个 Go 验证者 |
| 6 | Rust | 29546 | 30306/QUIC | 原 Gov5 槽位 6 的等身份替换 |

本次基线固定为：

- 验证者数 7，容错数 2，法定人数 5，轮值跨度 7；
- chain ID `1143`（`0x477`）；
- 创世哈希 `0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec`；
- Gov5 `main`：`ff9b9730b787614755c6ebd837cb1980a0cb6be6`；
- Gov5 二进制 SHA-256：`976016fb7a9c14aed434afe823bf5bd84691e6cc826cf90db3691a049ab23eab`；
- Rust 二进制 SHA-256：`f1f22469b483f06b1c5819bb8e47c90c021cfe298774352fadd164bc2e788e5e`。

这些值是可回放的测试基线，不是以后网络的永久参数。任一代码、二进制、创世或验证者集合
发生变化，都必须建立新的运行编号并重新完成本文的验收窗口，不能沿用旧的 PASS。

正式 24 小时结果为 PASS：持续 `86,410` 秒、`7,859` 个样本、增长 `100,810` 块、最大
端点高度差 6；两台 Rust 均形成已提交 QC，七个验证者均轮值出块，equivocation 为 0。
随后交替从 Go/Rust RPC 注入 17 笔签名交易，完成 301 次精确 RPC 对比；交易后继续增长
210 块且未重发交易。

证据位于资格运行目录，不提交大体积运行数据到源码仓：

```text
evidence/formal24h-46a0df1-final-verification.json
evidence/formal24h-46a0df1-total-final-verification.json
```

## 2. 三种节点身份

| 模式 | 投票/出块 | 需要 BLS 私钥 | 用途 |
|---|---|---|---|
| RPC/执行跟随节点 | 否 | 否 | 查询、执行同步、应用接入 |
| 共识 observer | 否 | 否 | 校验 H2-v4、QC、区块与 QMDB lineage |
| validator participant | 是 | 是 | 参与投票、形成 QC、按槽位出块 |

“加入舰队”必须先进入 observer/跟随状态，完成追高和数据一致性检查，再由双方在批准的切换
窗口启用 participant。不得把一个还未追高的节点直接作为验证者启动。

> **严禁双签：**同一个验证者的 BLS 私钥和槽位在任一时刻只能由一个进程使用。用 Rust
> 替换 Gov5 槽位时，必须先干净停止旧 Gov5 进程并确认 PID、端口均已释放，再启动 Rust。
> 数据库保留不动，以便回滚；不得让两个客户端短暂重叠运行。

## 3. 双方必须共同冻结的网络包

每次新建网络或升级都创建一个只读的发布目录，至少包含：

```text
release-NETWORK-YYYYMMDD/
├── MANIFEST.sha256
├── genesis.json
├── consensus-peer-bound.json
├── bootstrap-bundle.json
├── inventory.csv
├── binaries/
│   ├── gov5-n42
│   └── n42-node
└── public-identities/
    ├── validator-addresses.txt
    ├── bls-public-keys.txt
    └── p2p-peer-ids.txt
```

双方逐项核对并记录：

1. chain ID、创世哈希、创世文件 SHA-256、fork 高度和执行规则完全一致；
2. H2-v4/`interopV4` 已启用，Gov5 header profile 与 bootstrap bundle 相同；
3. 验证者的顺序、地址、BLS 公钥和 P2P PeerId 逐槽一致；
4. `validator_set_size`、fault tolerance、slot/period、timeout 与静态委员会策略一致；
5. 静态 peer 的公网地址、传输协议、端口、防火墙和 NAT 映射已确认；
6. 两种二进制的源码提交、构建命令、编译器版本和 SHA-256 已记录；
7. 所有主机使用 NTP/chrony，UTC 偏差在运维门限内；
8. 私钥只在目标主机落盘，权限最小化，发布包中只放公钥和哈希。

在 macOS 使用 `shasum -a 256`，Linux 可使用 `sha256sum`。分发后必须重新核对：

```bash
cd release-NETWORK-YYYYMMDD
shasum -a 256 -c MANIFEST.sha256
```

`consensus-peer-bound.json` 和 `bootstrap-bundle.json` 是认证边界，不能在节点上临时手改。
需要改验证者、PeerId 或创世时，重新生成整套包并产生新 manifest。

## 4. 构建与准备

Rust 节点在固定提交上构建：

```bash
git status --short --branch
git rev-parse HEAD
cargo build --release --bin n42-node
shasum -a 256 target/release/n42-node
```

Gov5 二进制由 Gov5 仓库在共同确认的提交上按其发布流程构建。不要从运行目录中覆盖正在使用
的二进制；先复制到新发布目录、计算哈希、完成冒烟测试，再进行滚动切换。

每个验证者都必须拥有独立的：

- BLS 私钥，且对应公钥已在固定槽位中；
- P2P 私钥，且推导出的 PeerId 与清单一致；
- execution 数据目录；
- consensus/QMDB 数据目录；
- RPC、execution P2P、consensus P2P 和 StarHub 端口。

不要复用另一条链的 datadir。加入已有链时，若复制快照，必须在源节点停止或使用受支持的
原子快照，同时复制**匹配的 execution 数据和 consensus/QMDB 数据**。只复制其中一半会
破坏已认证 lineage，Rust 节点应当拒绝启动。

## 5. 启动 Gov5 验证者

以下是单节点参数骨架，所有占位值来自本次网络清单：

```bash
gov5-n42 \
  --chain private \
  --profile n42 \
  --datadir /path/to/gov-datadir \
  --port 30301 \
  --http \
  --http.port 28501 \
  --mine \
  --etherbase 0xVALIDATOR_ADDRESS \
  --block-interval-ms 1000 \
  --verbosity 3 \
  --p2p.no-discovery \
  --p2p.min-sync-peers 0 \
  --p2p.max-peers 7 \
  --p2p.genesis-override 0xGENESIS_HASH \
  --p2p.peer /ip4/PEER_IP/tcp/PEER_PORT/p2p/PEER_ID \
  --p2p.peer /ip4/ANOTHER_PEER_IP/tcp/PEER_PORT/p2p/PEER_ID
```

空 datadir 不能只依赖 `--chain private` 自动生成链。必须先用冻结的 `genesis.json` 完成 Gov5
初始化，并检查 `chaindata`、BLS keystore、`network-keys`、`network.json` 和
`epoch_schedule.json` 均存在。启动日志中的实际创世哈希必须等于 manifest。

新网络先启动足以形成法定人数的 Gov5 骨干节点，再逐台加入其余节点。已有网络只允许滚动
操作，一次停止的验证者数量不得触及容错上限。

## 6. Rust 先跟随，再加入共识

### 6.1 Observer 阶段

跟随阶段不加载验证者私钥，并显式启用 observer：

```bash
env \
  N42_CONSENSUS_CONFIG=/path/to/release/consensus-peer-bound.json \
  N42_DATA_DIR=/path/to/runtime/observer/consensus \
  N42_OBSERVER_MODE=1 \
  N42_GOV5_HEADER_PROFILE=1 \
  N42_INTEROP_GENESIS_HASH=0xGENESIS_HASH \
  N42_GOV5_BOOTSTRAP_BUNDLE=/path/to/release/bootstrap-bundle.json \
  N42_CONSENSUS_PORT=19780 \
  N42_STARHUB_PORT=9443 \
  N42_NO_AUTO_CONNECT=1 \
  N42_TRUSTED_PEERS=/ip4/PEER_IP/tcp/PEER_PORT/p2p/PEER_ID \
  N42_ENABLE_MDNS=0 \
  N42_ENABLE_DHT=0 \
  N42_ENABLE_HTTP_RPC=1 \
  N42_P2P_KEY=@/path/to/p2p-key-file \
  n42-node node \
    --chain /path/to/release/genesis.json \
    --datadir /path/to/runtime/observer/reth \
    --disable-discovery \
    --port 31303 \
    --http --http.addr 127.0.0.1 --http.port 29545 \
    --authrpc.port 29551 \
    --ipcdisable --log.file.max-files 0 --color never
```

observer 至少满足以下条件后才可申请切换：

- RPC 可用，`eth_chainId`、创世块哈希与网络包一致；
- 已追至舰队当前高度附近，公共高度的 block hash、state root、receipts root 一致；
- H2-v4 消息持续到达，已观察到 committed QC；
- QMDB bootstrap receipt/lineage 校验通过；
- 日志无 panic、fatal、equivocation、认证失败或持续 catch-up 循环。

### 6.2 Participant 切换

切换窗口按以下顺序执行：

1. 记录七端点共同高度、hash、state root 和当前 QC；
2. 干净停止被替换槽位的 Gov5 进程；
3. 确认旧 PID 不存在，P2P/RPC 端口已释放，创建“替换已激活”审计记录；
4. 从停止状态的数据复制 execution 与 consensus/QMDB 匹配对，或使用已认证快照；
5. 检查 Rust BLS/P2P 密钥分别对应**同一个被批准槽位**；
6. 启动 Rust participant，验证追高；
7. 等待至少完成两轮、该槽位轮到两次，确认实际出块和 QC 投票；
8. 若失败，先停 Rust，再按相反顺序恢复原 Gov5；仍不得重叠签名。

participant 的关键环境变量如下：

```bash
env \
  N42_CONSENSUS_CONFIG=/path/to/release/consensus-peer-bound.json \
  N42_VALIDATOR_KEY=@/path/to/validator-bls-key \
  N42_P2P_KEY=@/path/to/p2p-key-file \
  N42_DATA_DIR=/path/to/runtime/rust-slot-N/consensus \
  N42_GOV5_H2_PARTICIPANT=1 \
  N42_GOV5_HEADER_PROFILE=1 \
  N42_INTEROP_GENESIS_HASH=0xGENESIS_HASH \
  N42_GOV5_BOOTSTRAP_BUNDLE=/path/to/release/bootstrap-bundle.json \
  N42_QMDB_REPLAY_DEPTH=1048576 \
  N42_GOV5_CATCHUP_BUFFER_BLOCKS=131072 \
  N42_CONSENSUS_PORT=19780 \
  N42_STARHUB_PORT=9443 \
  N42_NO_AUTO_CONNECT=1 \
  N42_TRUSTED_PEERS=/ip4/PEER_IP/tcp/PEER_PORT/p2p/PEER_ID \
  N42_ENABLE_MDNS=0 N42_ENABLE_DHT=0 N42_ENABLE_HTTP_RPC=1 \
  n42-node node \
    --chain /path/to/release/genesis.json \
    --datadir /path/to/runtime/rust-slot-N/reth \
    --disable-discovery --port 31303 \
    --http --http.addr 127.0.0.1 --http.port 29545 \
    --authrpc.port 29551 \
    --ipcdisable --log.file.max-files 0 --color never
```

`N42_GOV5_H2_PARTICIPANT=1` 是 fail-closed 路径：它要求 Gov5 header profile、明确的创世哈希、
认证的 replay-v2 QMDB 基线、静态验证者配置和本地已登记 BLS 身份。缺一项时不应绕过检查。

仓库中的资格脚本已经封装了已验证的 5+2 本机拓扑：

```bash
export N42_QUAL_RUNTIME=/path/to/runtime
export N42_GOV_BINARY=/path/to/release/binaries/gov5-n42
export N42_NODE_BINARY=/path/to/release/binaries/n42-node

scripts/gov5-interop-qualification.sh start-gov
scripts/gov5-interop-qualification.sh start-rust
scripts/gov5-interop-qualification.sh start-rust2
scripts/gov5-interop-qualification.sh status
```

该脚本假定 runtime 中已经存在冻结的 artifacts、初始化后的 Gov5 datadir 和匹配的 Rust
execution/QMDB 数据；它是资格验证工具，不是自动生成生产密钥或创世的供应工具。

## 7. 出块与共识检查

先查询所有端点的最新头：

```bash
for port in 28501 28502 28503 28504 28505 29545 29546; do
  curl -fsS -H 'content-type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' \
    "http://127.0.0.1:${port}" |
    jq -c --arg port "$port" '{port:$port,number:.result.number,hash:.result.hash,stateRoot:.result.stateRoot}'
done
```

快速变化的链应并发采样；逐端点串行读取可能人为制造一到数块的表面高度差。判断一致性时，
取本轮最小高度，再在七端点查询该**同一高度**，比较：

- `hash`、`parentHash`；
- `stateRoot`、`transactionsRoot`、`receiptsRoot`；
- `miner` 是否属于固定的七验证者集合；
- parent chain 是否连续。

Rust 共识状态与双签证据：

```bash
curl -fsS -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"n42_consensusStatus","params":[]}' \
  http://127.0.0.1:29545 | jq '.result'

curl -fsS -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"n42_equivocations","params":[]}' \
  http://127.0.0.1:29545 | jq '.result'
```

验收要求 `validatorCount == 7`、`hasCommittedQc == true`、equivocation 总数和 evidence 长度
均为 0。仅“高度在增长”不能证明 Rust 在出块；必须按 `miner` 审计轮值槽位，并将 Rust 日志
中的 committed block hash 与七端点的 canonical hash 对齐。

## 8. 30 分钟、24 小时与交易验收

### 8.1 30 分钟门禁

先记录开始高度，然后运行 30 分钟纯共识窗口：

```bash
RUNTIME=/path/to/runtime
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)"
HEADS="$RUNTIME/evidence/${RUN_ID}-heads-30m.jsonl"

N42_QUAL_RUNTIME="$RUNTIME" \
  scripts/gov5-interop-qualification.sh monitor-heads 1800 10 "$HEADS"

N42_QUAL_RUNTIME="$RUNTIME" \
  scripts/gov5-interop-qualification.sh audit-soak "$HEADS" 1800 120 6 1
```

通过标准：窗口不少于 1,800 秒，样本间隔不超过 120 秒，七端点最大高度差不超过 6，公共
高度的 canonical identity 一致，并且纯共识窗口无交易。另对两台 Rust 分别运行
`audit-rust-leaders`，检查跨度 7 的预期轮值及七端点 hash 完全一致。

### 8.2 24 小时稳定性

30 分钟通过后再开新证据文件执行 24 小时窗口：

```bash
HEADS="$RUNTIME/evidence/${RUN_ID}-heads-24h.jsonl"

N42_QUAL_RUNTIME="$RUNTIME" \
  scripts/gov5-interop-qualification.sh monitor-heads 86400 10 "$HEADS"

N42_QUAL_RUNTIME="$RUNTIME" \
  scripts/gov5-interop-qualification.sh audit-soak "$HEADS" 86400 120 6 1
```

正式窗口同时应由独立进程完成：

- 每 300 秒记录两台 Rust 的 RSS、CPU、线程、FD 和逻辑计数；
- 每 600 秒确认 Gov5 远端提交仍等于本次固定提交；
- 记录系统时钟和区块时间戳；
- 1h、6h、12h、18h 里程碑审计；
- 两台 Rust 的完整 leader range 审计；
- 关键日志审计，区分 GossipSub `Duplicate` 去重与真实 ERROR；
- 结束后做七端点只读 EVM/RPC 对比和一次 Rust 重启追高演练。

监控进程意外退出、上游漂移、二进制被替换或日志被截断，都使当前正式窗口失效；修复后必须
用新 `RUN_ID` 重新计时。不得拼接两段运行时间宣称 24 小时连续通过。

### 8.3 混合 RPC 交易

纯共识窗口完成后再进行交易测试，交替把已离线签名、nonce 连续且 chain ID 正确的交易发给
Gov5 与 Rust RPC。所有端点检查交易、receipt、部署代码、storage、余额和 nonce，再观察至少
3 分钟继续出块。禁止为了“补结果”重发已 finalized 的交易。

Gov5 与 reth 的 JSON 展示可能不同，例如 `rewards`、`verifier`、`totalDifficulty`、
`sha3Uncles`、`size`，以及 storage 的最短十六进制形式与 32 字节补零形式。跨客户端门禁应
比较共识/执行语义：canonical hash、root、receipt、code、余额、nonce，以及 storage 的数值
word；不能把纯展示差异误判为状态分叉，也不能因此放松 canonical 字段比较。

## 9. 故障处理

| 现象 | 首要检查 | 处理原则 |
|---|---|---|
| 节点启动即拒绝 | genesis、manifest、bootstrap receipt、QMDB lineage | 不绕过 fail-closed；重新取得匹配网络包或完整快照 |
| 有 RPC、没有 peer | PeerId、multiaddr 协议、NAT/防火墙、静态 peer | 修正清单和连通性，不临时开启未知 peer 自动发现 |
| 能跟随但不投票/出块 | participant 开关、BLS 槽位、静态委员会、追高状态 | 先保持 observer，确认身份和 QC 后重新安排切换 |
| 高度增长但 Rust 未出块 | miner 轮值、leader 日志、槽位顺序 | 不能判 PASS；检查槽位映射和 H2-v4 ingress |
| 七端点短暂不同高 | 是否串行采样、最小公共高度 hash | 并发重采样；只要超出门限或公共高度不一致立即失败 |
| `Syncing`/追高循环 | execution 与 consensus 水位、catch-up buffer | 使用匹配快照；不要只复制 reth 数据库 |
| equivocation 非零 | 是否发生身份重叠、重复进程或重复密钥 | 立即隔离相关验证者、保存证据，停止资格计时 |
| 上游提交变化 | Gov5 remote/main 守护记录 | 当前窗口作废，审计变更、重编译、重新开始 |
| 资源持续上升 | RSS、线程、FD、缓存逻辑计数趋势 | 保存证据并定位泄漏，修复后重新跑完整窗口 |

回滚验证者替换时严格反向执行：停止 Rust、确认密钥和端口释放、恢复原 Gov5 数据目录、启动
Gov5、等待重新追高并复核公共高度。任何时候都不能通过同时启动两个同身份进程来“抢恢复”。

## 10. 最终验收清单

- [ ] 网络包和 `MANIFEST.sha256` 已由 Gov5/N42 双方复核；
- [ ] 所有节点的 chain ID、创世哈希、fork、H2-v4 和验证者顺序一致；
- [ ] 二进制提交和 SHA-256 已固定，窗口内无上游漂移；
- [ ] Rust 先以 observer 追高，QMDB lineage 和公共高度 identity 通过；
- [ ] 替换槽位无身份重叠，equivocation 为 0；
- [ ] Gov5 与 Rust 均按预期槽位出块，七端点 canonical hash 一致；
- [ ] `validatorCount`、committed QC、法定票数和 parent chain 连续性通过；
- [ ] 30 分钟窗口通过后，新的连续 24 小时窗口通过；
- [ ] Go/Rust 混合入口交易在七端点执行结果一致，交易后继续稳定出块；
- [ ] Rust 重启追高、单节点滚动恢复和回滚流程已演练；
- [ ] 证据、日志、manifest、运行编号和最终 PASS 摘要已归档。

## 11. 仓库内参考

- [`scripts/gov5-interop-qualification.sh`](../scripts/gov5-interop-qualification.sh)：5+2 启停、头部监控、leader、资源、交易和 soak 审计；
- [`scripts/gov5-existing-seven-qualification.sh`](../scripts/gov5-existing-seven-qualification.sh)：既有七节点 observer → participant 的受控替换流程；
- [`docs/devlog-133-h2-v4-mixed-participant.md`](devlog-133-h2-v4-mixed-participant.md)：H2-v4 混合参与者安全边界；
- [`docs/devlog-117-h2-v4-production-profile.md`](devlog-117-h2-v4-production-profile.md)：Gov5/N42 生产 profile 与 wire contract；
- [`docs/devlog-134-gov5-live-voter-and-catchup-watermark.md`](devlog-134-gov5-live-voter-and-catchup-watermark.md)：投票门控与追高水位修复。
