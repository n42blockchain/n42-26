# N42 原生交易与帧：第一批移植

日期：2026-10-08。对应 [N42 原生实时链计划](../performance-mainline-20261007.md) 的第一批。

## 已提交的实现

- `3ccdf1a`：新增 `n42-tx-types`，移植 `0x50` Ed25519 交易、N42 envelope、
  block/receipt primitive、存储编码、帧 root/认证规则、规范向量与原测试。
  原型基于已验证的 Reth 2.7 / Alloy 2.5 依赖源；来源 SHA256 见
  `crates/n42-tx-types/PORT.md`。私有 key cache 只复制解压后的公钥缓存，保持有界。
- `a4763a2`：新增 `AdmittedFrame::authenticate`。精确解码、检查 chain id 与
  数量/交易字节/证明数量边界，拒绝帧内重复交易；从实际解码内容计算 root 与 sender。
  有效配置的可信网关达到门槛才使用认证结果，否则批量验证交易签名。
  不可满足的网关门槛在证明检查前拒绝；即使认证帧也保留密钥和签名形态检查。
  输出是私有不可变数据，clone 共享同一交易数组。该 API 不是状态有效性或 canonical 判定。
- `852d284`：独立 Python 规范检查与原生帧入队认证微基准。
- `849a470`：帧 layout 长度使用 checked sum，拒绝溢出后再切片；有效 frame root 不变。

节点尚未改为 N42Primitives，原生网络解码器、帧队列、构块和直接执行还没有接到此库。
本批不启用任何节点开关，不声称已经实时接收/出块或实现共享执行。
网络读取必须在分配前限制 wire 长度；这里的认证 API 接受调用者已经持有的交易字节。
账户 nonce、余额、费用、队列信用与 canonical prune 留给精确父状态和帧队列检查。

## 已取得的验证结果

- `python3 scripts/check_native_tx_vectors.py`：3 个向量全部匹配。
  独立重算 RLP 字节、signing hash、Ed25519 签名、sender 和完整交易 hash。
  这是规范向量验证，不替代 Rust 移植代码的编译与测试。
- `python3 -m unittest discover -s scripts -p test_summarize_execution_probe.py -v`：9 项通过。
- rustfmt、限定改动的 `git diff --check` 及三个 Cargo manifest 语法解析通过。
- 基础冻结快照 Rust release 测试：19 项通过、4 项忽略。
- 最终认证快照编译失败：快照遗漏 admission 模块导出，示例条件分支数组长度不匹配。
  已刷新完整源码快照并显式使用 slice；失败日志保留，不计为认证代码通过。
- 新验证任务 `.artifacts/native-execution-20261008/` 已排队；参考链持有共享机器。
  包含认证、执行参数回归及 warmup/A1/B/A2 帧认证微基准，结果待补。

## 冻结验证任务

`.artifacts/native-primitives-20261007/` 保存 `source-manifest.json`、`claim.jsonl`、
`check.sh`、两个最小工作区、`vector-check.log` 和 `status.json`。
目录按任务开始日命名，本批文档跨日更新。参考共享源码不修改。

驱动先测试基础移植，再测试最终认证候选，最后构建并运行原生帧微基准。
依赖解算使用本机缓存并保存各冻结工作区锁文件；已有主工作区 Cargo.lock / API 升级
继续保留，不能把最小工作区的锁文件直接覆盖完整主工作区。

基准使用 500 笔真实签名的 0x50 交易、16 个 sender、独立测试链 id 941007。
同一 release executable 内运行 warmup/A1/B/A2，各 10 次；输入构造和网关签名不计时。
A 完整验证交易签名，B 使用显式配置的可信网关证明；两者检查同一 root、交易与 sender。
B 的信任配置与 A 不同，不能把结果解释为无条件保持同一认证成本的优化。
它只测 decode/root/authentication，排除执行、QMDB、共识和持久化，不能换算成链 TPS。

## 接下来

先审阅 Rust 真实结果并修复，再把 N42 原生交易环境转换接到直接转账执行接口，
随后接有界帧队列及 frame plan；以实时原生交易、共同提交、回执和 QMDB bin root
作为首次端到端验收。共享 owner 与构块产物复用在这些真实入口上实现。

## 执行参数接线

借用参考 tx-types 的转换接口，新增 `AdmittedTransaction::execution_env()`，直接使用认证后的 sender，
不重新解码或恢复签名。原生交易和回执类型仍为 0x50；内部执行参数 type 2 表示费用语义。
该参数可供现有直接转账执行器及解释器备用路径使用。新增字段一致性回归；
尚未切换节点执行工厂、帧队列和原生回执构建器，因此没有端到端性能结论。
