# devlog-125 — Gov5 原生执行 commitment

日期：2026-07-21

分支：`feat/gov5-n42-live-interop`

基线：`main @ f904813`、Reth `2.4.1 @ c533db8`

## 目标与安全边界

把 Gov5 replay-v2 的 receipt 与状态 mutation 语义接入 Reth 的确定性执行验证，不使用
skip/defer，也不改变默认 Ethereum profile。现有七节点 datadir、QMDB checkpoint 与历史 range
保持只读，本阶段仍不开放投票。

## 原生 receipt root

- 将 Gov5 `hash.DeriveSha(block.Receipts)` 的原生算法提升到 consensus crate：逐 receipt 编码
  RLP `[status, cumulativeGasUsed, logs]` 后串接并 Keccak；它不是 Ethereum receipt trie。
- `Gov5H2` profile 先对原始 header 强制核对原生 root，再只在临时 header 副本中放入
  Ethereum receipt-trie root，复用 Reth 的 gas-used、logs-bloom、requests 与 BAL 验证。
- 默认 Ethereum profile 完全不变。回归测试证明同一执行结果在标准 profile 被拒、在正确
  Gov5 commitment 下通过，伪造 root 仍以 `BodyReceiptRootDiff` 硬拒。

## QMDB mutation 转换

- 增加 Gov5 精确键规则：账户 `Blake3(address)`，storage
  `Blake3(address || slot_be32)`，均无额外 domain。
- 增加 `StateAccount.MarshalV2` 等价编码：presence bitmap、nonce unsigned LEB128、balance
  最短大端、非空 code hash 32 bytes；零 hash 与 Keccak-empty 均按空代码处理。
- 每块 operation 在改变树以前按 32-byte key 排序；重复 key fail closed 且不产生部分写入。
- 将 Reth `BundleState` 转为账户与 storage operation。普通账户只输出实际变化；SELFDESTRUCT
  bundle 按 revm 的完整 storage 语义输出整地址 wipe，避免漏掉“值未变化但账户被销毁”的 slot。

## 当前能力与剩余阻塞

目前已经有执行结果到 Gov5 receipt/QMDB 输入的确定性、可测试转换边界，但尚未把 QMDB tree
安装为 Reth Engine Tree 的 state-root strategy。因此节点仍只能安全接入现有七节点做只读
observer/finality follower，不能宣称 execution follower，更不能参与共识。

下一步是在 Reth 2.4.1 官方 `StateRootStrategy` 接口上安装 branch-safe QMDB job，先从 runtime-02
genesis 连续执行 1-49 并逐块对拍 header root；之后补 checkpoint/ancestor PlainState hydration，
再扩到原七节点与至少 1000 连续块门禁。

## 合并后审计跟进

- QMDB membership proof 的公开验证入口改为强制接收 caller requested key，拒绝用合法 B proof
  回答 A 查询的 key substitution。
- upper path 折叠后要求 twig id 的所有高位均被消耗；仅改变未认证 slot 高位、保持路径方向位
  不变的伪造 proof 现在必定失败。
- H2 vote/timeout/new-view 的 HighTC 在对象编码边界显式执行与 decoder/gov5 schema 相同的
  4096-byte 总限制，公共 encoder 不再可能输出自身 decoder 必拒的 envelope。
