# Gov5 payload 转换跟进（2026-10-07）

上一轮七节点 A/B/A 的领导者规范化验证中位数为 963 / 971 / 964 ms。
本轮针对该路径中的冗余工作，不改变 QMDB 二叉状态承诺，也不改交易签名或执行拓扑。

## 小提交

已按顺序推送：`0453c75`（交易体复用）、`d783d5c`（原始交易预计算与回归用例）、
`bdb6889`（阶段计时）。

1. difficulty=0 / 历史 difficulty=1 头部选择复用同一份交易体，去掉整块 clone。
2. live Gov5 标准头哈希预计算使用原始交易字节，不在预计算阶段再次解码 EIP-2718 envelope。
   交易根仍从原始交易计算，不根据声明哈希跳过承诺计算；上游仍进行完整交易解码、
   标准头和分叉字段校验。replay-v2 仍严格解码后匹配原历史头形态。

3. 增加 `n42_engine_payload_conversion_duration_ms{phase=standard_header|ethereum_validate|native_header_bind|replay_v2}`。
   在原生 extraData 初检之后开始计时；包含对应阶段的失败返回，Ethereum profile 不增加这些计时。

回归覆盖非空交易体的两个 difficulty 版本、畸形/未知类型/尾随字节、
原哈希下交易顺序变更、上游 pre-Shanghai withdrawals 拒绝与 replay-v2 非空交易体。

## 独立转换基准

`.artifacts/gov5-validator-20261007/` 保存修改前、去 clone、最终候选源码及 SHA256。
冻结工作区继承上一轮 Reth 2.7 / QMDB 等未提交升级；不是纯 Git HEAD。
适配后的 Reth 为 `3452d7d0217095446b0f2632a283e314951da273`。

基准在同一 release test 可执行文件内编译修改前和最终候选，使用同一份 220,000 笔
legacy EIP-2718 payload，先比较完整区块，再进行预热 / A1 / B / A2 各 10 次。
计时包含调用者 payload clone 和完整转换，不含输入构造、返回块销毁、签名恢复/密码学校验、EVM 执行、
QC 提交、QMDB 持久化或网络传播。test 分配器和数据形态不同于生产节点，
只用于定位转换成本，不能换算成 canonical TPS，也不代表 E1 / Ed25519 原生帧性能。

## 验证状态

用户选择先推送已完成静态检查的改动，验证结果稍后补。
`rustfmt --edition 2024 --check` 与本文件改动的 `git diff --check` 已通过。
本轮尚未取得编译、回归测试或转换基准结果，因此不声明性能提升或运行正确性已验证。

冻结快照验证已排队，驱动 PID 为 `3138755`，当前未持有共享机器声明；
参考仓库的七节点任务仍占用机器。驱动在共享机器协议满足后依次运行两个完整库测试、
包含四个源码版本的隔离转换回归测试，以及同一可执行文件中的 A/B/A 基准。
完整库测试失败状态将单独保存，隔离测试通过不能替代完整库测试通过。

本地复核材料位于 `.artifacts/gov5-validator-20261007/`：
`source-manifest.json`、`check-clone.sh`、`claim-clone.jsonl`、
`validator-before.rs`、`validator-clone.rs`、`validator-next.rs`、`validator-timed.rs`。
这些材料尚未作为测量结果发布，后续需审阅真实日志再补充结论。

## 原生路径下一批依据

只读复核 `../n42-rs` 当前 HEAD `af18b95d3148b103d92775aa6997104a11605b6d`，
所引用的 frame index、output shards 和 import-once 文件没有工作区差异。
`tx-queue/src/frames.rs` 的 `MAX_FRAMES=16_384` 是帧索引保留上限，不能替代
入队计数/字节信用、sender nonce 连续性和 canonical prune。
`output_shards.rs` 让 QMDB root 与后继构块直接读 FrozenShards 的视图，
发布执行所需的连续 BundleState 在 seal 后合并；`freeze_on_thread` 在消费输出前仍须 join，
线程创建失败会退回原输出，并非省略冻结或验证。
这些是后续迁移契约，不把本轮 legacy 转换基准当作原生帧路径的容量验证。
