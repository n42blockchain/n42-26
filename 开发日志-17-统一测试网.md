# 开发日志-17: 统一测试网脚本 + E2E 测试补充

## 概述

将 3 个高度重复的测试网脚本（local-testnet.sh / testnet-7node.sh / testnet-21node.sh）
统一为一个参数化的 `scripts/testnet.sh`，并补充了 3 个 E2E 测试场景。

## 设计决策

### 统一脚本方案

**选择**: 以 `testnet-7node.sh` 为模板，将硬编码的节点数参数化为 `--nodes N`

**考虑过的替代方案**:
1. Python 重写 — 功能更强但引入额外依赖，且现有 bash 脚本已经稳定
2. 只保留一个脚本删除其他 — 会破坏已有文档和工作流引用
3. 配置文件方式 — 过度工程化，CLI 参数更直观

**最终决策**: bash 脚本参数化 + 旧脚本改为薄包装器（exec 转发），保持向后兼容。

### 超时缩放

不同节点数量需要不同的超时参数，使用查表法而非公式计算：

| 节点数 | BASE_TIMEOUT | MAX_TIMEOUT | STARTUP_DELAY | 节点间启动间隔 | 出块等待 |
|--------|-------------|-------------|---------------|---------------|---------|
| 1      | 10000ms     | 30000ms     | 2000ms        | -             | 60s     |
| 3      | 15000ms     | 45000ms     | 2000ms        | 3s            | 78s     |
| 5      | 15000ms     | 45000ms     | 2000ms        | 2s            | 90s     |
| 7      | 20000ms     | 60000ms     | 3000ms        | 2s            | 120s    |
| 21     | 30000ms     | 90000ms     | 5200ms        | 1s            | 180s    |

### 单节点特殊处理

- 跳过 P2P 密钥生成和 trusted-peers 配置
- 不传递 `--p2p-secret-key-hex` 参数

## 实施细节

### Part 1: 统一脚本 `scripts/testnet.sh`

**函数化结构**:
- `parse_args()` — 解析 CLI 参数，验证节点数 (1/3/5/7/21)
- `compute_timeouts()` — 根据 N 查表计算超时参数
- `setup_ulimit()` — 提升文件描述符限制
- `setup_python_venv()` — 创建/激活 Python 虚拟环境
- `build_binaries()` — 支持 `--debug` 标志切换 release/debug 构建
- `generate_genesis()` — Python 生成 genesis.json
- `generate_bls_keys()` — 确定性 BLS 密钥
- `generate_p2p_keys()` — 确定性 devp2p 密钥（单节点跳过）
- `start_validators()` — 循环启动各节点
- `wait_for_blocks()` — 轮询等待出块
- `start_blockscout()` / `start_tx_generator()` / `start_mobile_sim()` / `start_error_monitor()`
- `print_summary()` — 终端面板
- `keepalive_loop()` — 监控 PID + 状态报告

**新增参数**:
```
--nodes N           验证者数量 (1/3/5/7/21，默认 7)
--debug             使用 debug 二进制
--data-dir DIR      自定义数据目录
--block-interval MS 区块间隔
```

### Part 2: 旧脚本薄包装器

- `testnet-7node.sh` → deprecated 提示 + `exec testnet.sh --nodes 7 "$@"`
- `testnet-21node.sh` → deprecated 提示 + `exec testnet.sh --nodes 21 "$@"`
- `local-testnet.sh` → 顶部添加 deprecated 提示，保留原有功能

### Part 3: E2E 测试

#### Scenario 4 补充 7 节点

在现有 5-node 和 21-node 测试之间插入 7-node 子测试：
- `port_offset_base=150` 避免与现有测试端口冲突
- 使用相同的 5 项验证（高度一致性、最小出块数、哈希一致性、领导者轮换、区块间隔稳定性）

#### Scenario 12: Blockscout RPC 兼容性

从 `scripts/verify-blockscout-rpc.sh` 移植为 Rust e2e 测试：
- 启动单节点，等待出块
- 逐一调用 Blockscout 所需的 RPC 方法并验证
- Required (必须通过): `eth_blockNumber`, `eth_chainId`, `eth_getBlockByNumber`,
  `eth_getBlockByHash`, `eth_getTransactionReceipt`, `eth_getLogs`, `eth_getBalance`,
  `eth_getCode`, `eth_gasPrice`, `debug_traceTransaction`, `debug_traceBlockByNumber`,
  `net_version`, `web3_clientVersion`
- Optional (warn-only): `trace_block`, `trace_replayBlockTransactions`
- N42 custom (warn-only): `n42_consensusStatus`, `n42_validatorSet`

#### Scenario 13: 奖励分发验证

- 启动 3 节点，设 `N42_REWARD_EPOCH_BLOCKS=10`（短周期）
- 运行 ~30 个区块
- V1: 验证 validator coinbase 余额增长 > 0
- V2: 验证各 validator 出块比例大致均等

**NodeProcess 扩展**: 添加 `start_with_env()` 方法支持额外环境变量传递。

### 文件变更清单

| 文件 | 操作 | 说明 |
|------|------|------|
| `scripts/testnet.sh` | 新建 | 统一测试网启动脚本 |
| `scripts/testnet-7node.sh` | 重写 | 薄包装器 (exec 转发) |
| `scripts/testnet-21node.sh` | 重写 | 薄包装器 (exec 转发) |
| `scripts/local-testnet.sh` | 修改 | 添加 deprecated 提示 |
| `tests/e2e/src/scenarios/scenario4_multi_node.rs` | 修改 | 插入 7-node 子测试 |
| `tests/e2e/src/scenarios/scenario12_blockscout_rpc.rs` | 新建 | Blockscout RPC 兼容性测试 |
| `tests/e2e/src/scenarios/scenario13_rewards.rs` | 新建 | 奖励分发验证测试 |
| `tests/e2e/src/scenarios/mod.rs` | 修改 | 注册 scenario12/13 |
| `tests/e2e/src/main.rs` | 修改 | 添加 match arms 12/13，扩展 --all 范围 |
| `tests/e2e/src/node_manager.rs` | 修改 | 添加 start_with_env() 方法 |

## 验证

- `cargo check -p e2e-test` — 编译通过，0 新警告
- `bash -n scripts/testnet.sh` — 语法检查通过
- 旧脚本 `testnet-7node.sh` / `testnet-21node.sh` 正确转发到统一脚本

## 后续计划

- 实际运行 `scripts/testnet.sh --nodes 1/3/5/7/21` 端到端验证
- 运行 E2E scenario 12/13 验证功能正确性
- 考虑为 local-testnet.sh 也改为薄包装器（当前保留原代码以兼容 debug 构建工作流）
