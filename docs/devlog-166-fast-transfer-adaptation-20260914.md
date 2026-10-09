# 接入 n42-rs 普通转账执行快路径

日期：2026-09-14，接续 [复用计划](devlog-165-fleet-records-and-reuse-plan-20260914.md) 的 P1。
这次修改接入真实 EVM factory；没有新的四节点 TPS 成绩，目标继续进行。

## 实现

从 n42-rs `466c1839791afadba7f1dd4d48c5518440dcafba` 的
`crates/n42/engine-types/src/fast_transfer.rs` 适配普通转账状态转换和原有差分用例，
新增 `crates/n42-execution/src/fast_transfer.rs`。

`N42EvmConfig::new` 通过本仓 `N42EvmFactory` 读取 `N42_FAST_TRANSFER=1`。
默认关闭，启用后在 Prague/Osaka 对满足资格条件的普通转账计算账户更新；其余交易
交给同一底层 REVM。`with_evm_factory` 支持在不修改进程环境的情况下做同配置对拍。
没有修改签名恢复/验证、H2、系统调用、提款、收据构造或 QMDB 根/WAL 规则。

builder 已将自己的 EVM config 传给 `default_ethereum_payload`，后者调用该 config
的 `builder_for_next_block`；follower 的 `BasicBlockExecutor` 使用同一个 factory。
这避免 rs 曾发生的“只在 follower 启用、builder 仍用默认 factory”问题。
本轮运行时用例覆盖 block executor；实际网络 payload-builder 行为仍需 fleet 验证。

## 相对上游的适配与补齐

- 保留本仓 `0x0302` randomness precompile 和创建 EVM 时的 prevrandao 设置。
  检查实际 precompile map，不使用“高地址一定不是预编译”的假设；支持动态高地址预编译回退。
- Typed 交易缺 chain ID、缺 prevrandao/blob 执行环境、自定义 gas schedule，以及提前开启
  Amsterdam state/intrinsic gas 的配置回退到解释器，保留实际 REVM 的结果或错误。
  这些条件是对本仓通用 factory 的补齐，未声称它们在上游 fleet 输入中已经造成事故。
- 不把 Prague/Osaka 的资格规则扩大到 Cancun/Amsterdam。配置资格在每个 EVM 创建时
  检查一次，避免逐交易重建 gas 参数表；公开 wrapper 不提供修改 cfg/block 的接口。
- 保留 nonce、余额、费用、gas 上限、code/delegation、账户存在性、三方重叠及溢出回退。
  inspector 开启时通过解释器；数据库读取错误直接传播，不能伪装成账户不存在。
- 命中与回退先在 EVM 内计数，完成或丢弃 EVM 时批量发布，避免所有执行线程逐笔竞争
  一个全局 atomic。指标为 `n42_fast_transfer_total` 和
  `n42_fast_transfer_fallback_total{reason}`；命中计数不表示 H2 已提交或交易已持久化。

## 复用已有运行工具

原生 `scripts/chain94-fleet.py` 会清除继承的所有 `N42_*` 变量，因此仅在命令前
export 新开关不会生效。本轮增加显式选项，并在清理环境之后传给所有节点：

```sh
python3 scripts/chain94-fleet.py --runtime /path/to/prepared/fleet start \
  --binary /path/to/n42-node --fast-transfers --qmdb-reads only
```

选项记录在 `run.json`；默认行为仍是 fast transfer 关闭、reader off。
该命令仍是原七人快照舰队，不能作为四人委员会配置。没有在本轮启动或修改任何旧舰队。
通用 `testnet.sh` 显式传递 `N42_FAST_TRANSFER`，资格工具的配置摘要记录该选项；
摘要仍是 launcher 配置，是否实际命中须读节点指标，不能用环境变量代替执行证据。

## 验证

- `n42-execution --lib`：115 passed，包括 19 项快路径测试。另以进程环境
  `N42_FAST_TRANSFER=1` 跑同一套，115 passed。
- Prague/Osaka × legacy/2930/1559 × 新/旧收款方 × 不同转账额对拍。
  新用例比较执行结果/验证错误及完整 `BundleState`，包含 originals、statuses、storage 和 reverts。
- 混合序列包含三次普通转账与两次 SSTORE 合约调用；命中数精确为三，完整结果和 bundle 相同。
  另覆盖本仓随机数预编译、动态高地址预编译、inspector 切换及注入数据库错误。
- 实际 `N42EvmConfig` + tracking block executor + QMDB-only provider：
  两种执行方式各连续三块、每块 32 笔，成功收据、nonce/余额/费用和独立算术 oracle 一致；
  三个 durable roots 完全一致，两条链各自关闭并重开 WAL 后检查所有相关账户。
  fallback provider 没有账户/槽，不能通过静默回退读到资金。
- 旧启动器回归：6 passed；新增用例截获全部七次 `Popen`，验证显式设置能覆盖旧环境，
  且不会继承 `N42_SKIP_TX_VERIFY=1`。没有真正启动进程或 socket。
- workspace all-targets Clippy `-D warnings`、shell 语法和 diff whitespace 检查通过。
- Release 连续区块测试：2 passed（原 Cancun 对照与新增 Prague 快路径对照），
  1 个显式性能实验保持 ignored；没有运行新的性能矩阵。

原始日志、固定上游源码和本轮源码摘要见 [验证目录](benchmarks/20260914-fast-transfer/)。
构建使用 devlog164–165 同一干净、固定补丁的 reth 副本及 `--locked --offline`；
工作区原有其他改动保留。

连续区块用例是本地执行/存储验证，synthetic headers 没有证明完整原生头、真实网络、
H2 finality 或设备掉电耐久性；Prague 用例未部署所有系统合约。
没有从这些小块 debug 耗时推算 TPS，也没有把上游速度写成本仓实测。

下一项继续 P2：适配 sender-group 并行执行与 bundle graft，复用上游 candidate-order、
账户重复触碰、reward/coinbase 和去重 revert 用例，并接回完整组块路径。
