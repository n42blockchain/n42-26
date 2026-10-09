# H2-v4 受信提交证书成为吞吐验收门槛

日期：2026-09-14，接续 [devlog-159](devlog-159-h2-commit-verifier-20260914.md)。
**4 节点 1M 成功提交 TPS 仍未实测达标。** 保持 H2 共识、二叉树 QMDB
状态读取、正常重执行和持久化校验；本轮补齐验收中的证书与祖先链绑定。

## 强制受信配置

`scripts/h2-tps-audit.py` 的 capture/audit 都要求 `--trusted-config`。
`scripts/qualify-1m-tps.sh` 从 `N42_BENCH_TRUSTED_CONFIG` 复制该文件到产物目录，
在压测前固定配置。没有配置、没有验证器或节点无法提供有效 commit QC 时停止。
绝不从 `n42_validatorSet` 自动生成受信名单。

配置格式（占位符必须替换为实际受信配置，不能用测试 fixture 的密钥）：

```json
{
  "schema": 1,
  "profile": "h2v4",
  "chainId": 94,
  "genesisHash": "0x<32-byte actual genesis hash>",
  "faultTolerance": 1,
  "validatorChangesHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "validators": [
    "<48-byte public key at index 0, hex without 0x>",
    "<48-byte public key at index 1, hex without 0x>",
    "<48-byte public key at index 2, hex without 0x>",
    "<48-byte public key at index 3, hex without 0x>"
  ]
}
```

chainId 94 只是格式示例，必须使用部署链的值。操作者负责认证 genesis、签名配置
和验证者顺序；配置文件的存在和哈希不是可信来源证明。公钥不包含私钥。
验收限定静态四验证者 H2-v4、f=1、零 changes hash，要求签名绑定 chain ID 和
genesis。Native/Gov5 legacy 仍可通过独立工具诊断，但不能进入本资格门槛。

```sh
cargo build --release --bin n42-keccak --bin n42-verify-commit --bin n42-stress
N42_BENCH_TRUSTED_CONFIG=/path/to/authenticated-h2v4.json \
  scripts/qualify-1m-tps.sh
```

## 八份边界证书与真实祖先关系

- 捕获保存原始 QC、受信规范化配置及 SHA-256、验证器二进制 SHA-256、实际验证
  请求和结果。view/hash 必须与同一 status 中的报告及原始头部身份一致。
- 开始/结束四节点共八份 QC 在 audit 时重新调用真实 `n42-verify-commit`，
  不信任捕获文件中的 `verified:true`。prepare 不会走 commit-or-prepare 回退。
- 验证器缺失、失败、输出无效、捕获间或审计期间二进制变化、名单顺序或链身份
  不一致，都不能通过。规范化配置哈希不包含未知注释字段；原文件另存。
- 计数仍从最高开始高度之后到最低结束高度，分母包含完整捕获时间，包括捕获时
  的证书验证时间，按共同成功交易计一次，不把四节点相加。
- 对每节点，验证其开始证书块到共同开始块的原始 parent 链，以及共同结束块到
  其结束证书块的原始 parent 链。保存这些额外头部，不能按高度推断祖先提交。

边界/报告升级为 schema 5，scope 为
`h2v4_commit_qcs_qmdb_only_native_headers_and_receipts`，拒绝 schema 4 及更早
记录。现有回执根、头部哈希、QMDB-only/WAL、计数器和进程身份门槛全部保留。

## 验证与可复现产物

- Python 离线验收 **43 项通过**；仅替换 RPC 传输为内存 fixture，头部 Keccak 和
  BLS 验证均使用真实构建的可执行程序，没有 mock 密码检查。
- 新增八项测试覆盖八份证明归档、四节点分别缺失/坏 QC、无受信配置、错链/顺序/
  f/profile、证据篡改、验证器失败、共同区间上下两侧断链、极低阈值仍拒绝坏签名。
- Rust verifier：3 项通过、2 个显式 fixture 导出测试默认 ignored。
  新导出器只存在于 `#[cfg(test)]`，生产 CLI 不提供签名模式。
- workspace all-targets clippy `-D warnings`、shell 语法和 diff 空白检查通过。
- [源代码、合成证书、边界文件、完整报告与日志](benchmarks/20260914-h2-trusted-qc-gate/)。
  合成正常案例是 9/63 TPS；降低到 0.1 的门槛仅测试控制流，绝非 1M 实测。

编译仍使用固定补丁 reth `23316e3ff8adca8c3bd5085ff0565fcae019202a` 的干净依赖树。
相邻开发 reth 未修改，生产密钥未读取，未发送节点消息。

## 仍需推进

此门槛证明的是“按操作者受信 H2-v4 配置认证的证书和原始链/回执绑定”。它不认证
信任文件来源，不提供第二套独立 BLS 实现，不重执行 EVM，不重建交易 trie root，
也不证明所有状态读取走 QMDB 或硬件耐久。原始头部注册表仅 8,192 项且不持久化，
仍限制长窗口/重启后取证。真实四节点连续 1M 成功 TPS、重启追赶和业务状态验证
仍待完成；目标继续进行。
