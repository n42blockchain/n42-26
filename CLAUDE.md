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

- 自定义区块链：执行端基于 reth v1.11.0（本地 path 依赖 `../reth-latest`），共识端采用 HotStuff-2 变体
- reth 需要先打补丁：`git apply ../n42-26/reth-n42.patch`（在 `../reth-latest` 目录执行）
- 分发节点（IDC）负责出块、共识投票、存储状态；手机并行验证，不在共识关键路径上
- 规模：100-500 IDC 节点，每节点约 10,000 手机；性能目标：8 秒 slot

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

# Release 构建（节点 + E2E 测试工具）
cargo build --release -p n42-node-bin -p e2e-test

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
| `n42-mobile` | 手机协议、数据包、收据、本地验证 |
| `n42-mobile-ffi` | Android/iOS C/JNI 绑定 |
| `n42-network` | libp2p 服务、QUIC StarHub、共识/区块直连通道 |
| `n42-node` | 编排器、RPC、持久化、手机桥接、奖励分发 |
| `n42-parallel-evm` | 乐观并行 EVM 执行 |
| `n42-jmt` | Blake3 Jellyfish Merkle Tree，16 分片并行，快照 |
| `n42-zkproof` | ZK sidecar 证明系统（SP1 zkVM 后端） |

### 关键设计

- **共识**：HotStuff-2，乐观投票（follower 在 import 前投票），2 轮提交，3 轮超时恢复
- **签名**：BLS12-381 聚合签名，紧凑 QC
- **网络**：libp2p GossipSub + QUIC；StarHub 管理手机 QUIC 连接；TX 转发给 leader（O(n) 而非 O(n²) gossip）
- **区块传播**：compact block + 执行输出缓存（缓存命中约 3ms）
- **状态树**：JMT + Blake3，16 分片并行更新，支持手机 Merkle proof
- **手机奖励**：EIP-4895 withdrawals 机制
- **ZK**：`n42-zkproof-guest` 用 SP1 RISC-V 工具链构建，已从 workspace 排除

### E2E 场景编号

| 编号 | 场景 |
|------|------|
| 1 | 单节点启动 |
| 3 | ERC-20 合约 |
| 4 | 多节点共识（correctness profile） |
| 5 | 手机验证 |
| 8 | 手机 EVM |
| 12 | Blockscout RPC |

CI PR 只跑 `1,3,4`（smoke-consensus）和 `5,8,12`（mobile-rpc）。

## Code Quality

生成代码后立即运行 `cargo check` / `cargo clippy`，不等用户询问。

## 开发日志

每次完成阶段性工作后，增量追加到 `docs/` 对应文件（索引见 `DEVLOG.md`）：
1. 设计决策（选择方案的原因，考虑过的替代方案）
2. 实施细节（关键代码结构、API、模块依赖）
3. 遇到的问题及解决方案
4. 阶段完成状态
5. 后续计划

## Communication

用户使用中文（普通话）交流，始终用中文回复，除非用户切换到英文。
