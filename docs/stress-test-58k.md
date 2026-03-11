# 58K Cap 压测指南 (Ubuntu 测试机)

## 前置条件

1. **Rust 工具链**: `rustup` + stable toolchain
2. **克隆仓库**:
   ```bash
   git clone git@github.com:n42blockchain/n42-26.git
   git clone git@github.com:AstroxNetwork/reth.git   # N42 fork 的 reth
   ```
3. **应用 reth patch**:
   ```bash
   cd reth
   git checkout e63ebac380
   git apply ../n42-26/reth-n42.patch
   cd ..
   ```

## Step 1: 编译

```bash
cd n42-26
cargo build --release
# 二进制产物: target/release/n42-node, target/release/n42-stress
```

## Step 2: 生成 Presign 文件

预签名 500 万笔交易（只需执行一次，文件可复用于 `--clean` 启动的链）:

```bash
./target/release/n42-stress \
  --accounts 5000 \
  --presign 5000000 \
  --presign-save /tmp/n42-presigned-5M.bin
```

耗时约 1-2 分钟，生成 ~1.5GB 文件。

## Step 3: 启动 7 节点测试网

```bash
N42_SKIP_TX_VERIFY=1 \
N42_MAX_TXS_PER_BLOCK=58000 \
N42_FAST_PROPOSE=1 \
N42_DEFER_STATE_ROOT=1 \
N42_INJECT_PORT=19900 \
./scripts/testnet.sh --nodes 7 --clean --no-explorer --no-tx-gen --no-monitor --no-mobile-sim
```

### 环境变量说明

| 变量 | 值 | 作用 |
|------|-----|------|
| `N42_SKIP_TX_VERIFY` | `1` | 跳过 tx pool 中的签名验证 |
| `N42_MAX_TXS_PER_BLOCK` | `58000` | 每块最大交易数 (58K cap) |
| `N42_FAST_PROPOSE` | `1` | 收齐票立即出块，不等 slot timer |
| `N42_DEFER_STATE_ROOT` | `1` | 延迟状态根计算，header 中使用 B256::ZERO |
| `N42_INJECT_PORT` | `19900` | 启用 TCP 注入端口 (节点 0=19900, 1=19901, ..., 6=19906) |

等待所有 7 个节点启动并开始出块（日志中出现 `consensus started`），大约 10-20 秒。

## Step 4: 运行压测

**打开新终端**，执行:

```bash
./target/release/n42-stress \
  --inject 127.0.0.1:19900,127.0.0.1:19901,127.0.0.1:19902,127.0.0.1:19903,127.0.0.1:19904,127.0.0.1:19905,127.0.0.1:19906 \
  --target-tps 0 \
  --presign-load /tmp/n42-presigned-5M.bin \
  --batch-size 500 \
  --duration 90
```

### 参数说明

| 参数 | 值 | 作用 |
|------|-----|------|
| `--inject` | 7 个 TCP 端口 | 二进制 TCP 注入，绕过 JSON-RPC |
| `--target-tps` | `0` | 不限速，全速灌入 |
| `--presign-load` | 预签名文件 | 跳过签名，纯 I/O 注入 |
| `--batch-size` | `500` | 每批 500 笔 tx |
| `--duration` | `90` | 持续 90 秒 |

## 预期结果

基于 macOS (M-series) 的基准数据:

| 指标 | 预期值 |
|------|--------|
| **Effective TPS** | ~50,000-53,000 |
| **Block Time** | ~1.0s |
| **Build p50** | ~57ms |
| **R1 Collect p50** | ~33ms |
| **R2 Collect p50** | ~38ms |
| **Follower Import** | ~3ms (compact block cache hit) |

> Ubuntu/Linux 上磁盘 I/O 通常比 macOS 更快，TPS 可能更高。

## 观察指标

压测运行中，观察节点 0 (leader) 日志:

```bash
# 关键日志关键词
grep "N42_PAYLOAD_PACK\|view_time\|block_time\|R1_collect\|R2_collect\|build_ms\|finish_ms" /tmp/n42-testnet/node-0/logs/*.log
```

重要指标:
- `tx_count`: 每块交易数，应接近 58,000
- `view_time_ms`: 出块周期，Fast Propose 下应 ~1000ms
- `build_ms`: payload 构建时间
- `pool_pending`: tx pool 待处理数，Pool Gate 会控制在 90K 以下

## 停止测试

1. `Ctrl+C` 停止 stress 工具
2. `Ctrl+C` 停止 testnet（或 `pkill -f "target/release/n42-node"`）

> **注意**: 不要用 `pkill -f "n42-node"`，会误杀 cargo build 进程

## 故障排除

| 问题 | 原因 | 解决 |
|------|------|------|
| TPS 很低 (<10K) | presign 文件 nonce 不匹配 | 确保用 `--clean` 启动链（nonce 从 0 开始） |
| 链卡住不出块 | pool 溢出 / nonce gap | 检查 `N42_INJECT_PORT` 是否设置（启用 Pool Gate） |
| patch 应用失败 | reth 版本不对 | 确认 `git checkout e63ebac380` |
| 编译失败 | 缺少依赖 | Ubuntu: `apt install build-essential pkg-config libssl-dev clang` |

## 对比其他 Cap 值

如需测试其他 cap，只需修改 `N42_MAX_TXS_PER_BLOCK`:

```bash
# 48K cap (保守)
N42_MAX_TXS_PER_BLOCK=48000

# 54K cap
N42_MAX_TXS_PER_BLOCK=54000

# 64K cap (block_time 会膨胀到 1.34s，TPS 反降)
N42_MAX_TXS_PER_BLOCK=64000
```

已验证的 cap sweep 数据 (macOS):

| Cap | TPS | Block Time |
|-----|-----|------------|
| 48K | 45,069 | 1.0s |
| 54K | 51,578 | 1.0s |
| **58K** | **52,898** | **1.0s** |
| 64K | 43,182 | 1.34s |
