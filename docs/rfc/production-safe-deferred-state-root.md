# RFC — Production-Safe Deferred State Root

> Status: Draft · Date: 2026-05-13 · Supersedes/extends: devlog-32, devlog-53

## Motivation

State root computation is not actually on N42's commit-critical path today: `devlog-32` 证明 Compact Block cache hit 时 `root_ms = 0`，因为 reth 在已缓存的 BundleState 上做 Synchronous SR 几乎免费。然而项目已经为更激进的"跳过/延后" SR 提供了两个 opt-in：

- `N42_SKIP_STATE_ROOT=1`：reth 端完全跳过 SR 计算（最大化吞吐，但不安全）
- `N42_DEFER_STATE_ROOT=1`：leader 把 header `state_root = B256::ZERO`，reth 跳过验证（同样不安全）

两者目前都在 `bin/n42-node/src/main.rs:311-318` 显式打 `BENCHMARK MODE — NOT FOR PRODUCTION` 警告。`consensus_loop.rs:626-629` 的"未来：spawn 异步 SR"注释指明这是未完成工作。

本 RFC 提出**如何把 deferred state root 变成 production-safe** 的路线，重点是与已有 ZK sidecar（`devlog-53`, `n42-zkproof`）协同，避免 `devlog-32` v1/v2 失败时踩过的 sparse trie 状态损坏陷阱。

## Non-goals

- 不修改 HotStuff-2 投票内容（QC 内容保持 `(parent_hash, tx_root, view)`，本来就不签 state_root）
- 不强制改 EIP-4895 / withdrawals 或现有 EIP-4844 blob 行为
- 不削弱 manual sync recovery（snapshot + reth pipeline sync 仍需可工作）

## Background — devlog-32 的失败教训

v1 "后台 compute_state_root_serial"：reth 的 anchored sparse trie 依赖 TrieUpdates 增量推进；传 `TrieUpdates::default()` 会让 trie 进入未定义状态，后续没有 compact block cache 的块计算出错误 root → 永久卡链。

v2 "主线程预创建 provider"：性能反而下降，2000 TPS 即卡死。

**核心教训**：state root 不是独立的"事后可补"计算，它绑定 reth 的内部 sparse trie 状态机。任何 deferred 方案必须在 reth 的 trie state 演进**之外**生成 root（要么完整重算，要么 ZK 见证），不能"伪造一个空更新"。

## Proposed Design

分两阶段。

### Phase A — `N42_DEFER_STATE_ROOT` 守门 + 告警 hardening（短期，2-3 天）

把现有 benchmark mode 加上 hard guards，防止误用进入生产：

- **启动拒绝**：`bin/n42-node/src/main.rs:311-319` 检测到 `N42_DEFER_STATE_ROOT=1` **AND** 缺失 `N42_ALLOW_BENCH_MODE=1` 时直接 `std::process::exit(1)`，而不是 `warn!`。
- **链 ID 限制**：仅当 chain_id 落在保留的 benchmark 区间（例如 `chain_id >= 4242400 && chain_id < 4242500`）时允许 deferred SR；否则 panic。
- **Metric 暴露**：暴露 `n42_deferred_state_root_enabled` gauge（0/1），dashboard alert 上线。

> 关键：这一阶段不解决安全性，只确保生产部署绝不会启用。这是为后续 Phase B 留出安全的实验空间。

### Phase B — ZK sidecar 作为 production state root oracle（中期，2-4 周）

复用 `n42-zkproof` 的 SP1 sidecar 作为 state root 的异步生成器：

1. **数据流**
   - Leader 出块时 `state_root = B256::ZERO`（与现有 deferred 行为一致）
   - ZK sidecar (`consensus_loop.rs:469` 已挂入)以更高频率（每块而非每 `proof_interval`）生成 proof
   - Sidecar 输出含 `(block_hash, state_root, proof_bytes)` 写入新的 `AttestationStore` 表

2. **共识层**
   - **不**修改 QC 签名内容
   - 在共识 commit 之后，引入"finality lag" 概念：只有 `block_n - committed_state_root_height >= K`（建议 K=2）时才向外宣称 `n` finalized
   - RPC `n42_finality` 返回 `(last_committed, last_proven_state_root_height)`

3. **手机轻客户端**
   - 现有 `n42-mobile` 已经按 epoch / withdrawals 验证；改为额外校验 sidecar 输出的 ZK proof（依赖 SP1 verifier 在手机端的可用性）
   - 短期可接受 "trust full node within K blocks" 的弱安全性，注明在 Mobile SDK 文档

4. **Reorg 处理**
   - ZK sidecar 失败/超时（默认 30s）→ 节点回退到 inline SR 模式（在 `executor.rs` 触发常规 reth SR 计算路径）
   - 通过 `n42_deferred_state_root_fallback_total` counter 监控

5. **失效控制 (feature flag)**
   - `enable_zk_deferred_state_root: bool` 在 `chainspec` 中，硬分叉 height 后启用
   - 紧急时可在分叉前关闭

### 不接受的方向

- **fraud proof 窗口**：8s slot + 移动验证模型下挑战窗口过短，难以可靠收集 fraud proof
- **重新走 devlog-32 v1/v2 路径**：sparse trie 状态损坏已知是 dead end

## 实施前置依赖

- **Phase A**：无外部依赖，直接改 `bin/n42-node/src/main.rs` 启动检查
- **Phase B**：
  - 先稳定 SP1 prover 在生产 8s slot 内 p99 < 30s（当前 `devlog-53` 数据 < 此值，但需复测）
  - JMT 已接入 (`devlog-52`)，提供 sidecar 所需的 BundleState diff
  - `n42-zkproof::scheduler::on_block_committed` 改为每块触发（当前默认 300 块）

## 影响面

- `bin/n42-node/src/main.rs:311-319` — 启动 guard
- `crates/n42-node/src/orchestrator/consensus_loop.rs:469` — sidecar 调度频率
- `crates/n42-node/src/orchestrator/consensus_loop.rs:626-629` — 把 debug log 改为 metric + state 更新
- `crates/n42-zkproof/src/scheduler.rs` — proof interval 配置
- `crates/n42-node/src/attestation_store.rs`（已存在）— 写入 (block_hash, state_root, proof) 元组
- `crates/n42-mobile/...` — SDK 加 sidecar proof 校验路径
- `crates/n42-chainspec/...` — feature flag

## 风险与回滚

- ZK prover 性能回归 → sidecar 来不及生成 → fallback 到 inline SR；fallback 计数 > 阈值时告警，运维可直接关 feature flag
- Sparse trie 状态损坏（devlog-32 v1/v2 教训）— 本设计**不**触碰 reth 内部 sparse trie，仅在 reth 之外生成 root，绕开此陷阱
- 移动端 ZK verifier 性能不达标 → 退回到 "trust within K blocks" 模式，仅影响安全模型，不影响节点运行

## 成功标准

- Phase A 上线后：所有非 benchmark chain_id 启动都拒绝 `N42_DEFER_STATE_ROOT`；CI 加 negative-test 验证拒绝路径
- Phase B 上线后：testnet 30 天连续运行无回退到 fallback；`n42_finality.last_proven_state_root_height` 始终 ≥ `last_committed - K`
- commit p50 在 Phase B 启用后**不退化**（因为 SR 本来就不在 critical path 上）

## 验证步骤

```powershell
# Phase A — 单元测试
cargo test -p n42-node-bin -- --test main_guard_rejects_defer_state_root

# Phase A — 拒绝路径
$env:N42_DEFER_STATE_ROOT="1"
$env:N42_CHAIN_ID="1"  # not benchmark range
cargo run --release -p n42-node-bin  # should exit 1

# Phase B — 联合 ZK sidecar
$env:N42_ZK_PROOF="1"
$env:N42_DEFER_STATE_ROOT="1"
$env:N42_ALLOW_BENCH_MODE="1"
./scripts/testnet.sh --nodes 7 --clean
# 观察 metric
# - n42_zk_proof_generation_ms p99 < 30s
# - n42_deferred_state_root_fallback_total == 0
# - rpc n42_finality.last_proven_state_root_height advances
```

## 开放问题

- ZK sidecar 在 SP1 zkVM 之外的备选 prover（Risc0, Boojum）是否值得评估？
- 是否需要 `state_root` 携带在后续块 N+K 的 header 而非独立 attestation 表？（Header 改动会触动 RLP 兼容性，本 RFC 倾向**不**改 header）
- Mobile SDK 的 SP1 verifier 是否能在低端设备上 < 1s 验证？（依赖 devlog-53 后续数据）
