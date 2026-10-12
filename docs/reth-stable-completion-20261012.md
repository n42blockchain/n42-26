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
