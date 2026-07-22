# N42 原生链跨语言节点互通路线（QMDB + HotStuff-2）

> 状态：QMDB Phase 0/1 已完成并通过现有 full replay；H2-v4 wire、共同签名域、静态验证者生产 profile 与 gov5→Rust 真机 finality observer 已完成。本文定义 gov5（Go）与 n42-26（Rust）成为同一条 N42 原生链中独立客户端的边界、迁移顺序和验收条件。replay-v2 的 QMDB 历史和 HotStuff-2（H2）共识是第一优先级；eth-el archive+ 互通在此路线稳定后再单独推进。

## 目标

gov5 和 n42-26 应像 geth 与 reth 一样，能以不同实现加入同一条 N42 原生链：验证同一 `stateScheme=qmdb` 创世与分叉、传播 H2 消息、验证相同 QC、从同一 replay-v2 历史恢复相同 QMDB root，并在同一 finalized head 上出块。

`minimal full archive+` 仍是后续目标，但不应与第一阶段混在一起。第一阶段只定义 QMDB replay bootstrap：节点可导入验证过的 QMDB snapshot 和后续 finalized diff；archive 节点保留 QMDB history（death stamps、block positions、key versions、top band）以服务历史 proof。它不要求两个客户端共享 MDBX 文件。

## 不可破坏的既有能力

- 现有 Rust 7 节点拓扑、其 gossip/direct-block 快路径和性能基准继续作为回归基线。
- 不删除或改写已有 replay 数据、`n42-data`、gov5 `n42data`，也不以清空 datadir 作为迁移手段。
- 已部署的 Rust `/n42/*/1` 请求响应协议和现有 topic 保持可用；跨客户端协议只能新增版本，不能替换其语义。
- 内部实现可以不同：gov5 的 MDBX/JMT/LtHash 与 Rust 的 SBMT/Twig 都是本地索引。互操作承诺的是规范化区块、收据、检查点和证明校验，而不是相同的内部文件布局。

## 已确认的互通缺口

| 层 | gov5 当前实现 | n42-26 当前实现 | 结论 |
|---|---|---|---|
| 传输 | libp2p，SSZ+snappy，协议后缀 `/ssz_snappy` | libp2p，长度前缀 bincode | 连接层相近，wire codec 不兼容 |
| Gossip | `/n42/{fork-digest}/...` | 固定 `/n42/consensus/1`、`/n42/blocks/1` 等 | topic 和 fork 隔离不兼容 |
| 同步 | SSZ 的 status、range、snap/checkpoint RPC | `BlockSyncRequest/Response` 的 bincode `/n42/sync/1` | 不能直接互相同步 |
| QMDB commitment | Go split commitment：冻结 leaf tree + active-bits commitment | Rust Twig 删除时把旧 leaf 写为 `NULL_HASH` | **更新/删除后的 root 不相同；当前 Rust Twig 不能导入 QMDB replay state** |
| QMDB 持久化 | MDBX `qmdbEntries/Twigs/Meta/Index`，history 另有 death stamps 等表 | bincode+zstd snapshot + StateDiff WAL | 不能共享 datadir；需要 portable export/import |
| H2 codec | SSZ `HotStuffConsensusMsg` + snappy | versioned bincode `ConsensusMessage` | 消息不能互解 |
| H2 signed bytes | Round-2 `commit || view_le || block_hash`（46B） | v3 再绑定 `validator_changes_hash`（78B） | QC/commit-vote 的 BLS 验证不能互通 |
| Replay | gov5 replay-v2 已可从旧链 DB 构建 QMDB + history | Rust 有 StateDiff/Twig sidecar，但没有 gov5 QMDB bootstrap 导入器 | 需要同一 replay release 和逐 checkpoint golden output |

以上结论来自 Go 的 `lib/qmdb/qmdb.go`、`lib/qmdb/persist.go`、`modules/table.go`、`internal/consensus/hotstuff/{codec,types}.go` 和 `internal/replay/engine_v2.go`，以及 Rust 的 `crates/n42-twig-core/src/lib.rs`、`crates/n42-primitives/src/consensus/messages.rs`、`crates/n42-network/src/{codec,gossipsub/handlers}.rs`。

## 当前安全结论

**此时不能让 Rust validator 加入 Go 的现有 7 节点 H2 网络。** Rust 的 insert-only QMDB root vectors 是有价值的底层哈希检查，但不足以代表 replay-v2 的真实状态：Go 在 deactivation 时只翻 active bit，保留旧 leaf；Rust 会清空 leaf。更不能接受来自另一端的 CommitQC，因为两端对 Round-2 签名的消息长度和域不同。

现有 7 节点、replay DB 和性能路径因此保持不动，作为 Go-side reference network 与 golden-data producer；所有跨实现接入先以 observer/importer 方式进行。

## 规范边界

跨客户端接口必须先版本化、再实现。原生链 v1 只定义以下四类可验证对象：

1. **Chain identity**：chain id、genesis hash、fork schedule digest、consensus 参数、可选的状态承诺方案标识和最低/最高 protocol version。
2. **Finalized native block bundle**：header、body、receipts、H2 finality/QC、交易根、receipt root 与 QMDB state root。字段的字节序、可选字段和哈希域分隔必须固定。
3. **QMDB bootstrap export**：`nextSlot`、每个 slot 的 key/value、每个 twig 的 active bitset、root、历史层 cursor；分段范围、压缩算法、未压缩字节数、内容哈希、checkpoint 和签名。导出必须包含每个 append slot（包含已删除的 frozen leaf）；只含 live key 或缺失 dead slot 的稀疏导出必须拒绝。MDBX 表是 Go 的内部存储，不是 wire format。
4. **Replay checkpoint**：源链范围、规范化 diff/bundle 哈希、目标 block hash、QMDB root、receipt root、累计交易数和实现版本。它使重放结果可在不共享数据库的情况下比较。

规范对象采用 SSZ 编码，网络承载采用 SSZ+snappy；磁盘归档可使用 Zstd，但其解压后的对象必须是相同的 SSZ 字节。所有长度、列表上限和拒绝条件与 schema 一起固定。bincode 只能保留给 Rust 内部协议，不能成为跨客户端格式。

原生 QMDB profile 的 header state root 必须是双方以完全相同的 QMDB scheme 得出的 root。MPT/JMT/BMT/Twig/LtHash 的其它实现只能以带 scheme ID 的扩展形式出现，不能替代 QMDB root、receipt root 或 block hash 的跨客户端验收。

## 迁移阶段

### Phase 0 — 锁定 QMDB 与 H2 基线

- 为 chain identity、QMDB bootstrap export、H2 的全部 7 个消息类型、一个 finalized bundle 和一个 replay checkpoint 建立语言无关的 golden vectors。
- QMDB vectors 必须覆盖 insert、update、delete、跨 twig、snapshot/reload、history cursor；不能只覆盖 insert。
- H2 vectors 必须覆盖 Round-1、Round-2、timeout 与 NewView 的签名 preimage、SSZ bytes、bitmap bit order 和 BLS verification；未知版本、错误 genesis/fork digest、截断、超长、错误 hash 和错误签名均应拒绝。
- 在 Rust 与 Go CI 都执行同一批 vectors；两端的老 7 节点 E2E 仍须通过。

### Phase 1 — QMDB-compatible Rust engine 与 bootstrap importer

- Rust 新增 **split-commitment QMDB engine**，保留 frozen leaf tree，并以 `blake3(0x03 || active_bits)` 与 leaf root 组合 twig root。它不得替换已有 Twig sidecar，直到完整 vectors 通过。
- 导入器从 portable QMDB bootstrap export 重建 slot layout、active bits 与 history cursors，并逐段验证 root；若 exporter 因 eviction 缺少 dead slot，则必须同时导出可验证的 frozen leaf 数据，不能静默降级为 live-key snapshot。绝不直接写入或依赖 gov5 MDBX datadir。
- Go 增加只读 exporter；Rust 先作为 observer/importer，在每个 replay checkpoint 比较 QMDB root。

### Phase 2 — H2 v4 共用 wire 与签名域

- 新增 `/n42/h2/4/ssz_snappy`，不改写 Rust `/n42/consensus/1` 或 Go 旧 topic；握手协商后才启用。
- 升级 Go 与 Rust 到同一 H2 v4 schema：统一 validator-change binding、`HighTC`、`Decide` 字段、bitmap bit order、BLS domain/preimage。不能选择性“兼容”46B 与 78B commit 签名。
- Rust 先验证 Go 消息、Go 先验证 Rust 消息；双向 codec + signature vectors 通过后才允许 observer 消费 live H2 gossip。

Legacy observer 已先完成：Rust observer 根据本地 genesis fork digest 订阅 gov5 的旧
`hotstuff_consensus/ssz_snappy` topic，严格验证 Go 产生的 Snappy 帧与七类消息，但不把消息交给投票引擎。该路径用于 live 线协议取证，不等同于 H2-v4 validator 互通。

H2-v4 签名域现已由共享 vectors 固定为
`N42H2V4 || phase || chain_id || genesis_hash || view`，并按消息阶段追加 block hash 与 validator-change hash。旧引擎尚未切换；必须等 v4 envelope/topic 与双向验证完成后通过显式链配置启用。

H2-v4 envelope 与 Go 生成的 Snappy frame 也已互认；Rust observer 会同时订阅
`/n42/h2/4/ssz_snappy` 并按显式 chain id/genesis 严格过滤。2026-07-21 的独立七节点真机运行中，Rust 经 TCP/Noise/Yamux 接入 gov5，在 view 476 成功验证真实 Decide 的 BLS CommitQC。当前仍只产生观察事件，不转换为 Rust 原生投票事件；详见 devlog-118。

### Phase 3 — replay-v2 等价性与 catch-up

- 将 replay 输入固定为 QMDB bootstrap export + finalized native block bundle/diff，而不是直接读取另一客户端的 MDBX 表。
- 每个 checkpoint 比较 block hash、receipt root、QMDB root、nextSlot、live count、交易计数及跳过原因；差异保存为可复现的最小 bundle。
- 先运行历史 replay 的小窗口，再运行现有完整历史数据；严禁通过跳过 root 或 receipt 校验来获得“成功”。

### Phase 4 — 受限混合网络

- 在 interop 协议上实现 finalized-range、QMDB bootstrap/history segment 和 proof 请求；先让非验证者/归档客户机使用。
- 仅在 Phase 0–3 长期稳定且 H2 QC、BLS 域分隔、leader schedule 与 proposer 规则均有跨实现 vectors 后，才允许 Go/Rust 验证者共同参与投票。
- 性能协议保持分流：既有 7 节点继续使用优化 direct-block 通道；跨客户端先走可验证的 H2 v4 通道，随后再用基准数据优化。

## 首轮验收

- Go 生成的 QMDB bootstrap export、H2 v4 vectors 和 checkpoint 能被 Rust 验证；Rust 生成的同类对象能被 Go 验证。
- Rust observer 能从 gov5 replay-v2 导出的 QMDB bootstrap 引导，在至少一个 checkpoint 达到相同的 block hash、receipt root、QMDB root、nextSlot 与 live count。
- 双向 1,000-block replay 的每个 checkpoint 完全一致，并覆盖 update、delete、跨 twig、snapshot/reload 与 history proof。
- 现有 7 节点 E2E、SBMT/Twig proof E2E 和高性能 benchmark 无回归；结果单独记录，不与 interop 慢路径混淆。
- 旧协议 peer 和新 interop peer 可同时存在，且错误身份/格式的 peer 不会影响旧网络出块。

## 下一项实现

QMDB 已完成 deterministic vectors、portable exporter、Rust importer，以及既有 87,786,434-slot full replay 的同 root 验收。H2 全部 7 类 legacy 消息、H2-v4 签名域/envelope、QC/TC bitmap、Go Snappy frame、生产 profile 与 Rust 真机只读 finality observer 均已通过。下一项是实现 finalized-range/replay-v2 transport 并完成 1,000-block finalized bundle + QMDB checkpoint replay；在此之前仍禁止混合 validator 投票。

Rust observer 现可在启动网络前通过 `N42_QMDB_BOOTSTRAP` 读取并流式验证
portable checkpoint。必须同时提供 `N42_QMDB_BOOTSTRAP_BLOCK`、
`N42_QMDB_BOOTSTRAP_BLOCK_HASH` 与 `N42_QMDB_BOOTSTRAP_ROOT`；节点将它们连同
chain ID 和 gov5 genesis identity 全部核对，任一不符即 fail closed。该路径只建立
QMDB replay 锚，不会把 gov5 的 QMDB root 冒充成本地 Reth/MPT execution root。

可同时设置 `N42_FINALIZED_RANGE_BOOTSTRAP`。该 bounded v1 bundle 最多包含 128 个
连续 canonical blocks，Rust 会验证 header Keccak、block 内嵌 header、parent lineage、
chain/genesis 与整帧 Blake3，并要求 range head 的 block hash/state root 与 QMDB
checkpoint 完全一致。compact receipts 已按 gov5 native codec 解码，并与 block 交易数
绑定、逐块重算 native receipt root；runtime-02 的 49 块/247 tx 已通过。当前仍是只读
import boundary；安全的两阶段执行导入和网络 request/response 完成前不会推进本地
Reth head，也不会开放 validator 投票。
