# N42-26 Development Log

> Project: HotStuff-2 Consensus + reth Execution Layer Custom Blockchain
> Repository: n42-26
> Start Date: 2026-02-15

---

## Log Index

Logs are split into separate files by phase for easy maintenance:

| File | Content | Phase |
|------|---------|-------|
| [devlog-01-foundation](docs/devlog-01-foundation.md) | Phase 1-6: Execution, Consensus, Network, Mobile Verify, Integration | Foundation |
| [devlog-02-integration-test](docs/devlog-02-integration-test.md) | Phase 7-15: E2E Integration, Testing, Feature Completion | Integration |
| [devlog-03-audit-hardening](docs/devlog-03-audit-hardening.md) | Code Audit, Test Coverage, Production Hardening | Production |
| [devlog-04-research-notes](docs/devlog-04-research-notes.md) | Consensus Research: Leader Election, Attack Resistance, Recovery | Research |
| [devlog-05-network-arch-research](docs/devlog-05-network-arch-research.md) | IDC-Mobile Network Architecture: Bottleneck Analysis, Firedance | Research |
| [devlog-06-network-quick-optimize](docs/devlog-06-network-quick-optimize.md) | Phase 1: Zero-copy, Send Timeout, zstd Compression, CacheSync | Optimization |
| [devlog-07-network-arch-improvement](docs/devlog-07-network-arch-improvement.md) | Phase 2: Pre-framing, Connection Tiers, Multi-Endpoint Sharding | Architecture |
| [devlog-08-network-smart-optimize](docs/devlog-08-network-smart-optimize.md) | Phase 3: EWMA RTT, Arc Lock-free, Tiered Broadcast | Optimization |
| [devlog-09-stream-format-refactor](docs/devlog-09-stream-format-refactor.md) | Versioned Sequential Stream Format: StreamPacket, ReadLogDB | Protocol |
| [devlog-10-blob-tx-support](docs/devlog-10-blob-tx-support.md) | EIP-4844 Blob Transaction Support: EL Enable, GossipSub Sidecar | Ecosystem |
| [devlog-11-7node-testnet](docs/devlog-11-7node-testnet.md) | 7-Node HotStuff-2 Testnet: One-click Launch, Blockscout | Testnet |
| [devlog-12-code-simplification](docs/devlog-12-code-simplification.md) | Project-wide Code Simplification: -3860 lines | Quality |
| [devlog-13-mobile-verify-reward](docs/devlog-13-mobile-verify-reward.md) | Mobile Verify Simulator, EIP-4895 Reward Distribution | Feature |
| [devlog-14-code-simplification-extra](docs/devlog-14-code-simplification-extra.md) | Post-Mobile Verify Code Simplification: -254 lines | Quality |
| [devlog-15-production-readiness](docs/devlog-15-production-readiness.md) | P0/P1/P2 Production Readiness: Dynamic Validators, iOS FFI, RPC Auth | Production |
| [devlog-16-full-module-maturity-audit](docs/devlog-16-full-module-maturity-audit.md) | 8-Crate Deep Audit: 13 P0 + 25 P1 + 33 P2 Findings, 442 Tests | Audit |
| [devlog-17-unified-testnet](docs/devlog-17-unified-testnet.md) | Unified Testnet Setup | Testnet |
| [devlog-17b-mining-plugin-ui](docs/devlog-17b-mining-plugin-ui.md) | n42_mining Flutter Package: Embeddable Mining UI, FFI, Riverpod | Mobile UI |
| [devlog-18-tps-deep-optimize](docs/devlog-18-tps-deep-optimize.md) | TPS 680→1000+: Genesis GasLimit, GossipSub 8MB, v6 Stress Tool | Performance |
| [devlog-19-tps-optimize-phase1](docs/devlog-19-tps-optimize-phase1.md) | TPS Optimization Phase 1 | Performance |
| [devlog-20-tps-optimize-phase2](docs/devlog-20-tps-optimize-phase2.md) | TPS Optimization Phase 2 | Performance |
| [devlog-21-fast-block-tps-breakthrough](docs/devlog-21-fast-block-tps-breakthrough.md) | Fast Block Production & TPS Breakthrough | Performance |
| [devlog-22-consensus-exec-pipeline](docs/devlog-22-consensus-exec-pipeline.md) | Consensus-Execution Pipelining | Architecture |
| [devlog-23-parallel-evm-research](docs/devlog-23-parallel-evm-research.md) | Parallel EVM Research & Implementation | Research |
| [devlog-24-pipeline-optimize-diagnose](docs/devlog-24-pipeline-optimize-diagnose.md) | Pipeline Optimization & Diagnostics | Performance |
| [devlog-25-chain-startup-stability](docs/devlog-25-chain-startup-stability.md) | Chain Startup Stability & Stress Testing | Stability |
| [devlog-26-high-load-stall-fix-baseline](docs/devlog-26-high-load-stall-fix-baseline.md) | High-Load Chain Stall Fix & TPS Baseline | Bug Fix |
| [devlog-27-evm-skip-bench-bottleneck](docs/devlog-27-evm-skip-bench-bottleneck.md) | EVM Skip Benchmark & Bottleneck Location | Analysis |
| [devlog-28-perf-optimize-design](docs/devlog-28-perf-optimize-design.md) | Performance Optimization Design | Planning |
| [devlog-29-tx-gossip-bottleneck-fix](docs/devlog-29-tx-gossip-bottleneck-fix.md) | TX Gossip Bottleneck Diagnosis & Fix | Bug Fix |
| [devlog-29-high-load-stall-cure-direct-push](docs/devlog-29-high-load-stall-cure-direct-push.md) | High-Load Stall Root Cause & Direct Push | Bug Fix |
| [devlog-29b-tx-forward-to-leader](docs/devlog-29b-tx-forward-to-leader.md) | TX Forward to Leader Implementation | Feature |
| [devlog-30-tps-bottleneck-analysis-roadmap](docs/devlog-30-tps-bottleneck-analysis-roadmap.md) | TPS Bottleneck Deep Analysis & Optimization Roadmap | Analysis |
| [devlog-31-full-pipeline-diagnose-100k-roadmap](docs/devlog-31-full-pipeline-diagnose-100k-roadmap.md) | Full Pipeline Diagnosis & 100K TPS Roadmap | Analysis |
| [devlog-32-compact-block-delayed-sr](docs/devlog-32-compact-block-delayed-sr.md) | Compact Block & Delayed State Root Experiment | Feature |
| [devlog-33-stress-v8-gas-limit](docs/devlog-33-stress-v8-gas-limit.md) | Stress Tool v8 & Gas Limit Breakthrough | Performance |
| [devlog-34-2g-gas-limit-pool-overflow](docs/devlog-34-2g-gas-limit-pool-overflow.md) | 2G Gas Limit & Pool Overflow Fix | Bug Fix |
| [devlog-35-pipeline-profiling-bottleneck](docs/devlog-35-pipeline-profiling-bottleneck.md) | Pipeline Profiling & Bottleneck Location | Analysis |
| [devlog-36-stress-v9-block-size-mgmt](docs/devlog-36-stress-v9-block-size-mgmt.md) | Stress Tool v9 & Block Size Management | Performance |
| [devlog-37-43k-tps-plan](docs/devlog-37-43k-tps-plan.md) | 43K TPS Breakthrough Plan | Planning |
| [devlog-38-hot-state-zerocopy-timeout-fix](docs/devlog-38-hot-state-zerocopy-timeout-fix.md) | Hot State Zero-copy & Timeout Bug Fix | Bug Fix |
| [devlog-39-timeout-fix-stress-tps](docs/devlog-39-timeout-fix-stress-tps.md) | Timeout Fix Stress Verification & TPS | Performance |
| [devlog-40-pipeline-timing-analysis](docs/devlog-40-pipeline-timing-analysis.md) | Pipeline Timing Analysis & Optimization | Analysis |
| [devlog-41-fast-propose-tps](docs/devlog-41-fast-propose-tps.md) | Fast Propose & TPS Breakthrough | Performance |
| [devlog-42-gossipsub-fallback-stress](docs/devlog-42-gossipsub-fallback-stress.md) | GossipSub Fallback & Stress Breakthrough | Performance |
| [devlog-42b-regression-fix-timing-audit](docs/devlog-42b-regression-fix-timing-audit.md) | Regression Fix & Timing Audit | Bug Fix |
| [devlog-43-deep-timing-analysis-tps](docs/devlog-43-deep-timing-analysis-tps.md) | Deep Timing Analysis & TPS Optimization | Analysis |
| [devlog-43b-detailed-timing-tps-eval](docs/devlog-43b-detailed-timing-tps-eval.md) | Detailed Timing Analysis & TPS Evaluation | Analysis |
| [devlog-44-24k-block-12k-tps-verify](docs/devlog-44-24k-block-12k-tps-verify.md) | 24K Block Stable 12K TPS Verification | Performance |
| [devlog-44b-concurrent-timing-txpool-optimize](docs/devlog-44b-concurrent-timing-txpool-optimize.md) | Concurrent Timing & TX Pool Optimization | Performance |
| [devlog-45-pool-ordmap-packing](docs/devlog-45-pool-ordmap-packing.md) | Pool OrdMap & Packing Optimization | Performance |
| [devlog-46-optimistic-voting-r1](docs/devlog-46-optimistic-voting-r1.md) | Optimistic Voting: R1 vote_delay 363ms→0ms | Performance |
| [devlog-47-channel-split-runtime](docs/devlog-47-channel-split-runtime.md) | Channel Split + Independent Runtime: R2 -40% | Performance |
| [devlog-48-28k-cap-14k-tps-timing](docs/devlog-48-28k-cap-14k-tps-timing.md) | 28K Cap Full Optimization Timing Baseline: 13.4K TPS | Analysis |
| [devlog-49-lan-max-tps-cap-sweep](docs/devlog-49-lan-max-tps-cap-sweep.md) | LAN Max TPS: Cap Sweep, 48K+FP = **39K TPS** | Performance |
| [devlog-50-cachehit-fastpath-90k-tps](docs/devlog-50-cachehit-fastpath-90k-tps.md) | Cache Hit Fast Path: **90,949 TPS** | Performance |
| [devlog-51-jmt-blake3-integration](docs/devlog-51-jmt-blake3-integration.md) | JMT + Blake3: 16-shard Parallel, Prometheus, Mobile Proofs | Architecture |
| [devlog-52-jmt-full-integration](docs/devlog-52-jmt-full-integration.md) | JMT Full Integration: Orchestrator, RPC, Snapshot, Dart SDK | Architecture |
| [devlog-53-zk-sidecar-proof-system](docs/devlog-53-zk-sidecar-proof-system.md) | ZK Sidecar Proof System: SP1 zkVM Backend | Architecture |
| [devlog-54-dynamic-validator-set](docs/devlog-54-dynamic-validator-set.md) | Commit-then-Activate: Dynamic Validator Set Change Protocol | Feature |
| [devlog-55-audit-reth-upgrade-tier1](docs/devlog-55-audit-reth-upgrade-tier1.md) | Go Audit Fix + reth Upgrade + Rotor/Precompile/DA/Auth | Feature |

### Standalone Documents

| File | Content |
|------|---------|
| [90K-cap-timing-analysis](docs/90K-cap-timing-analysis.md) | 90K Cap Timing Analysis |
| [stress-test-58k](docs/stress-test-58k.md) | 58K Cap Stress Test Guide |
| [low-end-device-test](docs/low-end-device-test.md) | Low-End Device Testing |
| [starknet-poseidon-vs-aptos-jmt-comparison](docs/starknet-poseidon-vs-aptos-jmt-comparison.md) | Starknet Poseidon vs Aptos JMT Comparison |

---

| [devlog-56-consensus-evidence-persistence](docs/devlog-56-consensus-evidence-persistence.md) | Consensus Evidence Persistence, Validator Change Protocol, Prague Upgrade |
| [devlog-58-jmt-vs-qmdb-gap-p0](docs/devlog-58-jmt-vs-qmdb-gap-p0.md) | JMT vs QMDB/NOMT Gap Analysis + P0: In-Memory Tree + Background Snapshot, Disk 33–48× Slower | Architecture |
| [devlog-59-jmt-to-bmt-sbmt-phase1](docs/devlog-59-jmt-to-bmt-sbmt-phase1.md) | Decision JMT→Self-built SBMT, Persistent SBMT WAL/fsync, Recovery E2E, EOA code_hash alignment | Architecture |
| [devlog-60-sbmt-e2e-verification](docs/devlog-60-sbmt-e2e-verification.md) | SBMT RPC Proof + Restart Recovery E2E: Genesis Snapshot, Inclusion/Exclusion Proofs, Mobile Verify | Verification |
| [devlog-61-sbmt-proof-key-binding](docs/devlog-61-sbmt-proof-key-binding.md) | SBMT 证明 key+shard 绑定安全修复（轻客户端 soundness）+ key 派生/shard_index 单一来源去重 | Security |
| [devlog-62-sbmt-benchmark-vs-gov5](docs/devlog-62-sbmt-benchmark-vs-gov5.md) | SBMT 规模化 benchmark + in-place insert 优化（1.8×）+ 对标 gov5 BMT/QMDB；识别路径压缩机会 | Benchmark |
| [devlog-63-twig-memory-core-design](docs/devlog-63-twig-memory-core-design.md) | 决策：走 AlDBaran 全 DRAM 路线（非 QMDB SSD 路线）；内存即吞吐；Rust 自建 twig 核心 | Design |
| [devlog-64-rust-twig-engine-spec](docs/devlog-64-rust-twig-engine-spec.md) | Rust 全-DRAM twig engine 实现 spec + P1–P6 实现（对 gov5 字节验证、#11 绑定、StateDiff 桥接） | Design |
| [devlog-65-twig-real-data-profiling](docs/devlog-65-twig-real-data-profiling.md) | twig 引擎真实主网账户剖析（reth2k）+ value-arena 优化（−8% RSS，280 B/acct，root 不变） | Profiling |
| [devlog-68-twig-node-integration](docs/devlog-68-twig-node-integration.md) | Twig P6 持久化、mobile/FFI 验证、节点接线、mac 4-node E2E + WAL 恢复 | Architecture |
| [devlog-69-concurrency-audit](docs/devlog-69-concurrency-audit.md) | 跨 crate 并发/正确性审计：consensus 状态机+twig 边界+orchestrator 判定健全；修复 network set_validator_context 静默丢弃 + execution destroyed-then-recreated 账户误删（EIP-6780 DestroyedChanged） | Audit |
| [devlog-70-p6-node-e2e-closeout](docs/devlog-70-p6-node-e2e-closeout.md) | P6 节点 E2E 收尾：fresh 4-node Twig root 一致、WAL crash recovery、leader drain finalization；真实 8s slot profiling 另见 devlog-71 | Verification |
| [devlog-71-real-slot-profile](docs/devlog-71-real-slot-profile.md) | macOS 真实 4-node slot profiling pilot：transfer/contract-heavy + mobile sim + Twig，记录 critical-path 和采样限制 | Profiling |

### 高 TPS 调查（devlog-80→87，已收尾）

合成压力工况（7 节点、2s 出块、90k tx/块、skip-verify、deferred state-root）下定位高 TPS 墙钟尾的真因。**结论**：墙钟尾不是 EVM、不是卡死的 `building_on_parent` guard，而是 90k 压力下 reth(EL) 落后共识 **2-6 块**（head_lag p50=4/p95=6），leader 在 EL 还没追上的 head 上构建 → FCU 返回 `Syncing/no-payload` → 不广播 → 10s 超时级联。**leader 单边 backpressure 已被 A/B 数据否决**（验证者独立超时，治标更差）；真正的修复需网络可见的 view-extension 或抬高 EL 吞吐天花板。合成数 64-90k 块 TPS / 9k 持续已远超 8s-slot 生产目标，调查归档收手。

| 文件 | 内容 | 类型 |
|------|------|------|
| [devlog-80-interblock-cadence](docs/devlog-80-interblock-cadence.md) | inter-block cadence 探针：30s 尾是 harness pool-drain 假象，leader build 1.9-2.6s | Profiling |
| [devlog-81-batch-transfer-fastlane-bench](docs/devlog-81-batch-transfer-fastlane-bench.md) | pool-depth 诊断 + batch-transfer 快车道（CPU-only：批量 ecrecover 1e4-5e4× 降，48-64M transfers/s，带宽受限）| Benchmark |
| [devlog-82-continuous-cadence](docs/devlog-82-continuous-cadence.md) | per-node-continuous 重测：30s 假象消失（inter-block p95 31.7s→12.4s），暴露满池超时 | Profiling |
| [devlog-83-timeout-view](docs/devlog-83-timeout-view.md) | 满池超时=leader 侧不广播视图（81.8%）；smoking gun：reth worker 线程 tracing span panic | Diagnosis |
| [devlog-84-fix-payload-span-panic](docs/devlog-84-fix-payload-span-panic.md) | span panic 修复（n42 `Span::none()` + reth `parent: &parent_span`，0 panic）；但非 TPS 收益 | Bug Fix |
| [devlog-85-fcu-nopayload-noleaderbuild](docs/devlog-85-fcu-nopayload-noleaderbuild.md) | 真因二分：A=FCU 返回 Syncing/no-payload；B=leader 在 bg import 在途换届被迫 defer；均=EL/import 跟不上节奏 | Diagnosis |
| [devlog-86-el-headlag](docs/devlog-86-el-headlag.md) | EL head-lag 钩子量化：Syncing/no-payload 时 head_lag 稳定 2-6 块（真·多块滞后，非 1 块竞态）| Diagnosis |
| [devlog-87-el-backpressure-scheduler](docs/devlog-87-el-backpressure-scheduler.md) | EL-backpressure 调度器（flag 门控）A/B：no-gain，leader 单边延迟治标更差，默认 OFF，不并入 | Benchmark |

### Caplin EL-seam 重构（stage 3-6）

| 文件 | 内容 | 类型 |
|------|------|------|
| [devlog-88-caplin-cl-seam-stage3-6](docs/devlog-88-caplin-cl-seam-stage3-6.md) | 把共识层重构成 Caplin 式 ports-and-adapters：sink/network/EL/blob/exec-cache 全走 port trait，抽出 `n42-consensus-service` crate（硬-reth-free，允许 revm/Receipt）；行为字节级等同，212 单测+6 集成绿；E2E 待 datc 让机 | Architecture |
| [devlog-89-caplin-cl-seam-stage7-9](docs/devlog-89-caplin-cl-seam-stage7-9.md) | stage 7 observer 折叠到 ports 并入 crate；stage 8 async finalize-FCU 本已实现（flag-gated），A/B 留真机；stage 9 `EngineApiRpcExecutionLayer`（Engine-API JSON-RPC 客户端实现 ExecutionLayer）+ standalone 共识二进制（双模），含 JWT provider-feature 统一冲突的 hmac 修复 | Architecture |

---

| [devlog-97-locked-qc-build-parent](docs/devlog-97-locked-qc-build-parent.md) | gov5 S3 audit: LockedQC-authoritative leader builds, async view/parent binding, fail-closed reth defer | Security |

New entries: append to the corresponding category file, or create a new numbered file (e.g., `devlog-57-xxx.md`) in `docs/`.
