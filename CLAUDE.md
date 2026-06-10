# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Git 提交规则

所有 git 提交不要包含 "Claude" 或 "Co-Authored-By: Claude" 等字样。

提交命令模板：
```
GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
  git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" -m "message"
```

## 项目上下文

- 自定义区块链：执行端基于 reth（本地 path 依赖 `../reth`），共识端采用 HotStuff-2 变体
- 分发节点（IDC）负责出块、共识投票、存储状态；手机并行验证，不在共识关键路径上
- 规模：100-500 IDC 节点，每节点约 10,000 手机；性能目标：8 秒 slot
- Rust edition 2024，最低 Rust 1.93+

### ⚠️ reth fork 基线（务必对齐，否则会触发依赖降级）

`../reth` 有两个基线，**workspace 依赖 pin 与 reth 分支必须匹配**，用错会导致编译失败或被迫降级依赖：

| reth 分支 | base | 对应依赖 pin |
|-----------|------|-------------|
| `n42-v2-upgrade`（旧） | reth v2.2.0 | revm 38 / alloy-evm 0.34 / reth-primitives-traits 0.3.1 |
| **reth main 合并版（当前主线）** | upstream main | **revm 40.0.3 / alloy-evm 0.36.0 / reth-primitives-traits 0.4.0** |

**当前 n42-26 主线（含 `chore/merge-reth-main-deps-upgrade`）用 reth main 合并版。** 动手前先
`git -C ../reth log -1 --oneline` 确认 reth 在 main 合并版；**切勿为了让旧 reth 编过而降级
`Cargo.toml` 的 revm/alloy/reth-* 版本**——那会推翻 deps upgrade 工作（参见 devlog-60 维护者说明）。

## 常用命令

```bash
# 检查编译（所有 target）
cargo check --all-targets

# Lint（警告即报错）
cargo clippy --all-targets -- -D warnings

# 单元测试
cargo test --workspace

# 单个 crate 测试
cargo test -p n42-consensus

# 单个测试函数
cargo test -p n42-consensus test_name

# 集成测试（n42-consensus 有 7 个模块）
cargo test -p n42-consensus --test integration_test                        # 全部
cargo test -p n42-consensus --test integration_test fault_tolerance        # 单模块
cargo test -p n42-consensus --test integration_test -- --nocapture         # 带输出

# 性能基准（标记为 #[ignore]，需显式指定）
cargo test -p n42-consensus --test performance_bench --release -- --ignored --nocapture

# Release 构建（节点 + E2E 测试工具）
cargo build --release -p n42-node-bin -p e2e-test

# 其他二进制
cargo build --release -p n42-stress       # 压力测试工具（TCP 注入，122K tx/s）
cargo build --release -p n42-mobile-sim   # 手机模拟器
cargo build --release -p n42-evm-bench    # EVM 基准

# 本地测试网（推荐，7 节点默认）
./scripts/testnet.sh
./scripts/testnet.sh --nodes 3 --debug        # 3 节点 debug 构建
./scripts/testnet.sh --nodes 21 --clean       # 21 节点，清除旧数据
./scripts/testnet.sh --nodes 1 --no-explorer  # 单节点，无 Blockscout

# E2E 测试（需先 release 构建）
E2E_SCENARIO_FILTER=1,3,4 target/release/e2e-test --binary target/release/n42-node
E2E_SCENARIO_FILTER=5,8,12 target/release/e2e-test --binary target/release/n42-node

# 手机模拟器
./scripts/mobile-sim.sh

# 压力测试
./scripts/step_stress.sh
```

## 代码架构

### Crate 职责

| Crate | 职责 |
|-------|------|
| `n42-primitives` | BLS 密钥、共识消息、共享类型 |
| `n42-chainspec` | 链配置、ValidatorInfo、ConsensusConfig（含 deterministic_key_bytes） |
| `n42-consensus` | HotStuff-2 状态机、验证者生命周期、reth 适配器 |
| `n42-execution` | EVM 执行辅助、witness 生成、state diff |
| `n42-mobile` | 手机协议、数据包、收据、本地验证（零 reth 依赖，仅 alloy + ed25519） |
| `n42-mobile-ffi` | Android/iOS C/JNI 绑定（staticlib + cdylib） |
| `n42-network` | libp2p 服务、QUIC StarHub、共识/区块直连通道 |
| `n42-node` | 编排器（ConsensusOrchestrator 3-way select! loop）、RPC、持久化、手机桥接、奖励分发 |
| `n42-parallel-evm` | 乐观并行 EVM 执行 |
| `n42-jmt` | Blake3 Jellyfish Merkle Tree，16 分片并行，快照 |
| `n42-zkproof` | ZK sidecar 证明系统（SP1 zkVM 后端，trait ZkProver + MockProver） |

### 依赖关系要点

- `n42-primitives` 是最底层 crate，被几乎所有 crate 依赖
- `n42-mobile` 零 reth 依赖 — 只依赖 `alloy-primitives`、`ed25519-dalek`、`lru`、`serde`
- `n42-zkproof-guest` 用 SP1 RISC-V 工具链构建，已从 workspace 排除
- 所有 reth 依赖通过 `../reth` 本地 path 引入

### 关键设计

- **共识**：HotStuff-2，乐观投票（follower 在 import 前投票），2 轮提交，3 轮超时恢复
- **共识引擎**：事件驱动状态机（`ConsensusEngine::process_event`），无内部事件循环，完全确定性
- **签名**：BLS12-381 聚合签名，紧凑 QC
- **网络**：libp2p GossipSub + QUIC；StarHub 管理手机 QUIC 连接；TX 转发给 leader（O(n) 而非 O(n²) gossip）
- **区块传播**：compact block + 执行输出缓存（缓存命中约 3ms）
- **状态树**：JMT + Blake3，16 分片并行更新，支持手机 Merkle proof
- **手机奖励**：EIP-4895 withdrawals 机制，按 epoch 分发（默认 21,600 块）
- **ZK**：`n42-zkproof-guest` 用 SP1 RISC-V 工具链构建，已从 workspace 排除

### E2E 场景编号

| 编号 | 场景 | CI |
|------|------|----|
| 1 | 单节点启动 | smoke-consensus |
| 3 | ERC-20 合约 | smoke-consensus |
| 4 | 多节点共识（correctness profile） | smoke-consensus |
| 5 | 手机验证 | mobile-rpc |
| 8 | 手机 EVM | mobile-rpc |
| 12 | Blockscout RPC | mobile-rpc |

手动 E2E 场景（不在 CI）：2（RPC load）、6（stress）、7（21×21）、9（recovery）、10（chaos）、13（reward）。详见 `tests/e2e/README.md`。

### CI 管道

| Workflow | 触发 | 内容 |
|----------|------|------|
| `e2e.yml` | push main/develop, PR to main | clippy + unit tests + E2E 1,3,4 + 5,8,12 |
| `nightly.yml` | 每日 02:00 UTC / 手动 | E2E 5,8,12 + cargo-audit |
| `execution-spec-shards.yml` | 手动 dispatch | Docker n42-node 镜像 + Hive 集成测试 |

## Code Quality

生成代码后立即运行 `cargo check` / `cargo clippy`，不等用户询问。

## 开发日志

每次完成阶段性工作后，增量追加到 `docs/` 对应文件（索引见 `DEVLOG.md`）：
1. 设计决策（选择方案的原因，考虑过的替代方案）
2. 实施细节（关键代码结构、API、模块依赖）
3. 遇到的问题及解决方案
4. 阶段完成状态
5. 后续计划

架构文档在 `docs/00-Workspace-Map.md`、`01-System-Architecture.md`、`02-Core-Flows.md`、`03-Crate-Reference.md`。

## Communication

用户使用中文（普通话）交流，始终用中文回复，除非用户切换到英文。
