# Reth v2.7.0：两 Rust 工程稳定版适配补全

日期：2026-10-12。状态：适配进行中，完整编译及运行验收待完成。

用户明确选择官方最新稳定版 v2.7.0，而非上游未发布 main。
基线为 `3d592ece6de8c4559987416a544fc215fd6d6921`。
正在运行的 Gov5 replay v2 七节点长测使用冻结二进制；升级不修改其运行目录。

## 分支与实现边界

- n42-26 `main` 已声明 v2.7.0，并有六个已锁定的集成补丁。
  实际适配 Reth 源是 `.artifacts/reth-upgrade/reth-worktree` @ `3452d7d...`。
  原 `../reth` 是旧的、包含未提交改动的 checkout，保持只读；全面验证在独立 workspace 使用正确源。
- n42-rs `main` 已声明 v2.7.0。已核对 102 个实际 git 依赖均为该 tag，
  Cargo.lock 中的 Reth git source 均指向上述官方 commit。
- 七节点自定义链实测所用 n42-rs `perf/erigon-borrow-20260828` @ `e7d5c0cf7`
  仍为 v2.5.1，具有主线缺少的 chain94 扩展 header、dirty-slot、Prague/EOF 兼容规则。
  从该已验证基线创建 `upgrade/reth-v2.7.0-chain94-20261012`，
  工作树 `../n42-rs-wt-reth27-chain94`；原兼容工作树及已冻结运行二进制保持不变。
  新分支不是性能 Ed25519 路线的替代品。

## 已完成的补全

1. n42-26 四个 workflow、五个 Reth checkout 从旧 fork ref 改为官方 v2.7.0 commit，
   保留补丁应用步骤；新增 CI pin 与 source lock 一致性回归。
2. n42-26 / n42-rs 开发入口纠正旧版本说明；不降低 Alloy/revm 以迁就旧 checkout。
3. n42-rs 升级工具用 TOML 解析实际生效的 patch，避免把注释掉的 fork 重新同步；
   `N42_RETH_SOURCE` 支持隔离 upstream checkout。工具回归 2 项通过。
4. 兼容分支按 v2.5.1 → v2.7.0 三方同步 vendored Reth；10 个冲突文件逐项处理，
   保留 N42 storage trait、传播和链规格定制。
   Alloy engine/beacon 源按其真实 2.3.0 基底合并到 2.5.0，保留 MobileRegistryRoot 扩展；无合并冲突。
5. 新的 `reth-storage-overlay` 使用官方 v2.7.0 实现，迁移状态 provider 接口。
   本次旧兼容分支未声明具有性能主线的 QMDB-only 读取接口，不把另一条路线的钩子未经接线直接复制过来。
6. EVM 父状态读取与 mobile SDK 使用 `into_evm_state_provider`，Engine 的 block 校验接收 access-list wrapper；
   新 SSZ 测试初始化 N42 附加字段。storage API 保持 no-std 导入兼容。
7. 本工程 frame 字节计数回归的 `.sum()` 类型推断失败已显式指定 `usize`。
8. 兼容分支复用主线 `f24c85ac5` 的恢复修复：R1/R2 水位与 locked QC 在同一日志记录中同步落盘，
   恢复时取更高水位和 lock，损坏记录拒绝启动，保留旧 20-byte 日志读取。
   不移植 benchmark 跳过同步选项。另复用 `e21c76dbb` 的日志写入失败回滚与重试测试。

所有变更按依赖、存储接口、provider、节点、网络、执行适配及恢复修复逐段提交、逐次推送。
升级分支中的源码同步提交尚不能替代完整构建和运行结果；锁文件将在依赖解析成功后另行提交。

## 验证记录与待办

- 本工程脚本完整回归：95 项通过（包括下列定向回归）。
- `test_apply_reth_patches.py`：7 项通过。
- `test_reth_ci_baseline.py`：1 项通过。
- `test_reth_sync.py`：2 项通过。
- n42-26 实际隔离 Reth 源的六补丁 / source lock 检查通过。
- 合并源无冲突标记，`git diff --check` 通过。
- 静态 TOML 依赖检查已核对所涉及 workspace / patch crate 的依赖名称。
- 全面验证驱动已排队，等 Gov5 当前长测优雅退出并释放机器后，
  按机器独占规则执行兼容分支锁文件解析、release workspace all-targets 检查、
  本工程 all-targets 检查和完整 workspace 测试。结果写入
  `.artifacts/reth-stable-completion-20261012/`。

随后处理实际编译／测试错误，构建新二进制，在 replay 派生真实 QC 与 QMDB 根的基础上做
独立七节点持续运行、掉线／追块、整队重启和混合委员会验收。
现有旧二进制的短测通过记录不能记到新升级分支上。

## 全量验证闭环（续）

运行阶段发现并逐次修复：

- 仓库原 Cargo.lock 缺少已入 workspace 的 `n42-tx-types`；在正确隔离源上更新锁文件，
  保持 Reth v2.7.0 / Alloy / revm 版本。
- 较新 proptest/rand 改变 RNG trait 解析，`proptest-arbitrary-interop` 0.1.0 编译失败；
  本地窄补丁仅补 `Rng` trait 导入，保留原源码、测试和 MIT OR Apache-2.0 许可；不改共享 registry。
- 两份 n42-rs 的 all-targets 检查暴露 Sepolia 模块、非泛型 ChainSpec 测试初始化、
  已移除的 Snapshot RLP bench API、Criterion 变量遮蔽，以及缺失的四个上游 fork builder 方法。
  逐项修正；fork helper 复用官方实现，并验证明确的前置 fork。
- Alloy crates.io 包缺少 SSZ 和 payload 测试工件；从发布元信息对应的
  `alloy-rs/alloy` commit `59174912873b337ea5868c295cefa70997596f29` 补回
  11 组 SSZ vectors 和 5 份 payload JSON，并记录来源与摘要。
- 上游新回归默认 raw payload 转换限制 extraData 为 32 bytes；N42 既有 APoS/H2
  转换有意保留更长的 seal/committee 数据。测试改为验证三条 raw 转换路径完整保留数据；
  没有改变转换实现或既有共识层的 header 布局／长度验证。
- 本工程 parallel EVM 的全局 metrics 断言被其他 scheduler 测试的手动 abort 干扰，
  并且执行测试修改全局环境变量。17 个相关单测用同一测试 guard 隔离，保持生产逻辑和精确断言。

截至本次记录：

| 对象 | 结果 | 工件 |
| --- | --- | --- |
| n42-26 release workspace all-targets | PASS | `attempt-4/root-check.log` |
| n42-26 完整 release workspace 测试 | 1,803 通过 / 0 失败 / 30 声明忽略 | `attempt-6/root-tests.log` |
| n42-26 parallel EVM 定向回归 | 17 通过 / 0 失败 | `attempt-6/root-parallel-tests.log` |
| n42-rs 兼容升级分支 release all-targets | PASS | `attempt-5/rs-check.log` |
| n42-rs 主线 release all-targets | PASS | `attempt-5/rs-main-check.log` |
| 兼容升级 H2/网络/执行/节点恢复/QMDB | 351 通过 / 0 失败 | `attempt-2/rs-tests.log` |
| 兼容升级 chainspec/engine/payload 回归 | 217 通过 / 0 失败 | `attempt-7/rs-adapter-tests.log` |
| 主线 chainspec/engine/payload 回归 | 467 通过 / 0 失败 / 11 声明忽略 | `attempt-7/rs-main-adapter-tests.log` |
| 兼容升级节点、init-snapshot、h2_validator | release 构建通过 | `attempt-5/rs-build.log`、`rs-validator-build.log` |

各集合可能覆盖不同测试 target，不能把重复覆盖加成唯一测试总数。之前失败轮次全部保留；
最新成功门槛按上表所列轮次核对，不把包含已修复 FAIL 的整个旧驱动结果改写成 PASS。

新的 v2.7 二进制已安排重新构建、冻结与七节点升级续跑，使用停止的 v2.5 舰队独立副本，
保留旧数据与旧二进制。运行、重启、长期负载和混合互通尚待完成，编译／回归不替代这些门槛。
