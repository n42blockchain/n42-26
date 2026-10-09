# H2 提交证书导出与独立验证工具

日期：2026-09-14，接续 [devlog-158](devlog-158-h2-native-header-proof-20260914.md)。
保持 H2 共识、二叉树 QMDB 状态读取与现有执行/持久化校验。
**4 节点 1M 成功提交 TPS 仍未实测达标。** 本轮完成可运行的提交证书验证工具，
尚未将它接入资格脚本的强制门槛。

## 不能混用 QC 的签名对象

审阅 `protocol/quorum.rs`、`voting.rs`、`decision.rs` 以及 shared consensus state：

- prepare QC 与 commit QC 使用不同消息。提交验收必须调用 commit-only 检查，
  不能使用 `verify_qc_any_domain_with_profile` 的 prepare/commit 任一通过逻辑。
- Native commit 签名包含 validator-changes hash；H2-v4 还包含 phase、chain ID
  和 genesis hash。Gov5 legacy 的 commit 消息没有显式链身份域。
- Gov5 两种配置固定使用零 changes hash，当前协议不支持在其中搬入动态变更语义。
- 对祖先块的提交推导，需要认证后代和真实 parent 链；不能修改已签 QC 的 block hash
  来让它看起来直接签了祖先。该工具只验证请求指定的 exact certified block。

## 原子导出 QC

`n42_consensusStatus` 新增可空的 `commitQc`，含 `view`、`blockHash`、`signature` 和
按验证者 index 排列的 boolean `signers`。这些字段与原有 view/hash 从同一个
`Arc<Option<QuorumCertificate>>` 快照取得，不重新读取另一个可能已推进的 QC。
无 QC 时为 null。显式 bit 数组避免外部消费者误解 Msb0 或 padding。

这是导出实际状态，不是服务端新增一个“verified=true”声明。验证者名单和 epoch
是另一个上下文；调用方仍需认证对应 QC 的名单，不能把当前名单自动套到历史 QC。

## 独立进程验证器

新增 `n42-verify-commit`，不改 root manifest/lock 依赖，读取 stdin 的一份 JSON
请求，输出 JSON。成功退出 0；格式或验证失败输出 `verified:false` 并退出 1。
输入上限 1 MiB，支持 4–1024 个验证者。

```sh
cargo build --release --bin n42-verify-commit
target/release/n42-verify-commit < trusted-request.json
```

请求字段：

| 字段 | 含义 |
| --- | --- |
| `profile` | 明确选择 `native`、`h2v4` 或 `gov5legacy`，没有自动跨域尝试 |
| `chainId`、`genesisHash` | H2-v4 签名身份 |
| `validators` | 按 index 排列的受信 BLS 公钥，48-byte hex |
| `faultTolerance` | 受信配置中的 f；验证 n > 3f，quorum 为 n−f |
| `validatorChangesHash` | Native 的实际变更承诺；Gov5 必须为零 |
| `expectedView`、`expectedBlockHash` | 要验证的非 genesis 边界身份 |
| `qc` | RPC 导出的 view/blockHash/signature/signers |

验证器拒绝不匹配的 view/hash、view 0 sentinel、非法或重复公钥、非 boolean 或
长度错误的 bitmap、不足 quorum、非法签名及错误配置。构造 ValidatorSet 时保留
公钥 subgroup/非 infinity 检查，最终只验证 commit 域的聚合签名。

输出包含实际签名消息、验证者顺序哈希、signer 数及 `chainBoundSignature`。
后者只在 H2-v4 为 true；Native/Gov5 legacy 的这个签名格式本身不显式绑定链身份。
调用方必须保存原始请求和受信配置，不能只保留一个 `verified:true`。

该程序在验证者进程之外运行，但复用生产的签名消息构造、BLS 库和 commit verifier，
不是第二套密码实现。它不证明输入名单来自可信 genesis/epoch，也不单独证明链的
执行、提交发生的墙钟时间或具体 TPS。不能直接信任被测节点任意给出的新名单。

## 验证

- Rust verifier：3 项测试通过，1 个显式 fixture 导出测试默认 ignored。
  覆盖三种正确 commit 域、三种 prepare 签名拒绝、H2-v4 错链/错 genesis、
  Native changes hash、错 quorum/bitmap/key 顺序、重复公钥及 genesis sentinel。
- 使用实际构建的 CLI 跑 **27 个用例通过**，另覆盖坏 JSON、超长输入及非法公钥。
  三种 commit fixture 返回 0，prepare 与篡改用例返回 1。
- node RPC：18 项测试通过，包括同一快照字段和 JSON bitmap 序列化。
- workspace all-targets clippy `-D warnings`、`git diff --check` 通过。
- [源码、合成签名请求、真实 CLI 结果和复现脚本](benchmarks/20260914-h2-commit-qc/)。

测试密钥由显式测试代码生成，仅用于合成 fixture；没有读取生产私钥或向节点发送
消息。仍使用固定补丁 reth `23316e3ff8adca8c3bd5085ff0565fcae019202a` 验证。

## 下一步验收缺口

资格脚本目前仍使用 schema 4 的原始头部/回执门槛，尚未强制调用此工具。
下一步要把受信验证者配置和签名配置固定到捕获文件，核验开始/结束的真实 QC，并
明确处理证书块与共同区间的祖先关系；不能只增加一个可被忽略的验证输出。
交易根与业务状态、真实四节点连续窗口、重启追赶及耐久证据也仍未完成，目标继续进行。
