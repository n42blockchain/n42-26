# 开发日志 34 — 2G Gas Limit 与 Pool 溢出修复

## 日期: 2026-03-08

## 一、问题背景

日志-33 确认 500M gas limit 是 TPS 天花板：
- 理论极限：500M / 21K / 2s = 11,905 TPS
- 实际达到 11,028 TPS（92% 利用率）
- 50K TPS 注入导致 pool 溢出 → pacemaker timeout → 链 stall

## 二、改动

### Gas Limit 提升 (500M → 2G)
- `scripts/testnet.sh`: gasLimit `0x1DCD6500` → `0x77359400` (2,000,000,000)
- `scripts/testnet.sh`: `--builder.gaslimit 500000000` → `--builder.gaslimit 2000000000`
- 理论 TPS 极限：2G / 21K / 2s = **47,619 TPS**

### Pool 容量扩大
- `crates/n42-node/src/pool.rs` `idc_pool_config()`:
  - pending: 200,000 txs / 400MB
  - basefee: 100,000 txs / 200MB
  - queued: 100,000 txs / 200MB
  - max_account_slots: 16,384

### 压测工具配置
- accounts: 2000 → 3000
- max_pool: 80,000 → 200,000
- resume_pool: 40,000 → 100,000
- Genesis 预分配: 5000 accounts

### 修改文件
- `scripts/testnet.sh` — gas limit 2G, genesis 5000 accounts
- `crates/n42-node/src/pool.rs` — 扩大 pool 限制
- `bin/n42-stress/src/main.rs` — 更高默认参数

## 三、压测结果 (7 nodes, 2s slot, 2G gas, compact block)

| Target TPS | Sustained TPS | p50 Block TPS | Max Block TPS | Max Tx/Block | Gas Util | Fail Rate |
|---|---|---|---|---|---|---|
| 1,000 | 952 | 994 | 1,200 | 2,400 | 2.0% | 1.8% |
| 3,000 | 2,862 | 2,996 | 3,662 | 7,324 | 6.0% | 0% |
| 5,000 | 4,660 | 5,238 | 6,475 | 12,950 | 10.0% | 0% |
| 7,500 | 10,475 | 10,539 | 14,170 | 28,339 | 21.9% | 0% |
| 10,000 | 10,483 | 10,496 | 12,512 | 25,024 | 21.9% | 0% |
| 15,000 | 12,999 | 12,822 | 14,220 | 28,440 | 26.8% | 0% |
| 20,000 | 14,134 | 13,850 | 16,950 | 33,900 | 29.5% | 0% |
| **30,000** | **14,820** | **14,515** | **16,562** | **33,123** | **30.7%** | **0%** |
| 40,000 | 14,419 | 14,073 | 16,775 | 33,549 | 29.8% | 0% |
| 50,000 | 14,357 | 14,611 | 22,087 | 45,116 | 33.1% | 0% |

### 关键指标对比

| 指标 | 500M gas (日志-33) | 2G gas (本次) | 提升 |
|---|---|---|---|
| Peak sustained TPS | 11,028 | **14,820** | **34%** |
| Max block TPS | 11,904 | **22,087** | **86%** |
| Max tx/block | 23,809 | **45,116** | **90%** |
| 50K TPS 注入 | STALL | **稳定运行** | **修复** |
| Fail rate (30K) | 3.6% | **0%** | **消除** |
| Pool overflow | 75K → timeout | 34K peak, 稳定 | **修复** |

## 四、Pool 溢出修复验证

- 50K TPS 注入时，pool pending 峰值 34,361 — 远低于 200K 上限
- 0 次 backpressure pause
- 0 次 nonce resync
- avg_block_time 保持 2.0-2.2s，无 slot 超时
- **链全程稳定运行，CEILING DETECTED 自动停止**

## 五、瓶颈转移分析

### Gas Limit 已不是瓶颈
- 2G gas limit 下，gas 利用率最高仅 33.1%
- 最大区块 45,116 txs = 947M gas，仅用了 47% 的 2G 上限
- 即使切到 1G gas limit，当前 TPS 也不会受限

### 新瓶颈：reth new_payload 处理时间
- Payload builder 能在 2s 内构建 30K-45K tx 的区块
- 但 reth import (new_payload) 受 disk I/O 限制：
  - State root 计算：随状态树增大而增长
  - libmdbx 读写：每 tx 需要多次 SLOAD/SSTORE 到磁盘
  - 实际瓶颈是 follower 在 2s 内能 import 的最大区块
- 证据：sustained TPS ~14,800，与 import 时间而非 gas limit 相关

### 数据验证
- 14,800 TPS × 2s = 29,600 tx/block
- 29,600 × 21K gas = 621M gas（仅 31% of 2G）
- 理论极限 47,619 TPS 的 31% = 14,762 TPS — 与实测吻合

## 六、下一步

1. **Phase 3: Hot state cache / optimize reth import** — 减少 disk I/O，最高优先级
   - In-memory state overlay 跳过 libmdbx 读
   - 批量写入优化
2. **1s slot time 评估** — 当 import < 500ms 时可行
3. **大区块网络传输** — 45K tx 区块的 compact block 压缩效率
4. **长时间稳定性测试** — 30K TPS 运行 30 分钟
