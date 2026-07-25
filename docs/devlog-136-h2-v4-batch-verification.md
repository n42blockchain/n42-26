# devlog-136：H2-v4 随机系数批量验签

## 背景

H2-v4 已把 Native 与 Gov5 的签名域彻底分开，但 R1/R2 入站批次、QC 构造和 TC
构造仍逐签名调用 `verify_h2_v4_prevalidated`。委员会扩大或 Rust 验证者占比提高后，
这会失去 devlog-101 已验证过的 multi-pairing 收益。

## 实现

`n42-primitives` 新增 H2-v4 专用的
`batch_verify_h2_v4_prevalidated_with_fallback`：

- 使用 Gov5 POP ciphersuite 的 `H2_V4_DST`，不复用 Native 的 NUL 域；
- 每个 `(message, signature, public key)` 由系统随机 64-bit 非零系数加权后进入
  `blst::verify_multiple_aggregate_signatures`；
- 批量成功时不再逐签名验签；
- 批量失败时才逐个使用 H2-v4 域验签，返回精确的坏签名位置；
- 空批次、长度不匹配和 10,000 条上限继续 fail closed。

`ConsensusSigningProfile::batch_verify_with_fallback` 根据 profile 选择 Native 或 H2-v4
批量原语。以下三条原先的 H2-v4 逐签名路径现统一走该入口：

1. R1/R2 入站队列的批量认证；
2. `VoteCollector` 构造 QC 时对未预验签尾部的认证；
3. `TimeoutCollector` 构造 TC 时对未预验签尾部的认证。

聚合 QC/TC 的最终验签仍使用各 profile 原有的 fast aggregate verify；本改动只消除
聚合前未认证单签名的线性 pairing 路径。

## 安全边界

- H2-v4 签名在 Native 域下仍验证失败，域隔离不变。
- 随机系数在原语内部生成，签名者不能构造相消的无效签名。
- fallback 精确剔除坏位置；剩余票数不足 `n-f` 时 QC/TC 仍拒绝生成。
- H2-v4 批量入口只用于已通过 `ValidatorSet` 验证边界的公钥，fallback 不重复做
  subgroup 校验。

## 验证

- H2-v4 原语正向批次、跨域拒绝、两个坏签名精确定位、长度不匹配：通过。
- H2-v4 入站 R1/R2 批次的错 key 精确隔离：通过。
- H2-v4 QC 四票含一坏票时保留三票形成合法 quorum，并清除坏 signer bit：通过。
- `n42-consensus` 221 单元测试、12 个七节点 chaos 测试、67 个集成测试：零失败。
- `n42-primitives` 47 单元测试：零失败。
