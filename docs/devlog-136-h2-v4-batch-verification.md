# devlog-136：H2-v4 批量验签（2026-07-25）

## 背景

devlog-101 给 Native 共识加了随机系数批量验签（500 节点 QC 351.0ms→137.3ms，2.56x）。
混合客户端参与者模式引入 `ConsensusSigningProfile::H2V4` 之后，QC 构建与入站消息
验签这两条路径在 H2-v4 下都退化成逐签名 `verify_single`——批量优化只覆盖 Native。
devlog-135 把这条记为 MEDIUM：7 节点无碍，但 Rust 验证者占比或委员会规模一上去
就会变成瓶颈。本次补齐。

## 设计决策

### 为什么不是给 `batch_verify` 加一个 DST 参数

最直接的做法是把 DST 提升为函数参数。但批量验签有两条路径：多重配对的批量本体，
以及批量失败后逐个定位坏签名的 fallback。两者必须用同一个域——否则批量在 A 域失败、
fallback 在 B 域逐个"验证通过"，函数会返回"没有坏签名"，调用方于是把一批域不符的
签名全部收进 QC。这不是理论问题：`verify` 与 `verify_h2_v4_prevalidated` 是两个
独立函数，传错 DST 却仍调用 `verify` 是一行之差。

所以把域和单签名校验函数绑成一个 `Ciphersuite` 值，两个常量 `NATIVE` / `H2_V4`
各自成对定义，批量与 fallback 都从同一个值里取。传错域这件事在类型层面就不成立了。

### 为什么 H2-v4 的 fallback 用 prevalidated

`NATIVE.verify_one` 是 `BlsPublicKey::verify`（重新校验公钥子群），
`H2_V4.verify_one` 是 `verify_h2_v4_prevalidated`（不校验）。看起来不对称，实际是
各自路径的既有语义：Native 的 fallback 历来做完整校验，而 H2-v4 的公钥只可能经
`ValidatorSet::try_new` 进入，那里已经 `validate()` 过，与 H2-v4 其余全部验签路径
（`verify_single`、`verify_h2_v4_aggregate`）保持一致。批量本体两者都传
`pks_validate = false`，靠随机系数防 rogue-key，这一点没有区别。

## 实施

`crates/n42-primitives/src/bls/verify.rs`
- 新增 `Ciphersuite { dst, verify_one }` 与 `NATIVE` / `H2_V4` 两个常量。
- `batch_verify` / `batch_verify_with_fallback` 改为薄封装，内部走
  `batch_verify_with_suite` / `batch_verify_with_fallback_suite`。
- 导出 `batch_verify_h2_v4` 与 `batch_verify_h2_v4_with_fallback`。

`crates/n42-consensus/src/protocol/quorum.rs`
- `ConsensusSigningProfile::batch_verify` 按 profile 分派，三个调用点的 `match`
  分支消失，各减少一段逐签名回退代码：QC 构建（`VoteCollector`）、TC 构建
  （`TimeoutCollector`）、状态机入站 R1/R2 批量认证。

## 与真机侧并行实现的合并

真机侧同一天独立完成了同一项工作（`feat/gov5-h2v4-batch-verify`，
`batch_verify_h2_v4_prevalidated_with_fallback`），两版功能等价。合并取舍：

- **取本版的 `Ciphersuite` 绑定**：真机版把域与 H2-v4 标志作为两个独立参数
  （`batch_verify_with_domain(..., dst, h2_v4: bool)`），存在 `(DST, true)` 这类
  自相矛盾的组合；绑成一个值后不可表达。真机版的 fallback 另起一个函数，重复了
  长度检查、批量上限、空批次与回退循环约 25 行，本版共用同一实现。
- **取真机版发现的第三个调用点**：本版最初只改了 QC 构建与状态机入站两处，
  漏掉了 `TimeoutCollector::build_tc_with_profile`——TC 构建有一段结构相同、
  元组多一个字段的逐签名回退代码，grep 时被漏过。已补齐，并补一条 TC 专属回归
  （四票含一张错 view 签名，TC 仍成立且坏 signer 的 bit 保持为 0）。

## 实测

`cargo run --release -p n42-primitives --example h2_v4_batch_probe`（Windows，
release）：

| 委员会规模 | 逐签名 | 批量 | 加速 | 批量+fallback 封装 |
|---|---|---|---|---|
| 7 | 3.69 ms | 0.76 ms | 4.84x | 0.85 ms |
| 21 | 10.06 ms | 1.53 ms | 6.60x | 1.32 ms |
| 100 | 46.71 ms | 3.04 ms | 15.36x | 2.47 ms |
| 500 | 230.53 ms | 9.94 ms | 23.18x | 10.16 ms |

这是**纯验签微基准**，不是端到端 QC 构建时间——devlog-101 记录的 Native 端到端
2.56x 包含聚合、bitmap、序列化等固定开销，验签只是其中一部分。可以确定的是验签
部分本身随规模从 4.8x 涨到 23x。

devlog-135 当时写的"7 节点无实际影响"偏保守：7 节点也省下约 2.9 ms。在 8 秒 slot
预算里仍然可忽略，但结论应以数据为准而不是估计。

## 回归覆盖

`bls::verify` 新增四个测试：

- `h2_v4_batch_accepts_h2_v4_signatures`：正路。
- `the_two_ciphersuites_reject_each_other_in_batch`：Native 签名进 H2-v4 批量、
  H2-v4 签名进 Native 批量，都必须返回**全部位置**为坏——这条正是上面那个
  "fallback 用错域" 失效模式的守门测试。
- `h2_v4_single_element_batch_uses_the_h2_v4_domain`：单元素走的是另一条代码路径
  （跳过随机系数直接单验），需要独立确认域。
- `h2_v4_fallback_identifies_exactly_the_bad_positions`：坏签名定位精度不因换域而
  退化，混入一个"消息正确但域错误"的签名同样被抓出。

`protocol::quorum` 新增 `h2_v4_timeout_certificate_drops_only_the_bad_signature`：
TC 构建走的是与 QC 相同的批量路径，因此需要同样的保证——坏签名被精确剔除而不是
让整批失败，且其 signer bit 保持为 0。

`cargo clippy --all-targets -- -D warnings` 零告警；`cargo test --workspace`
46 套件零失败。

## 状态

`docs/codex-plan-gov5-interop-completion.md` 的 T7 完成。分支
`perf/h2-v4-batch-verify`（基于 `integration/gov5-interop-main`）。

与 P4/P6 真机窗口无关——H2-v4 批量验签是共识内部的验签实现，不改变任何 wire 格式、
签名域或跨语言向量，可随 T2 一并纳入，也可单独合入。
