# 开发日志-28：N42 性能优化方案设计

> 日期：2026-03-06
> 目标：基于瓶颈定位（日志-27）和行业方案调研，设计 N42 从 2000 TPS 到 50,000+ TPS 的优化路线

---

## 一、当前瓶颈总结

### 实测数据（7 节点本地测试网，4s slot）

| 指标 | 空块 | 中负载 | 满块 (23k txs) | 放大倍数 |
|------|------|--------|---------------|---------|
| Commit 延迟 | **36ms** | 477-2650ms | **25,680ms** | **713x** |
| Import 延迟 | 13ms | 110-441ms | 1,401ms | 108x |
| Build 时间 | 0ms | ~16ms | ~200ms | N/A |

### 瓶颈根因

**完整 payload JSON 通过 GossipSub 广播**：

```
handle_built_payload() → serde_json::to_vec(&execution_data) → bincode::serialize(BlockDataBroadcast)
→ network.announce_block(encoded)  // 完整 3.5MB 数据走 GossipSub mesh
```

关键代码路径：`execution_bridge.rs:694-772`

GossipSub 的 mesh 转发机制导致：
1. 每个节点收到消息后转发给 mesh 中的 D 个 peer（默认 D=8）
2. 大消息（3.5MB）的传输时间远超小消息
3. mesh 中多条路径同时传输同一大消息，浪费带宽
4. 节点互相发送相同消息的概率随消息大小急剧上升

### EVM 不是瓶颈

EVM 执行仅占总时间 <5%（16-200ms vs 25,680ms commit）。即使 EVM 执行时间降为 0，TPS 也不会显著提升。

---

## 二、行业高性能 EVM 链方案总结

### 2.1 Monad — 延迟执行 + RaptorCast

**核心创新**：
- **延迟执行 (Deferred Execution)**：共识只确定交易顺序，不等待执行完成。Block N 执行时，Block N+1 已在共识，Block N+2 已在提案
- **RaptorCast 块传播**：使用 Raptor 纠删码将块编码为多个 chunk（如 1000KB → 150 个 20KB chunk，任意 50 个即可重构）。两级广播树：Leader → 每个验证者各发不同 chunk → 验证者间互转，最坏延迟仅 2 跳
- **MonadBFT**：基于 HotStuff 优化的 2 阶段共识（N42 已采用 HotStuff-2，架构一致）
- **乐观并行执行**：同时处理多笔 tx，冲突时重执行

**性能**：400ms 出块，10,000 TPS mainnet

### 2.2 MegaETH — 极致节点专业化

**核心创新**：
- **节点专业化**：Sequencer（100+ 核，1-4TB RAM）、Replica（不重执行）、Prover（无状态验证，1核 0.5GB）
- **SALT 状态架构**：认证结构（Merkle proof）全部放入 RAM，30 亿 key-value 仅 ~1GB 内存
- **流式执行**：10ms 出块，持续处理交易流

**性能**：35,000 持续 TPS（一周压测），目标 100,000 TPS

### 2.3 Sei Giga — 异步状态承诺

**核心创新**：
- **异步状态承诺**：共识不包含 state root，由后续块异步提交。Merkle tree 更新 offload 到并行线程
- **State/Commitment 分离**：状态存储（flat KV）与状态承诺（Merkle）解耦
- **多提案者**：Autobahn 协议允许所有验证者在各自 lane 同时提案

**性能**：200K TPS 目标，50x 吞吐量提升

### 2.4 以太坊 EIP-7862 — Delayed State Root

- Block N 的 header 包含 Block N-1 的 state root
- 验证者基于交易执行结果 attestation，无需等待 state root 计算
- 已是以太坊正式 EIP

### 2.5 Reth 生态

| 项目 | 描述 | 适用性 |
|------|------|--------|
| **PEVM** | Block-STM 并行 EVM，reth 兼容，平均 2x 加速 | 可集成 |
| **Ress** | 无状态 reth 节点，磁盘 14GB，witness-based 验证 | 手机验证场景 |
| **Revmc** | EVM JIT/AOT 编译器，计算密集合约 6.9x 加速 | 合约场景 |
| **Gravity Reth** | 16 路并行 Merkle 化，state root 3-10x 加速 | 可参考 |

---

## 三、N42 优化方案设计

### 架构适用性分析

N42 的独特优势：
- **仅 7 个 IDC 节点**：不需要复杂的广播协议，可直接推送
- **HotStuff-2 共识**：2 阶段，天然支持流水线化
- **IDC + 手机分层**：天然的节点专业化架构
- **独立 L1**：可自由修改协议规则，无需等以太坊硬分叉

---

### Phase 0：块大小限制（立即，< 1 天）

**目标**：防止死亡螺旋，稳定持续 TPS

**设计**：在 `default_ethereum_payload` 的 tx 循环中添加 max_txs_per_block 限制

```rust
// payload builder 配置
let max_txs = std::env::var("N42_MAX_TXS_PER_BLOCK")
    .ok()
    .and_then(|v| v.parse::<usize>().ok())
    .unwrap_or(5000); // 默认 5000 txs/block

let mut tx_count = 0;
while let Some(pool_tx) = best_txs.next() {
    if tx_count >= max_txs {
        break; // 块大小限制
    }
    // ... existing logic ...
    tx_count += 1;
}
```

**预期效果**：
- 5000 txs/block ≈ 750KB（vs 23k txs = 3.5MB）
- Commit 延迟从 25s 降至 ~2s（线性关系）
- 持续 TPS = 5000 / 4s = 1250（牺牲峰值换稳定）
- 配合 1s slot：持续 TPS = 5000 / 1s = 5000

**改动文件**：
| 文件 | 改动 |
|------|------|
| `reth/crates/ethereum/payload/src/lib.rs` | 添加 max_txs_per_block 限制 |

---

### Phase 1：Leader 直推替代 GossipSub（短期，3-5 天）

**目标**：2000 → 8000-10000 TPS

**核心思想**：7 个 IDC 节点不需要 GossipSub mesh 的冗余传播。Leader 直接通过 TCP/QUIC 推送完整块数据给所有 follower。

**当前问题**：
```
Leader → GossipSub mesh → 多跳转发 → 所有节点
  每个节点收到后还会再转发给 D 个 peer → 大量重复传输
  3.5MB × 多跳 × 重复 = 网络带宽爆炸
```

**优化后**：
```
Leader → 直接发送给 6 个 follower（并行 QUIC 流）
  每个 follower 只收一次完整数据
  3.5MB × 6 = 21MB 总带宽（vs GossipSub 的 3.5MB × 多跳 × D）
```

**设计方案**：

1. **新增 Direct Block Push 通道**

在 `n42-network` 中新增 request-response 协议（类似已有的 `consensus_direct`）：

```rust
// n42-network/src/block_direct.rs
pub struct BlockDirectProtocol;

pub enum BlockDirectRequest {
    PushBlock {
        view: u64,
        block_hash: B256,
        payload_json: Vec<u8>,  // 完整 execution payload
        timestamp: u64,
    },
}

pub enum BlockDirectResponse {
    Accepted,
    AlreadyHave,
    Rejected(String),
}
```

2. **Leader 广播改为直推**

修改 `broadcast_block_data()`：

```rust
fn broadcast_block_data(
    network: &NetworkHandle,
    leader_payload_tx: &mpsc::UnboundedSender<(B256, Vec<u8>)>,
    hash: B256,
    current_view: u64,
    payload_json: &[u8],
    timestamp: u64,
) {
    // 方案 A：直接推送给所有已知 validator peers
    let broadcast = BlockDataBroadcast { block_hash: hash, view: current_view, payload_json: payload_json.to_vec(), timestamp };
    let encoded = bincode::serialize(&broadcast).unwrap();

    // 遍历 validator_peer_map，对每个 follower 直接发送
    for (idx, peer_id) in network.all_validator_peers() {
        network.send_block_direct(peer_id, encoded.clone());
    }

    // 保留 GossipSub 作为 fallback（仅发送 BlockAnnouncement 头部）
    let ann = BlockAnnouncement { block_hash: hash, block_number: ..., tx_count: ..., block_size: ... };
    network.announce_block(encode_block_announcement(&ann));

    let _ = leader_payload_tx.send((hash, encoded));
}
```

3. **Follower 接收优化**

在 `handle_block_data()` 中，优先处理直推数据，GossipSub 块头作为 fallback 触发按需拉取：

```rust
// 收到直推：立即处理
NetworkEvent::BlockDirectPush { data } => {
    self.handle_block_data(data).await;
}

// 收到 GossipSub 头部公告：检查是否已有完整数据
NetworkEvent::BlockAnnouncement { ann } => {
    if !self.has_block(ann.block_hash) {
        // 通过 request-response 从 leader 拉取
        self.request_block_from_peer(ann.source, ann.block_hash);
    }
}
```

**预期效果**：
- 块传播延迟：1 跳（Leader → Follower），预计 50-100ms（3.5MB / 1Gbps）
- Commit 延迟从 25s 降至 ~500ms
- 持续 TPS 提升 5-10x

**改动文件**：
| 文件 | 改动 |
|------|------|
| `n42-network/src/block_direct.rs` | 新增直推协议 |
| `n42-network/src/transport.rs` | 添加 block direct behaviour |
| `n42-network/src/service.rs` | 新增 NetworkCommand::SendBlockDirect |
| `execution_bridge.rs` | `broadcast_block_data()` 改为直推 + 头部公告 |

---

### Phase 2：Delayed State Root + Speculative Build（中期，5-7 天）

**目标**：8000 → 15000-20000 TPS

**核心思想**：借鉴 EIP-7862 + Monad 延迟执行，将 state root 计算从出块关键路径移除。

**当前管线**：
```
Build N (EVM + state_root) → Broadcast N → Consensus N → Import N → Build N+1
                                                                     ↑ 必须等 N 导入完成
```

**优化后管线**：
```
Build N (EVM only, no state_root) → Broadcast N → Build N+1 (speculative)
                                     ↓
                          [后台] state_root N 异步计算
                          [后台] Consensus N → Import N
```

**设计方案**：

1. **Block Header 引用 Parent State Root**

修改 N42 的 chainspec，Block N 的 header 包含 Block N-1 的 state root（而非 Block N 自身的）。

```
Block N header:
  parent_hash: hash(Block N-1)
  state_root: state_root_after_executing(Block N-1)   // 延迟 1 块
  transactions_root: merkle(Block N transactions)
```

这样 Build N 时不需要等 state root 计算完成。

2. **Speculative Build（推测性构建）**

Leader 在广播 Block N 后，使用 Block N 的 pending state（builder 已计算的内存状态）作为 parent 立即开始 Build N+1，不等 Import N 完成。

```rust
// execution_bridge.rs
async fn handle_built_payload(...) {
    // 1. 广播块数据
    broadcast_block_data(...);

    // 2. 触发投票
    let _ = block_ready_tx.send(hash);

    // 3. 立即用 pending state 开始下一个构建
    //    （不等 new_payload + fcu 完成）
    let _ = speculative_build_tx.send(SpeculativeBuildRequest {
        parent_hash: hash,
        parent_state: pending_state,  // 来自 builder 的内存状态
    });

    // 4. 后台异步导入
    tokio::spawn(async move {
        engine_handle.new_payload(execution_data).await;
        engine_handle.fork_choice_updated(fcu, None, ...).await;
    });
}
```

3. **异步 State Root 计算**

State root 在独立线程中计算，完成后写入下一个块的 header：

```rust
// 新增 AsyncStateRootWorker
struct AsyncStateRootWorker {
    pending_computations: VecDeque<(BlockHash, HashedPostState)>,
    completed: HashMap<BlockHash, B256>,
}

impl AsyncStateRootWorker {
    async fn compute_loop(&mut self) {
        while let Some((block_hash, hashed_state)) = self.pending_computations.pop_front() {
            let state_root = state.state_root_with_updates(hashed_state);
            self.completed.insert(block_hash, state_root);
        }
    }
}
```

**预期效果**：
- 消除 state root 从关键路径（~200ms per block）
- 消除 import 等待（~300-1400ms per block）
- 块间间隔从 build + broadcast + import 压缩到仅 build + broadcast
- 理论 TPS 提升 2-3x

**改动文件**：
| 文件 | 改动 |
|------|------|
| `execution_bridge.rs` | speculative build 逻辑 |
| `consensus_loop.rs` | 支持基于 pending state 的构建 |
| `mod.rs` | AsyncStateRootWorker |
| `payload_validator.rs` | 修改 state root 验证为延迟模式 |
| chainspec 或 genesis | 标记 delayed state root 激活高度 |

**注意**：这是最复杂的改动，需要仔细处理 reorg 场景下的 state 回滚。

---

### Phase 3：并行 EVM（中期，5-10 天）

**目标**：15000 → 30000+ TPS

**核心思想**：当块传播和 state root 不再是瓶颈后，EVM 顺序执行将成为下一个瓶颈。集成 PEVM 或 Grevm 实现乐观并行执行。

**技术选型**：推荐 **PEVM**
- 与 reth 原生兼容（基于 revm）
- Rust 实现，API 简洁
- Block-STM 乐观并行，简单转账几乎无冲突 → 接近线性加速
- 已有并行 Sparse Trie 的 state root 计算

**集成方案**：

在 `N42InnerPayloadBuilder::try_build()` 中替换 `default_ethereum_payload` 调用：

```rust
fn try_build(&self, args: BuildArguments<...>) -> Result<BuildOutcome<...>, ...> {
    let use_parallel = std::env::var("N42_PARALLEL_EVM").is_ok();

    if use_parallel {
        // 收集所有 best_txs
        let txs: Vec<_> = best_txs.collect();

        // PEVM 并行执行
        let result = pevm::execute_block(
            &self.evm_config,
            &state_provider,
            &txs,
            block_env,
            concurrency_level: num_cpus::get(),
        )?;

        // 构建 payload
        build_payload_from_result(result)
    } else {
        // 原有顺序执行路径
        default_ethereum_payload(...)
    }
}
```

**预期效果**（简单转账场景）：
- 顺序 EVM：~5700 tx/s（200ms for 23k txs）
- 并行 EVM（8 核）：~20,000-40,000 tx/s
- EVM 不再是瓶颈，瓶颈转移到 state I/O

**改动文件**：
| 文件 | 改动 |
|------|------|
| `Cargo.toml` (n42-node) | 添加 pevm 依赖 |
| `payload.rs` | 并行执行路径 |
| `evm_config.rs` | 可能需要适配 PEVM 的接口 |

---

### Phase 4：全内存热状态 + 异步 Merkle 化（长期，10-15 天）

**目标**：30000 → 50000+ TPS

**核心思想**：借鉴 MegaETH SALT + Sei SeiDB，将热状态常驻内存，Merkle 化异步进行。

**设计**：

1. **Hot State Cache**
```rust
struct HotStateCache {
    accounts: DashMap<Address, AccountState>,  // 并发安全
    storage: DashMap<(Address, U256), U256>,
    // N42 初期状态量 < 10GB，IDC 节点有 64-128GB RAM
}

impl StateProvider for HotStateCache {
    fn account(&self, addr: &Address) -> Option<AccountState> {
        self.accounts.get(addr).map(|v| v.clone())
    }
    fn storage(&self, addr: &Address, slot: &U256) -> Option<U256> {
        self.storage.get(&(*addr, *slot)).map(|v| *v)
    }
}
```

2. **State/Commitment 分离**
- EVM 执行只更新 HotStateCache（内存 KV）
- 后台线程异步更新 Merkle Patricia Trie
- 完全消除 state I/O 对 EVM 的阻塞

3. **增量 Merkle 更新**
- 只重新哈希被修改的 trie 路径
- 参考 Gravity Reth 的 16 路并行 Merkle 化

**预期效果**：
- State read: 内存 HashMap O(1) vs MPT traversal O(log n)
- State write: 内存更新即返回，无磁盘 I/O
- Merkle 化完全异步，不阻塞出块

---

## 四、优先级矩阵与路线图

| Phase | 优化项 | TPS 提升 | 工期 | ROI | 风险 |
|-------|--------|---------|------|-----|------|
| **0** | 块大小限制 | 稳定性 | < 1 天 | ★★★★★ | 极低 |
| **1** | Leader 直推 | 2000→8000 | 3-5 天 | ★★★★★ | 低 |
| **2** | Delayed State Root | 8000→15000 | 5-7 天 | ★★★★ | 中 |
| **3** | 并行 EVM | 15000→30000 | 5-10 天 | ★★★ | 中 |
| **4** | 全内存状态 | 30000→50000+ | 10-15 天 | ★★★ | 高 |

**推荐路线**：
- **Phase 0 + 1**：立即实施，解决最紧迫的死亡螺旋和块传播瓶颈
- **Phase 2**：紧跟其后，是解锁短 slot time（<1s）的前提
- **Phase 3**：Phase 1+2 完成后评估是否仍需要
- **Phase 4**：长期方向，需与 reth 上游生态同步

---

## 五、关键技术决策记录

### 为什么选择 Direct Push 而非 RaptorCast？

| 方案 | 延迟 | 实现复杂度 | 适用场景 |
|------|------|-----------|---------|
| **Direct Push** | 1 跳，最低 | 简单（TCP/QUIC） | **7 节点 IDC** |
| RaptorCast | 2 跳 | 高（纠删码+广播树） | 100+ 验证者 |
| Compact Block | 取决于 mempool 同步率 | 中 | 公链大量节点 |

N42 仅有 7 个 IDC 节点，Direct Push 的带宽成本完全可控：
- 3.5MB × 6 followers = 21MB per block
- 以 1Gbps 网络计，传输时间 ≈ 170ms
- 以 10Gbps IDC 网络，传输时间 ≈ 17ms

RaptorCast 的纠删码优势在节点数多时才显现（网络带宽分摊），7 节点场景下复杂度不值得。

### 为什么选择 PEVM 而非 Grevm？

| 方案 | 与 reth 兼容性 | 简单转账加速 | 复杂合约加速 | 成熟度 |
|------|---------------|-------------|-------------|--------|
| **PEVM** | 原生兼容 | 1.73-2x | 4-5x | 生产级 |
| Grevm 2.1 | 需适配 | 更高（DAG调度） | 11x (gigagas) | 实验性 |
| 自研 Block-STM | 完全可控 | 可定制 | 可定制 | 需验证 |

PEVM 与 reth 的 revm 生态一致，集成成本最低。N42 初期以简单转账为主，PEVM 2x 加速已足够。当合约场景增多后再考虑 Grevm。

### 为什么 Delayed State Root 放在 Phase 2 而非 Phase 1？

1. Phase 1（Direct Push）的 ROI 更高：直接解决 700x 延迟的根因
2. Delayed State Root 需要修改 chainspec 和 block validation 规则，影响面更大
3. Phase 1 完成后，block 传播延迟从 25s 降至 ~100ms，此时 state root 的 200ms 才成为新瓶颈
4. 先做简单的，再做复杂的

---

## 六、验证方案

每个 Phase 完成后：

1. **编译验证**：`cargo check && cargo clippy`
2. **单元测试**：`cargo test -p n42-network -p n42-node`
3. **7 节点压测**：`n42-stress --step --prefill 20000`
4. **记录指标**：
   - `n42_consensus_commit_latency_ms`（目标：Phase 1 后 < 500ms）
   - `n42_payload_build_ms`（目标：Phase 3 后 < 50ms）
   - `n42_pipeline_total_ms`（目标：< slot_time）
   - 实际 TPS、失败率、gas 利用率

5. **对比基线**（日志-27 数据）：

| 指标 | 基线 (日志-27) | Phase 0 目标 | Phase 1 目标 | Phase 2 目标 |
|------|--------------|------------|------------|------------|
| 持续 TPS | 2000 | 2000 (稳定) | 8000 | 15000 |
| Commit p50 | 2650ms | 500ms | 100ms | 50ms |
| Import p50 | 441ms | 300ms | 300ms | 50ms |
| 最大块 txs | 23,809 | 5,000 | 10,000 | 15,000 |

---

## 七、参考资料

- [MonadBFT + RaptorCast](https://docs.monad.xyz/monad-arch/consensus/raptorcast) — 纠删码块传播
- [MegaETH SALT](https://www.megaeth.com/blog-news/endgame-how-salt-breaks-the-bottleneck-thats-been-strangling-blockchains) — 内存认证结构
- [Sei Giga Whitepaper](https://arxiv.org/abs/2505.14914) — 异步状态承诺
- [EIP-7862: Delayed State Root](https://eips.ethereum.org/EIPS/eip-7862)
- [PEVM](https://github.com/risechain/pevm) — Block-STM 并行 EVM
- [Ress: Stateless Reth](https://www.paradigm.xyz/2025/03/stateless-reth-nodes) — 无状态节点
- [Revmc](https://www.paradigm.xyz/2024/06/revmc) — EVM JIT 编译
- [GossipSub 大消息优化](https://arxiv.org/html/2505.17337v1) — PREAMBLE + IMRECEIVING
- [Gravity Reth 16路并行](https://github.com/Galxe/gravity-reth) — 并行 Merkle 化
