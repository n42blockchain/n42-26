# 开发日志-09：版本化顺序读取流格式重构

> 日期：2026-02-20
> 阶段：移动验证数据包协议 V2

---

## 设计决策

### 核心洞察

同一区块、同样交易 → revm `Database::basic()`/`storage()`/`block_hash()` 调用顺序 **100% 确定性**。JournaledState 缓存首次读取 → 流只需首次读取值。因此可以去掉所有 address/slot key，只保留有序值流。

### 方案选择

| 考虑方案 | 优点 | 缺点 | 决定 |
|----------|------|------|------|
| 当前 bincode(VerificationPacket) | 成熟 | 含冗余 key、需 preimage、需 pre-state fixup | 保留但不再使用 |
| Protobuf/FlatBuffers | 标准化 | 额外依赖、对齐流反而增加开销 | 放弃 |
| **自定义紧凑二进制流** | 极简、零 key、条件字段、版本化 | 需自行维护编解码 | **采用** |

### 关键设计：ReadLogDatabase 直接捕获 pre-state

旧流程：execute → capture POST-state witness → fix_witness_pre_state（查询父状态修正）
新流程：ReadLogDatabase 包装 parent state DB → execute → log 捕获的就是 pre-state → 无需修正

这消除了 `fix_witness_pre_state` 和 `packet_builder`（preimage 解析）两个复杂模块。

---

## 实施细节

### Wire Format Header（4B 共用头）

```
magic(0x4E32 "N2") + version(0x01) + flags(1B)
```

应用于 StreamPacket、CacheSyncMessage、VerificationReceipt 三种消息。

### 新模块/文件

| 文件 | 功能 |
|------|------|
| `n42-mobile/src/wire.rs` | Wire 头部编解码、WireError |
| `n42-execution/src/read_log.rs` | ReadLogEntry 枚举、ReadLogDatabase wrapper、encode/decode |
| `n42-mobile/src/packet.rs` (新增) | StreamPacket 结构、encode_stream_packet/decode_stream_packet |
| `n42-mobile/src/verifier.rs` (新增) | StreamReplayDB + verify_block_stream |
| `n42-mobile/src/code_cache.rs` (新增) | encode_cache_sync/decode_cache_sync |
| `n42-mobile/src/receipt.rs` (新增) | encode_receipt/decode_receipt |

### ReadLogDatabase (`n42-execution/src/read_log.rs`)

- 包装任意 `Database`，用 `Arc<Mutex<Vec<ReadLogEntry>>>` 捕获读取日志
- `basic()` → 记录 Account/AccountNotFound + 捕获 bytecode
- `storage()` → 记录 Storage(value)
- `block_hash()` → 记录 BlockHash(hash)
- `code_by_hash()` → **不记录**（bytecodes 走独立通道）

### StreamReplayDB (`n42-mobile/src/verifier.rs`)

- 顺序消费 ReadLogEntry，忽略 address/slot/number 参数
- `code_by_hash()` 从 bytecodes HashMap 查找，不消费 cursor
- fail-safe：如果执行顺序不一致 → 返回错误值 → receipts_root 不匹配

### ReadLogEntry 紧凑编码

| Entry | 编码 |
|-------|------|
| AccountNotFound | tag(0x00) + data_len(0) |
| EOA(nonce=0, balance=0) | tag(0x01) + data_len(1) + flags(0x00) → **仅 4 字节** |
| EOA(有值) | tag(0x01) + flags + 条件 nonce(8B) + 条件 balance(32B) |
| Contract | tag(0x02) + flags + 条件字段 + code_hash(32B) |
| Storage | tag(0x03) + value(32B) |
| BlockHash | tag(0x04) + hash(32B) |

### IDC 端集成（`n42-node/src/mobile_packet.rs`）

- `generate_and_broadcast_v2()`：ReadLogDatabase → execute → 提取 log + codes → StreamPacket → zstd → broadcast
- CacheSyncMessage 使用 `encode_cache_sync()` 替代 `bincode::serialize`
- 移除对 `packet_builder`、`fix_witness_pre_state`、`execute_block_with_witness` 的依赖

---

## 遇到的问题及解决方案

1. **`alloy_evm::Database` vs `revm::Database`**：reth 的 executor 要求 `alloy_evm::Database`，它在 `revm::Database` 基础上额外要求 `Debug` 和 `Error: std::error::Error + Send + Sync + 'static`。解决：为 ReadLogDatabase 和 StreamReplayDB 添加 `#[derive(Debug)]`，为 StreamDbError 使用 `thiserror::Error`。

2. **`StorageKey` 类型差异**：`revm::primitives::StorageKey` 是 `U256`，而 `alloy_primitives::StorageKey` 是 `B256`。解决：明确使用 `revm::primitives::StorageKey`。

3. **Arc::try_unwrap 类型推断**：executor 消费 DB 后通过 Arc 恢复日志数据。`unwrap_or_else` 的 clone 返回值类型需要与 Mutex 匹配。解决：使用 match 模式替代 unwrap_or_else。

4. **n42-execution 缺少 thiserror**：为 DecodeError 添加 thiserror derive 时发现 Cargo.toml 未列出此依赖。解决：添加 `thiserror.workspace = true`。

---

## 阶段完成状态

- [x] Step A: Wire Format 基础模块（wire.rs、versioned encode/decode for CacheSync/Receipt）
- [x] Step B: ReadLogDatabase + encode/decode（read_log.rs）
- [x] Step C: StreamPacket 编解码（packet.rs 新增）
- [x] Step D: StreamReplayDB + verify_block_stream（verifier.rs 新增）
- [x] Step E: IDC 端集成（mobile_packet.rs 重写为 V2）
- [x] Step F: 全量编译 + 212 个测试通过
- [x] Step G: 零分配游标重构 + V1/V2 数据量对比测试 + 213 个测试通过
- [x] Step H: 端到端集成测试（真实 EVM 执行：ETH 转账 + ERC-20 转账 + 混合区块）+ 216 个测试通过
- [x] Step I: 紧凑二进制编码（比 logbin 更短）+ read_log 缩小 60-75% + 221 个测试通过

### 测试覆盖

| 包 | 通过测试数 |
|----|-----------|
| n42-execution | 48 |
| n42-mobile | 78 |
| n42-node (lib) | 87 |
| n42-node (stream_v2_pipeline) | 3 |
| **总计** | **216** |

---

## Step G：零分配游标重构（2026-02-20）

### 问题诊断

初版 V2 实现存在两个与"纯数据流"设计意图的偏差：

1. **数据量缩减未量化**：去掉 address/slot key 后理论缩小，但缺少实测
2. **中间分配**：`StreamReplayDB` 持有 `Vec<ReadLogEntry>`（从字节 decode 出的枚举对象），每次 verify 都 clone 整个 Vec

### 解决方案

**StreamReplayDB 改为字节流游标模式：**
- `data: Vec<u8>` + `pos: usize`（字节偏移）替代 `entries: Vec<ReadLogEntry>` + `cursor: usize`（条目索引）
- 每次 `basic()`/`storage()`/`block_hash()` 直接从 `data[pos..]` 读取 tag(1B)+data_len(2B)+data，解析为目标类型
- 零中间 `ReadLogEntry` 分配，零 Vec clone

**StreamPacket 改为持有预编码字节：**
- `read_log_data: Vec<u8>` 替代 `read_log: Vec<ReadLogEntry>`
- wire 格式增加 `read_log_byte_len: u32 LE` 前缀，解码时直接切片、不解析条目
- IDC 端在提取 log 后立即 `encode_read_log()` 为字节，一路传递到广播

**read_log.rs 导出编码常量：**
- `TAG_ACCOUNT_NOT_FOUND`/`TAG_ACCOUNT_EOA`/`TAG_ACCOUNT_CONTRACT`/`TAG_STORAGE`/`TAG_BLOCK_HASH` 改为 `pub`
- `ACCT_FLAG_NONCE_NONZERO`/`ACCT_FLAG_BALANCE_NONZERO` 改为 `pub`

### V1 vs V2 数据量对比（test_v1_v2_size_comparison）

测试场景：10 个账户（混合 EOA/合约），共 20 个 storage slot，3 个 block hash，5 笔交易，1 个 bytecode。

| 指标 | V1 (VerificationPacket) | V2 (StreamPacket) | 缩减 |
|------|------------------------|-------------------|------|
| 原始编码大小 | 5,181 bytes | 3,117 bytes | **-39.8%** |
| read_log 部分 | — | 1,201 bytes (33 条目) | — |

主要节省来源：
- 每个 account 省去 address(20B)
- 每个 storage slot 省去 key(32B)
- 每个 block hash 省去 number(8B)
- V2 无 block_number/parent_hash/state_root/transactions_root/receipts_root 等冗余字段（都在 header_rlp 里）
- 条件编码：零 nonce/balance 的 EOA 仅需 4 字节

### 修改文件

| 文件 | 变更 |
|------|------|
| `n42-execution/src/read_log.rs` | tag/flag 常量改为 `pub` |
| `n42-mobile/src/packet.rs` | `StreamPacket.read_log` → `read_log_data: Vec<u8>`；encode/decode 增加长度前缀；新增 V1/V2 对比测试 |
| `n42-mobile/src/verifier.rs` | `StreamReplayDB` 重写为字节游标模式（data+pos）；inline 解析替代枚举匹配 |
| `n42-node/src/mobile_packet.rs` | 提取 log 后立即 encode；StreamPacket 使用 `read_log_data` |

---

## 待分析问题

### bytecodes 的 HashMap 结构

`StreamReplayDB.bytecodes: HashMap<B256, Bytecode>` 仍存在，因为 EVM `code_by_hash(hash)` 是按哈希随机访问，不是顺序读取。可能的优化：
- **方案 A**：将 bytecode 嵌入读取流，在 Account(Contract) 条目之后紧跟 bytecode 数据
- **方案 B**：用 `Vec<(B256, Bytecode)>` + 线性扫描替代 HashMap，合约数量通常很少

---

## Step H：端到端集成测试 — 真实 EVM 执行验证（2026-02-20）

### 目标

验证 V2 流格式在真实 EVM 执行下的正确性：IDC 端用 ReadLogDatabase 执行区块、捕获读取日志 → 编码为 StreamPacket → Phone 端用 StreamReplayDB 回放 → 两侧 receipts_root 完全一致。

### 测试文件

`crates/n42-node/tests/stream_v2_pipeline.rs`

### 测试场景

| 测试 | 交易类型 | Read Log 条目 | Read Log 字节 | 捕获代码 | Receipts |
|------|---------|--------------|--------------|---------|----------|
| `test_v2_pipeline_eth_transfer` | ETH 转账 (1 ETH) | 3 | 79 B | 0 | 1 |
| `test_v2_pipeline_erc20_transfer` | ERC-20 transfer() | 5 | 157 B | 1 | 1 |
| `test_v2_pipeline_mixed_block` | ETH + ERC-20 混合 | 7 | 196 B | 1 | 2 |

### 关键实现

**手工 ERC-20 合约字节码** (`minimal_erc20_bytecode()`)：
- 纯 EVM 汇编构建，支持 `transfer(address,uint256)` 和 `balanceOf(address)`
- 存储布局与 Solidity 一致：`balances[addr]` 在 `keccak256(abi.encode(addr, 0))` slot
- 测试覆盖：SLOAD/SSTORE（sender+receiver balance）、CALLER、SHA3、CALLDATALOAD

**测试流水线** (`run_pipeline_test()`)：
1. IDC 侧：`ReadLogDatabase::new(CacheDB)` → `BasicBlockExecutor::new(evm_config, logged_db)` → `execute_one(&block)`
2. 提取 `read_log`（`Arc::try_unwrap`）→ `encode_read_log()` → 字节数据
3. Phone 侧：`StreamReplayDB::new(read_log_data, bytecodes)` → `BasicBlockExecutor::new(evm_config, replay_db)` → `execute_one(&block)`
4. 逐笔比对 receipt.success / cumulative_gas_used / logs
5. `Receipt::calculate_receipt_root_no_memo()` 比对 receipts_root

### Read Log 分析

ETH 转账（3 条目，79 字节）：
```
[0] Account(sender): nonce=0, balance=10 ETH, code_hash=EMPTY
[1] Account(receiver): nonce=0, balance=1 ETH, code_hash=EMPTY
[2] AccountNotFound(coinbase): 首次被 revm 访问
```

ERC-20 转账（5 条目，157 字节）：
```
[0] Account(sender): nonce=0, balance=10 ETH
[1] Account(contract): nonce=1, code_hash=0x8513...
[2] Storage(sender_balance=1000000)
[3] Storage(receiver_balance=0)
[4] AccountNotFound(coinbase)
```

混合区块（7 条目，196 字节）：
```
[0] Account(sender_a)           ← ETH 转账读取
[1] AccountNotFound(receiver)   ← receiver 首次出现
[2] AccountNotFound(coinbase)   ← coinbase 首次出现
[3] Account(sender_b)           ← ERC-20 转账读取
[4] Account(contract)           ← 合约（有 code_hash）
[5] Storage(sender_b_balance)   ← SLOAD
[6] Storage(receiver_balance)   ← SLOAD
```

注意：条目 [1] receiver 和 [2] coinbase 在第二笔交易中不再出现 — 因为 JournaledState 已缓存，不再调用 Database trait。这正是"仅首次读取"设计的验证。

### 新增依赖

| 文件 | 变更 |
|------|------|
| `Cargo.toml` (workspace) | 添加 `reth-testing-utils` workspace 依赖 |
| `crates/n42-node/Cargo.toml` | 添加 dev-dependencies: `revm`, `reth-testing-utils` |

---

## Step I：紧凑二进制编码 — 比 logbin 更短（2026-02-20）

### 问题诊断

对比 pevm 的 logbin 格式发现旧 V2 编码效率不足：

| 来源 | 旧 V2 每条目开销 | logbin 每条目开销 |
|------|-----------------|-----------------|
| 头部 | 3B（tag+data_len） | 1B（length） |
| nonce | 固定 8B | Compact 变长 |
| balance | 固定 32B | Compact 变长 |
| storage value | 固定 32B | CompactU256 变长 |
| block_hash | 35B（tag+len+32B） | 不记录 |

### 新编码设计

**核心思路**：单个头字节同时编码类型和元数据，消除 tag/data_len 字段。

```text
0x00        = AccountNotFound                    (1B 总计)
0x01-0x7F   = Account exists, flags packed:
  bit0      = 1 (exists marker)
  bit1-3    = nonce byte count (0-7)
  bit4      = balance nonzero
  bit5      = has_code_hash (contract)
  bit6      = nonce uses 8 bytes (overrides bit1-3)
0x80-0xA0   = Storage, value_len = header - 0x80 (1 + value_len bytes)
0xC0        = BlockHash                          (1 + 32 bytes)
```

变长字段使用大端序、去除前导零。

### 数据量对比

**合成测试（10 账户、20 slot、3 block hash、5 笔交易、1 bytecode）：**

| 指标 | V1 (VerificationPacket) | 旧 V2 | **新紧凑 V2** |
|------|------------------------|-------|--------------|
| 总编码大小 | 5,181 B | 3,117 B | **2,227 B** |
| read_log 部分 (33 条目) | — | 1,201 B | **311 B** |
| 缩减 (vs V1) | — | -39.8% | **-57.0%** |

**端到端集成测试（真实 EVM 执行）：**

| 测试 | 条目 | 旧 V2 | **新紧凑** | 缩减 |
|------|------|-------|----------|------|
| ETH 转账 | 3 | 79 B | **25 B** | -68.4% |
| ERC-20 转账 | 5 | 157 B | **54 B** | -65.6% |
| 混合区块 | 7 | 196 B | **65 B** | -66.8% |

read_log 部分 **缩小 60-75%**，总包大小 **缩小 57%**（vs V1）。

### 为什么比 logbin 更短

| 场景 | logbin | 新紧凑格式 |
|------|--------|-----------|
| AccountNotFound | 2B（len=1 + data=1B） | **1B**（header=0x00） |
| 零值 EOA | ~3B（len + Compact empty） | **1B**（header=0x01） |
| 零值 Storage | ~2B（len + CompactU256=0） | **1B**（header=0x80） |
| 小值 Storage (1B) | 2B（len + data） | **2B**（header=0x81 + 1B） |

关键优势：头字节同时编码类型和长度，0 额外开销；logbin 每条目固定 1B length 前缀。

### 修改文件

| 文件 | 变更 |
|------|------|
| `n42-execution/src/read_log.rs` | 完全重写编码格式：新常量（HEADER_*、ACCT_*）、变长 nonce/balance/storage、新 encode/decode |
| `n42-mobile/src/verifier.rs` | StreamReplayDB Database impl 重写为直接解析新格式；移除死代码 `read_entry_raw()` |

### 测试覆盖

| 包 | 通过测试数 |
|----|-----------:|
| n42-execution | 53 |
| n42-mobile | 78 |
| n42-node (lib) | 87 |
| n42-node (stream_v2_pipeline) | 3 |
| **总计** | **221** |

---

---

## Step J: 代码审计、安全加固与优化

> 日期：2026-02-21

### 审计发现与修复

#### 安全加固：OOM 防护

在 decode_cache_sync、decode_stream_packet 中发现了与 read_log 相同的 OOM 风险 — 恶意 count 字段可触发 `Vec::with_capacity` 分配巨量内存。

| 文件 | 修复 | 上限值 |
|------|------|--------|
| `code_cache.rs` decode_cache_sync | code_count, evict_count 上限检查 | MAX_CACHE_SYNC_CODES = 10,000 |
| `packet.rs` decode_stream_packet | tx_count, code_count 上限检查 | MAX_TX_COUNT = 100,000; MAX_BYTECODE_COUNT = 10,000 |

#### 代码清理

- `mobile_packet.rs`：移除冗余 `.map(|(h, c)| (h, c))` identity map
- `mobile_packet.rs`：移除未使用的 `BlockExecutionError` import

### 优化：StreamReplayDB 惰性 code_cache 查找

**问题**：`verify_block_stream` 中原先遍历整个 `code_cache.cached_hashes()` 将所有缓存条目 clone 到 HashMap — O(cache_size) 操作，而运行时通常只需 0-5 个 code。

**优化方案**：`StreamReplayDB` 增加 `code_cache: Option<&'a mut CodeCache>` 引用字段：
- `code_by_hash()` 先查 `bytecodes` map（来自当前 packet），miss 后懒查 `code_cache`
- 查到后缓存到 `bytecodes` 避免重复查询
- `new_without_cache()` 无 cache 版本用于测试和集成场景

**结构变更**：
```rust
pub struct StreamReplayDB<'a> {
    // ... existing fields ...
    bytecodes: HashMap<B256, Bytecode>,
    code_cache: Option<&'a mut CodeCache>,  // NEW: lazy fallback
}
```

手动实现 `Debug`（因为 `CodeCache` 包含 `LruCache` 未实现 Debug）。

### 新增测试

| 测试 | 覆盖 |
|------|------|
| `test_stream_replay_db_lazy_code_cache_fallback` | code_by_hash 从 cache 懒查找，结果缓存到 bytecodes |
| `test_stream_replay_db_packet_code_takes_precedence` | packet bytecodes 优先于 cache |

### 测试统计

| 模块 | 测试数 |
|------|--------|
| n42-execution | 81 |
| n42-mobile | 94 (+2) |
| n42-node (lib) | 87 |
| n42-node (stream_v2_pipeline) | 3 |
| **总计** | **265** |

### 修改文件

| 文件 | 变更 |
|------|------|
| `crates/n42-mobile/src/code_cache.rs` | OOM 防护常量 + decode_cache_sync 上限检查 |
| `crates/n42-mobile/src/packet.rs` | OOM 防护常量 + decode_stream_packet 上限检查 |
| `crates/n42-mobile/src/verifier.rs` | StreamReplayDB 加生命周期参数 + 惰性 cache + 手动 Debug + 2 新测试 |
| `crates/n42-node/src/mobile_packet.rs` | 移除冗余代码 + unused import |
| `crates/n42-node/tests/stream_v2_pipeline.rs` | 适配 new_without_cache API |

---

## Step K：主执行路径逻辑检查与修复

> 日期：2026-02-21

### 执行路径分析

对 IDC → 网络 → 手机 的完整执行路径进行了逐层追踪：

**IDC 端路径：**
```
orchestrator.rs: BlockCommitted → mobile_packet_tx.try_send((block_hash, view))
  → mobile_packet_loop: rx.recv() → generate_and_broadcast_v2()
    → provider.recovered_block() → ReadLogDatabase::new()
    → executor.execute_one() → encode_read_log + encode_stream_packet
    → zstd::compress → hub_handle.broadcast_packet()
```

**网络层路径：**
```
ShardedStarHubHandle → fan-out to shard StarHubs
  → StarHub: preframe_message(MSG_TYPE_PACKET_ZSTD=0x03, compressed)
  → QUIC uni-stream → phone
```

**手机端路径：**
```
n42-mobile-ffi recv_loop: accept_uni → read → msg_type prefix strip
  → 0x03: zstd decompress → pending_packets queue
n42_verify_and_send: poll packet → decode → verify → sign receipt → send
```

### 发现的问题

#### 问题 1（严重）：FFI 接收端未适配 V2 格式

**现象：** IDC 已使用 `encode_stream_packet()` 和 `encode_cache_sync()` 发送 V2 wire header 格式，但 FFI 端仍使用 V1 解码：
- `n42_verify_and_send`: `decode_packet()` + `verify_block()` → 无法解码 V2 StreamPacket
- `recv_loop` 0x02/0x04 handler: `bincode::deserialize::<CacheSyncMessage>()` → 无法解码 V2 CacheSyncMessage

**影响：** 整个手机验证管线完全无法工作。

**修复方案：** 通过 magic bytes `[0x4E, 0x32]` 自动检测 V2 vs V1 格式：
- `is_v2_wire_format()` 辅助函数：检查前 2 字节
- `n42_verify_and_send`: V2 → `decode_stream_packet()` + `verify_block_stream()`，V1 → 原逻辑
- `recv_loop` 0x02/0x04: V2 → `decode_cache_sync()`，V1 → `bincode::deserialize`
- 两条路径共享后续逻辑（receipt 签名、stats 更新、LastVerifyInfo）

#### 问题 2（语义修正）：orchestrator view vs block_number

**现象：** orchestrator 在 3 处发送 `(block_hash, view)` 到 `mobile_packet_tx`，但类型别名文档和 `mobile_packet_loop` 都将第二字段解读为 `block_number`。HotStuff-2 的 `view` 是共识轮次号，在 view change 时 view > block_number。

**影响：** 仅影响日志（语义错误），不影响功能（block_number 不参与 packet 构建）。

**修复方案：**
- `generate_and_broadcast_v2` 移除 `block_number` 参数
- 函数内部从 `header.number()` 提取真实 block_number 用于日志
- 类型别名文档改为 `(block_hash, consensus_view)`
- `mobile_packet_loop` 中解构为 `(_view)` 表明不使用

### 额外改进

- `StreamPacket::header_info()` 便捷方法：提取 `(block_number, receipts_root)` 而无需 FFI 引入 alloy 依赖
- 更新 `recv_loop` 文档注释：完整列出所有消息类型（0x01-0x04）

### 修改文件

| 文件 | 变更 |
|------|------|
| `crates/n42-mobile/src/packet.rs` | `StreamPacket::header_info()` 便捷方法 |
| `crates/n42-mobile-ffi/src/lib.rs` | V2 格式自动检测、`n42_verify_and_send` 双路径、`recv_loop` V2 CacheSync |
| `crates/n42-node/src/mobile_packet.rs` | 移除 `block_number` 参数，用 `header.number()` 替代；类型别名文档修正 |
| `crates/n42-node/src/orchestrator.rs` | 文档注释修正：`block_number` → `consensus_view` |

### 测试结果

- n42-execution: 81 passed
- n42-mobile: 94 passed
- n42-node: 87 passed
- n42-mobile-ffi: 37 passed
- **总计 299 tests, all passing**

---

## Step L：V2 端到端测试补全

> 日期：2026-02-21

### 问题

Step K 的测试全部通过（299 个），但仔细审查后发现：**没有一个测试覆盖完整的 V2 packet 编解码 + 验证路径**。

现有的 `stream_v2_pipeline.rs` 直接使用 `StreamReplayDB`，跳过了：
- `encode_stream_packet()` → wire format 编码
- zstd 压缩 → 解压
- `decode_stream_packet()` → wire format 解码
- `verify_block_stream()` → 完整验证入口

### 发现的第一个 bug

首次运行端到端测试时，**3 个测试全部失败**：
```
receipts root must match!
  computed=0x056b23..., expected=0x56e81f... (KECCAK_EMPTY_ROOT)
```

原因：测试中手动构造的 `Header` 使用默认的 `receipts_root`（空 trie root），但 `verify_block_stream` 会对比 header 中的 `receipts_root` 与实际执行结果。在生产环境中，区块构建器会先计算正确的 receipts_root 再写入 header。

修复：采用**两遍执行**策略（与真实区块构建流程一致）：
1. 第一遍：执行区块，计算 receipts_root
2. 将 receipts_root 设入 header
3. 第二遍：用 ReadLogDatabase 执行，构建 StreamPacket

### 新增测试

**集成测试（stream_v2_pipeline.rs）— 3 个新测试：**

| 测试 | 场景 | 覆盖路径 |
|------|------|----------|
| `test_v2_full_packet_eth_transfer` | 简单 ETH 转账 | 全路径 |
| `test_v2_full_packet_erc20_transfer` | ERC-20 代币转账 | 全路径 + bytecodes |
| `test_v2_full_packet_mixed_block` | 混合区块（ETH + ERC-20） | 全路径 + 多交易 |

每个测试覆盖完整链路：
```
ReadLogDatabase → encode_read_log → StreamPacket
  → encode_stream_packet → zstd compress
  → zstd decompress → decode_stream_packet
  → verify_block_stream → receipts_root match ✓
```

**FFI 测试 — 3 个新测试：**

| 测试 | 验证内容 |
|------|----------|
| `test_is_v2_wire_format_detection` | magic bytes 自动检测逻辑正确性 |
| `test_verify_v2_packet_enters_v2_path` | `n42_verify_and_send` V2 路径触发确认 |
| `test_cache_sync_v2_decode` | V2 CacheSyncMessage encode → decode → apply 全流程 |

### 最终测试结果

- n42-execution: 81 passed
- n42-mobile: 94 passed
- n42-node (lib): 87 passed
- n42-mobile-ffi: 40 passed (+3)
- stream_v2_pipeline: 6 passed (+3)
- **总计 308 tests, all passing**

---

## 后续计划

1. **删除旧代码**：`packet_builder.rs` 不再被 mobile_packet.rs 使用，可在确认无其他引用后删除
2. **旧格式兼容**：目前保留了 VerificationPacket 和 verify_block()，后续版本可移除
3. ~~集成测试：在真实链上验证 IDC → phone 全链路~~ → **已完成**（Step H）
4. ~~紧凑编码优化~~ → **已完成**（Step I：比 logbin 更短）
5. ~~代码审计与安全加固~~ → **已完成**（Step J）
6. ~~主执行路径逻辑检查~~ → **已完成**（Step K）
7. **Receipt V2 迁移**：FFI 发送和 StarHub 接收的 receipt 仍用 bincode，后续可迁移到 `encode_receipt()` wire 格式
8. **性能基准**：在真实区块数据上对比 V1/V2 编码大小和 zstd 压缩比
9. **版本协商**：支持 phone 通告能力版本，IDC 按能力选择发送 V1 或 V2
