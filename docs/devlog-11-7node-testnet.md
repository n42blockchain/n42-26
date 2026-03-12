# 开发日志 11 — 7 节点 HotStuff-2 测试网

> 日期：2026-02-21
> 目标：MacBook Pro 上运行 7 节点共识测试网，持续多日，含交易负载和区块浏览器

---

## 设计决策

### 1. 为什么使用自定义 genesis.json 而非 `--chain dev`

`--chain dev` 是 reth 的开发模式，预置了特定的开发账户和链配置。我们需要：
- Chain ID 4242（匹配 Blockscout 和钱包配置）
- 10 个确定性测试账户各预充值 100M N42
- 与 `tests/e2e/src/genesis.rs` 完全一致的账户派生

因此使用 `--chain /path/to/genesis.json`，共识配置仍通过环境变量注入（`N42_VALIDATOR_KEY`、`N42_VALIDATOR_COUNT`、`N42_ENABLE_MDNS`）。

### 2. Keccak-256 vs SHA3-256 的关键区分

这是整个方案最关键的正确性问题：

| 算法 | 标准 | Python API | 填充 |
|------|------|-----------|------|
| Keccak-256 | 以太坊原始 | `eth_utils.keccak()` | 0x01 |
| SHA3-256 | NIST FIPS 202 | `hashlib.sha3_256()` | 0x06 |

**相同输入产生不同输出**。Rust 端 `alloy_primitives::keccak256` 使用 Keccak-256，Python 端必须使用 `eth_utils.keccak`，否则派生的密钥不匹配 genesis 预充值地址，交易会因余额不足失败。

验证结果：
```
seed = b"n42-test-key-0"
Keccak-256: 23e2710fa7d877009b99d3977c5f64c2...
SHA3-256:   a457fbd4608c17d6f4946f3ff37d25ba...
```

### 3. 端口分配方案

7 个节点共需 7×6=42 个端口，从高端口号开始避免冲突：

| 服务 | 基础端口 | 范围 |
|------|---------|------|
| HTTP RPC | 18545 | 18545-18551 |
| WebSocket | 18645 | 18645-18651 |
| Auth RPC | 18551 | 18551-18557 |
| P2P | 30303 | 30303-30309 |
| Consensus | 9400 | 9400-9406 |
| Metrics | 19001 | 19001-19007 |

### 4. 三层 nonce 管理

多日运行中，nonce 漂移是最常见的交易发送失败原因：

1. **Layer 1 — 本地计数器**：每次发送后 +1，零延迟
2. **Layer 2 — 错误回滚+即时同步**：发送失败时回滚 nonce，从链上重新获取
3. **Layer 3 — 定时全量同步**：每 5 分钟从链上同步所有账户 nonce

配合指数退避（连续 20 次错误后暂停 5-30s），确保长时间运行的稳定性。

### 5. 数据持久化

使用 `$HOME/n42-testnet-data/` 而非 `/tmp/`：
- 重启不丢数据
- 支持 `--clean` 参数手动清理
- 日志、genesis、PID 文件集中管理

---

## 交付文件

| 文件 | 说明 |
|------|------|
| `scripts/testnet-7node.sh` | 主启动脚本：编译 → genesis → 7 节点 → Blockscout → 交易生成 → 监控 |
| `scripts/tx-load-generator.py` | Python 持续交易生成器：ERC-20 部署 + 70/30 混合交易 |
| `scripts/error-monitor.sh` | 错误监控：日志过滤、健康检查、崩溃检测、分叉告警 |
| `scripts/requirements-testnet.txt` | Python 依赖：eth-account + requests |

---

## 关键实现细节

### 主脚本流程 (testnet-7node.sh)

1. 解析参数（`--clean`、`--no-explorer`、`--no-tx-gen`、`--no-monitor`）
2. `ulimit -n 65536` 防止 FD 耗尽
3. `cargo build --release` 编译优化二进制
4. 内嵌 Python 脚本生成 `genesis.json`（使用 eth_utils.keccak 派生密钥）
5. 启动 7 个验证者（BLS 密钥：`printf '%056x%08x' 0 $((i+1))`）
6. 轮询 `eth_blockNumber` 等待出块
7. 创建 docker-compose override 文件指向节点 0 的 RPC 端口
8. 启动 Blockscout、交易生成器、错误监控（各为后台进程）
9. Ctrl+C 触发 trap 依次关闭所有服务

### 交易生成器 (tx-load-generator.py)

- **Phase 1**：部署 TestUSDT 合约（字节码从 `tests/e2e/contracts/TestUSDT.hex` 加载）
- **Phase 2**：2 tx/sec 持续生成（70% 原生转账 + 30% ERC-20 transfer）
- ERC-20 calldata 手动 ABI 编码（selector `0xa9059cbb` + 地址 + 金额）
- 后台线程每 30s 输出统计报告

### 错误监控 (error-monitor.sh)

- `tail -F` 监控所有 `validator-*.log` 文件
- 过滤模式：`ERROR|panic|SIGSEGV|out of memory|consensus.*timeout`
- 每 30s 查询所有节点 `eth_blockNumber`，检测 >10 块分歧
- PID 消失检测 → 抓取最后 50 行日志 + 全节点块高快照

### Blockscout 集成

通过 docker-compose override 文件将后端指向 node 0：
```yaml
services:
  backend:
    environment:
      ETHEREUM_JSONRPC_HTTP_URL: http://host.docker.internal:18545/
      ETHEREUM_JSONRPC_WS_URL: ws://host.docker.internal:18645
```

---

## 复用的现有代码

| 来源 | 复用内容 |
|------|----------|
| `scripts/local-testnet.sh` | BLS 密钥生成格式、节点启动参数、环境变量模式 |
| `tests/e2e/src/genesis.rs` | 测试账户密钥派生算法、genesis JSON 格式、Chain ID 4242 |
| `tests/e2e/contracts/TestUSDT.hex` | ERC-20 合约部署字节码 |
| `docker/docker-compose.blockscout.yml` | Blockscout 全栈 Docker 定义 |
| `docker/blockscout.env` | Blockscout 环境变量模板 |

---

## 资源预估

| 组件 | 内存 |
|------|------|
| 7× n42-node (release) | ~3.5 GB |
| Blockscout 全栈 | ~2.5 GB |
| Python tx-generator | ~50 MB |
| 系统开销 | ~1 GB |
| **总计** | **~7 GB** |

磁盘增长：~50MB/day，一周 ~350MB。

---

## 启动命令

```bash
# 安装依赖
pip3 install -r scripts/requirements-testnet.txt

# 全栈启动
./scripts/testnet-7node.sh

# 清除数据重新开始
./scripts/testnet-7node.sh --clean

# 只启动节点
./scripts/testnet-7node.sh --no-explorer --no-tx-gen --no-monitor
```

---

## 阶段完成状态

- [x] `scripts/requirements-testnet.txt` — Python 依赖文件
- [x] `scripts/tx-load-generator.py` — 交易生成器（三层 nonce 管理、ERC-20 部署、混合交易）
- [x] `scripts/error-monitor.sh` — 错误监控（日志过滤、健康检查、崩溃检测）
- [x] `scripts/testnet-7node.sh` — 主启动脚本（一键全栈）
- [x] Keccak-256 密钥派生验证通过
- [x] 所有脚本语法检查通过

## 后续验证计划

1. 启动后检查 7 个节点全部出块
2. Blockscout http://localhost:3000 显示区块和交易
3. 杀掉一个节点验证容错（f=2 允许 2 个故障）
4. 运行 24h 后检查内存无泄漏、nonce 无漂移
