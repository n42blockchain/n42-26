# 2026-09-14：四节点原生 H2/QMDB 创世引导

已生成独立四节点材料，真实 CLI 成功重建并持久化 152,004 个账户的原生 QMDB
创世状态，识别四人 H2-v4 roster、validator 0 和 QMDB-only 读取；随后在网络构造处
被当前环境的 `Operation not permitted` 拒绝。四节点启动预检同样在 socket 创建处失败，
未启动节点，未生成运行记录。没有 committed 区块或新的 TPS 成绩。

## 本轮实现

新增 `n42-native-fleet` 工具，输出目录必须不存在。四份随机 BLS/Ed25519 身份、
共识配置、可信验收 roster、Genesis JSON、原生创世头、finalized-range frame 和
完整 leaf-form QMDB 快照一同生成；密钥文件为 0600，目录为 0700，manifest 最后写入。
BLS 地址采用 Gov5 `cmd/hotstuff-testnet` 的公钥前 20 字节规则。发送者沿用本仓
`keccak256("n42-test-key-{i}")` 私钥，便于复用压测器。

本次参数：chain ID 941004，四验证者、f=1、quorum=3、静态 roster（epoch 1000），
5000 发送者、147000 独立收款人、四个预存余额的手续费账户；Cancun 激活，Prague 未激活。
收款人是 `last20(keccak256("n42-recipient-{i}"))`，生成时检查重复和发送者冲突。
200 ms slot、22 万笔区块上限、50 亿 gas、200 ms 构建预算是初始待测参数，不是测量结果。

复用 `chain94-fleet.py` 的进程归属校验、端口预检和显式环境传递，原七节点快照准备逻辑保留。
生命周期命令现在识别新四节点 manifest，并要求创世高度 0、Genesis 与共识 roster
逐项一致。旧签名快照不能截成四节点。原七节点启动继续采用原 Prague 时间；新配置明确
不注入它。新四节点还固定 builder gas target、开启独立 ingest 端口，记录交易上限与预算。
验收脚本新增 RPC/ingest/metrics 端口基址参数，默认值不变。

## 冷启动边界修复

固定 Gov5 `99f1039661f22362d91a9af2e49ecb98bc9c2d04` 的
`internal/genesis_block.go` 写入 Cancun 创世字段，但不写 mobileRegistryRoot。
本仓原 normalizer 只凭父头 mobile 字段选择原生路径，导致冷启动的第一块走旧分叉路径，
删掉 withdrawals/Cancun 标记。新增测试先复现 `blob_gas_used: None != Some(0)`，再修复
完整 Cancun 创世父头的选择。后续原生父头规则与非零 mobile root 的拒绝规则保留。
20 项 Gov5 block 回归通过，包括原生奖励/committee link、旧分叉和错误编码场景。

## 实际材料及验证

运行材料在忽略目录 `.artifacts/native-four-20260914/`，未把私钥收入源码或证据归档。

- Genesis：`0x9e94c7fa149c40589c37184ce38a28bd7c8e4d3816fc2d7dedb1d370cca9b8d5`
- QMDB：`0x1310a80b8aaa0663766cc8737b2f5ba3c555f84f79c27dd3e648a21634564d26`
- 源数据 152,004 live slots、75 twigs，账户/配置/快照 SHA-256 全部匹配 manifest。
- 既有 H2 验收器接受生成的可信四人 roster；八项启动器测试通过，覆盖新四节点参数与旧七节点。
- Workspace all-targets Clippy `-D warnings`、本轮 Rust 文件格式、shell 语法、diff 检查通过。
- CLI 隔离目录 `/tmp/n42-four-cli-bootstrap` 成功验证原生 root、写入 base 文件、启用 Only
  读取；网络创建失败。此检查使用 devlog170 完成的二进制，验证的是未变的 CLI 引导代码；
  第一块 normalizer 修复的生产二进制正在另行编译，状态以证据 manifest 为准。

[证据](benchmarks/20260914-native-four-genesis/manifest.json) 包含生成、失败复现、通过日志、
真实 CLI 引导日志、启动拒绝日志、公开 manifest/roster 和本轮源码摘要。UTC 日志跨到 9 月 15 日，
本地时区仍为 9 月 14 日。新增依赖只引用现有锁定版本，没有升级依赖。

在包含本轮修复的 Release 构建完成后，且执行环境允许 TCP/UDP 监听时，已有材料可启动：

```bash
python3 scripts/chain94-fleet.py --runtime "$PWD/.artifacts/native-four-20260914" start \
  --binary "$PWD/target/release/n42-node" --fast-transfers --parallel-build --parallel-import --qmdb-reads only
python3 scripts/chain94-fleet.py --runtime "$PWD/.artifacts/native-four-20260914" verify --seconds 30 --min-blocks 8
```

这是启动/出块验证；1M TPS 仍需带交易验收。下一项给压测器接入独立收款人文件和正确的
发送者/nonce 分布。目前默认压测器以所有发送者为收款人，而且各发送者按相同 nonce
选择同一收款人；会形成大依赖组和高度重复的收款账户，不能把它当作已修正负载。
旧链预签名文件不能直接给 chain 941004 使用，须按本链重签。

新 RPC/ingest/metrics 基址分别为 23400 / 34400 / 23600，可信配置是运行目录中的
`trusted-config.json`。最终门槛仍为四节点 H2 committed 成功交易、原生头/收据根及
持久化 QMDB-only 读取证据；当前环境的网络限制使这一验证尚未发生。
