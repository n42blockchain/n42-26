# 评估：redb vs MDBX —— 替代还是备用（2026-07-21）

> 结论导向的存储后端评估，落地到 n42-26 的实际存储用法，而非通用对比。
> 代码依据：本仓 `crates/`、`bin/`，reth path 依赖 `../reth`（reth 2.4.1 @ `c533db8ba`）。

## TL;DR

- **不要用 redb 替换 MDBX 作为主存储。** 执行层（reth）与 MDBX 深度耦合、schema 依赖 MDBX 的 **DupSort**（redb 无等价物）、
  且靠 mmap 零拷贝读吃 8s slot 预算；替换既踩 CLAUDE.md 红线（不得偏离 reth 基线、丢上游跟踪），性能上也是纯倒退。
- **"备用/热切换 MDBX↔redb" 的框架不成立**——没有这样的真实场景。
- **redb 在本项目唯一有真实价值的落点**：把 n42 **自有的**、目前仍偷偷依赖 `reth-libmdbx`（C 库）的小存储
  （首选 `EvidenceStore`）换成纯 Rust，让"硬-reth-free"的 `n42-consensus-standalone` **真正**摆脱 C 依赖。
  这是"纯 Rust 可移植性"取舍，低优先、低风险、可选。

## 一、n42-26 里 MDBX 到底用在哪（这决定结论）

| 存储点 | 后端 | 是否 MDBX | 代码位置 | 能否/该否换 redb |
|---|---|---|---|---|
| reth 执行层状态（账户/存储/changeset/trie/receipts） | reth MDBX | 是（硬依赖） | `../reth` + workspace `reth-libmdbx`（Cargo.toml:87） | **否**——reth schema 用 **DupSort**（如 storage changeset、hashed storage），redb 无等价，换=重写 reth 存储层 |
| Twig 生产状态承诺持久化（`PersistentTwig`） | **WAL 文件 append + `sync_data` + snapshot** | 否 | `crates/n42-jmt/src/persistent.rs`（`wal: File`、`OpenOptions`、`sync_data`） | 不适用——全-DRAM+追加 WAL 设计（devlog-63），塞 COW B-tree DB 反而写放大、违背"内存即吞吐"初衷 |
| `EvidenceStore`（共识证据/手机证据/注册表） | `reth_libmdbx` | 是 | `crates/n42-jmt/src/evidence_store.rs:28` | **可以，且是 redb 唯一合理落点**（低流量、非热路径） |
| JMT/SBMT `disk_store`（legacy，`N42_JMT` 非默认） | `reth_libmdbx` | 是 | `crates/n42-jmt/src/disk_store.rs:15` | 可以，但 JMT 已弃用默认（Twig 为默认后端，`bin/n42-node/src/main.rs:655-666`），不值当 |
| 共识状态快照（v5）/ vote log | bincode 文件 | 否 | `crates/n42-consensus-service/src/consensus_state.rs`、`persistence.rs` | 不适用 |
| 坏块缓存 / view 计数等 | 内存（进程内，不持久化） | 否 | orchestrator | 不适用 |

**关键发现**：`n42-consensus-service`（号称 hard-reth-free）→ 依赖 `n42-jmt` → `EvidenceStore`/`disk_store` 用 `reth_libmdbx`。
**因此那个 reth-free 的独立共识二进制（`bin/n42-consensus-standalone`），其持久化其实仍挂着 reth 的 C 库 MDBX。** 这是 redb 价值的落点。

## 二、redb vs MDBX 技术优劣（针对本项目 profile）

### MDBX（libmdbx）
- **优**：reth/Erigon 数百 GB 实战验证；**mmap 零拷贝读**（读密集最快，直接受益 8s slot 的状态访问）；
  DupSort 等富特性（reth 在用）；COW meta 页、无 WAL，崩溃安全成熟。
- **劣**：C 库（构建需 C 工具链/bindgen，FFI unsafe 面）；mmap → 需预设 `Geometry`、有 `MAP_FULL` 预分配风险；
  **Windows 上历史坑多**（gov5 侧踩过 `WriteMap` OOM——见根 CLAUDE.md/gov5 记录）。

### redb
- **优**：**纯 Rust**（无 C 工具链、内存安全、交叉编译/Windows 干净、无 mmap-`WriteMap` 那类坑）；
  API 稳定（1.x/2.x）、savepoint、嵌入式友好、单文件。
- **劣**：比 MDBX 年轻、数百 GB reth 级规模未经充分验证；读密集下普遍**慢于** MDBX 的 mmap 零拷贝；
  **无 DupSort**（直接判死"替换 reth EL"）；COW B-tree 写放大；同为单写多读（不是并发写优势）。

## 三、逐场景建议

1. **执行层（reth EL）：维持 MDBX，不替不备。**
   DupSort 依赖 + mmap 读性能 + reth 耦合 + 红线，四条任一都足以否决。redb 在这里是纯倒退。

2. **Twig 生产持久化：维持 WAL+snapshot，不引入任何 KV DB。**
   全-DRAM 吞吐设计的核心，redb/MDBX 都不该进这条热路径。

3. **redb 值得考虑的唯一场景（可选、低优先、低风险）——去 C 依赖化 n42 自有小存储：**
   若"reth-free 独立共识节点"是明确目标，把 `EvidenceStore`（必要时加 legacy JMT `disk_store`）
   从 `reth_libmdbx` 迁到 redb，让 `n42-consensus-standalone` 真正纯 Rust、无 C 依赖、Windows/交叉编译零摩擦。
   - 这些是低流量存储（证据/注册表，非 EL 热路径），性能不敏感 → redb 的读性能与规模短板都不构成问题。
   - 拿到的收益：纯 Rust 可移植性 + 摆脱 mmap-on-Windows 那类运维坑（对 Windows 原生部署尤其）。
   - 工作量估计：`EvidenceStore` 的 KV 读写是几张表的点读点写（无 DupSort 需求），从 `reth_libmdbx` API 迁到 redb
     约为单 crate 内局部改动 + 数据迁移/兼容测试；风险主要在编码格式对齐与崩溃恢复语义复核。

4. **"备用"框架不成立**：没有热切换 MDBX↔redb 的真实场景。正确问法是"n42 自有存储要不要去 C 依赖化"——
   **只在推进 reth-free standalone 时做，且只做 `EvidenceStore` 这类小 store；EL 永远留 MDBX。**

## 四、后续（若采纳场景 3）

- 先做只读 spike：清点 `EvidenceStore` 的表/键空间/访问模式与崩溃恢复约定，评估 redb 迁移的编码兼容与恢复语义。
- 迁移期可让 `EvidenceStore` 走 trait 抽象，redb/mdbx 双实现 feature 门控，便于 A/B 与回退（这才是真正的"备用"含义——
  抽象层下的可切换实现，而非运行时热切换）。
- 保持 EL 的 reth-libmdbx 不动；redb 只进 n42 自有 crate，避免把第二套存储引擎带进 EL 构建的复杂度。

## 附：判据一览

| 维度 | 权重（本项目） | MDBX | redb | 胜方 |
|---|---|---|---|---|
| EL 读密集性能（8s slot） | 高 | mmap 零拷贝，最快 | 自管页缓存，较慢 | MDBX |
| DupSort（reth schema 必需） | 硬约束 | 有 | 无 | MDBX（EL 侧唯一可选） |
| 数百 GB 规模验证 | 高（EL） | reth/Erigon 实战 | 未充分验证 | MDBX |
| 纯 Rust / 无 C 工具链 | 中（standalone/交叉编译） | C 库 | 纯 Rust | redb |
| Windows 原生运维 | 中 | mmap/WriteMap 历史坑 | 干净 | redb |
| 崩溃安全 | 高 | 成熟（COW meta） | ACID（较新） | 并列（MDBX 更久经考验） |
| 低流量小 store 适配 | 低（Evidence） | 可用但拖 C 依赖 | 纯 Rust 轻量 | redb |
