# devlog-186：3 Gov5 + 4 n42-26 全新创世混合舰队（2026-11-17）

路线归属：[自定义链兼容主线](04-Product-Lines-and-Performance.md)，ETH ECDSA 交易、H2 共识、QMDB bin 状态承诺。
编号跳过 179–185，这些编号已被 `feat/native-port` 分支占用。

## 目的与范围

此前混合验收（devlog-141、chain94 三工程持续运行）中 n42-26 只占 1 席。本轮在单机 macOS arm64 上
验证 n42-26 占多数的委员会：验证者 0–2 为 Gov5（`dda577f8`），3–6 为 n42-26（`883770f` + 本提交），
固定七人、quorum 5，全新创世，不使用 replay 数据。

这是单机 loopback、轻负载的正确性检查，不是性能数据，也不替代多机或长时间验收。

## 创世与启动方式

- 创世由 Gov5 `n42 init --chain private` 载入：London 于 0 激活（无 Shanghai/Cancun），
  `consensus: hotstuff`、`stateScheme: qmdb`、`hotstuff.interopV4: true`、`epochLength 0`、
  `period 1`、`baseTimeout 6000`、`maxTimeout 30000`，无 `devBlockReward`、无 `committeePool`。
- n42-26 从 Gov5 导出的第 0 块启动：`scripts/gov5-export-genesis-range.go` 导出 range，
  `n42-qmdb-export` 导出 portable checkpoint；环境为 `N42_GOV5_H2_PARTICIPANT=1`、
  `N42_GOV5_HEADER_PROFILE=1`、`N42_GOV5_QMDB_EXECUTION=1`、`N42_QMDB_BOOTSTRAP` 及
  `N42_QMDB_BOOTSTRAP_{BLOCK=0,BLOCK_HASH,ROOT}`，不设 `LEGACY_SIGNING` 与 `NATIVE_PRODUCER`。
- 七个验证者均使用 secp256k1 传输身份；双方以 TCP 静态互联，`consensus.json` 为全部七人绑定 `p2p_peer_id`。
- 没有选用 `n42-native-fleet` 生成的 Cancun 创世：按代码阅读，Gov5 当前的 `MobileRegistryRoot`
  头布局与本工程不一致，该组合未实测。

## 发现的问题

### 1. pre-Shanghai 配置下 leader 无法广播自己的块（已修复）

现象：轮到 n42-26 出块时日志报
`normalized native header is unavailable for direct block broadcast`，提案被放弃，链停在 Gov5 出的第 1 块。

原因：`029377a` 之后 leader 在广播前要求从原生区块头注册表取到本块；
`normalize_execution_payload_for_gov5_h2` 只有 native（Cancun/移动锚点）分支会注册，
pre-Shanghai 分支及"已规范化"提前返回分支都不注册。此前混合验收使用 chain94 native 分支，未覆盖该路径。

修复：两条 pre-Shanghai 路径均调用 `remember_pre_shanghai_native_header`。该配置下头部不含 Gov5 专有字段，
原生编码与 alloy 编码一致，哈希相同。新增回归测试
`pre_shanghai_leader_payload_is_registered_for_direct_broadcast`。

### 2. 零小费交易使两端 QMDB 根不同（配置问题，新增启动告警）

现象：一笔 gasPrice 等于 base fee 的普通转账进入交易池后，Gov5 leader 的块被 n42-26 以
`mismatched block state root` 拒绝，n42-26 leader 的块得不到 Gov5 投票；持有该交易的 leader 全部超时。
没有分叉，已提交块始终一致；以同 nonce 的正小费交易替换后恢复。

原因：Gov5 在未启用 `unchangedAccountNoOpTime` 时，会为"首次访问是零额 AddBalance"的账户
（零小费块的 coinbase）追加一个值未变的 QMDB 叶；Gov5 `cf051d3b`、`c987bd50` 已提供该规则，
但创世未设置时保持关闭。用 `N42_QMDB_TRACE_OPERATIONS=1` 实测，本工程在零小费块中只写发送方与接收方两个账户
（coinbase 余额为零和非零两种情况均如此），即本工程的行为等同于规则开启。

处理：混合链创世必须在 `config` 中加入 `"unchangedAccountNoOpTime": 0`。
新增 `gov5_unchanged_account_rule_active_from_genesis`；H2-v4 participant 以 QMDB 执行启动而创世未从 0
激活该规则时输出告警。没有改为拒绝启动，因为 chain94 既有配置不含该字段。
本工程没有复刻 Gov5 的旧行为：它取决于账户在块内的首次访问方式，revm 不暴露这一信息。

### 3. 其他

- `scripts/gov5-export-genesis-range.go` 头部注释有两行损坏，导致无法编译；已修复。
- `scripts/apply-reth-patches.sh`、`check-reth-source.sh` 依赖 bash 4（`declare -A`、`mapfile`），
  macOS 自带 bash 3.2 无法运行；本轮按脚本顺序手工应用六个补丁。未修改脚本。
- Gov5 兼容层把空回执的 `logs` 返回为 `null`，`cast send` 解析输出会报错，交易本身成功。
- 发给 n42-26 RPC 的交易在 `N42_DISABLE_TX_FORWARD=1` 下只能等该节点自己当 leader 才上链。

## 实测结果

第一条链（chain id 4207，未设置规则，含修复 1）：

| 项目 | 结果 |
|---|---|
| 七节点出块 | 七个验证者均出块；1–614 块在七节点上 hash、stateRoot、receiptsRoot、transactionsRoot 一致 |
| 停 1 个 n42-26 / 停 1 个 Gov5 | 其余 6 个 40 秒各提交 20 块；重启后 6 秒 / 4 秒追齐 |
| 停 1 Gov5 + 1 n42-26（5 在线） | 120 秒提交 20 块 |
| 再停 1 个（4 在线） | 高度 457 保持 60 秒不变 |
| 恢复第 5 个 | 8 秒后恢复提交 |
| 整队正常停止并重启 | 停机前锚点块在七节点一致；随后 60 秒提交 60 块 |
| `kill -9` 1 Gov5 + 1 n42-26 | 其余 5 个 90 秒提交 15 块；重启后 6 秒内追齐 |

演练共 17 项检查全部通过，全程带轻量转账负载。

第二条链（chain id 4208，创世含 `unchangedAccountNoOpTime: 0`）：
34 笔零小费或零金额交易（零小费转账、零金额转账、转给不存在地址、自转）全部成功，
带交易的块由 Gov5 与 n42-26 双方出过；1–153 块七节点一致。该链未重跑停启演练。

## 未覆盖

- Cancun 创世、合约类交易（自毁、回滚、零值调用、改后复原的存储）在两端的一致性。
- 多机、公网延迟、较高负载、断电。
- 修复 1 的回归测试未做"去掉修复后失败"的反向确认；缺陷本身已在实机复现。

## 回归

- `cargo clippy -p n42-network -p n42-node -p n42-node-bin --all-targets -- -D warnings` 通过。
- `cargo test -p n42-network --lib`：222 通过 / 0 失败 / 1 声明忽略。
- `cargo test -p n42-node --lib qmdb_state`：68 通过 / 0 失败 / 6 声明忽略。
- 未运行完整 workspace 测试与 E2E。
