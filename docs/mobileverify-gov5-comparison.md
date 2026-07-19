# gov5 mobileverify 与 n42-26 手机见证栈对照

日期：2026-07-18
任务：`docs/codex-task-sync-from-gov5-2026H1.md` P1-5
结论：**不原样移植 gov5。保留 n42-26 当前 dense bitfield；先建立全网稳定、可证明的 MobileIndex，再按密度自适应选择 dense/delta 编码。跨 IDC 合并只在需要“全网一张公共证书”时上线，并利用 StarHub 单 IDC 亲和性做分层合并。注册表承诺应复用 n42-26 的 QMDB/Twig 二叉状态承诺与短证明，不采用 MPT，也不采用已被 gov5 自己删除的 EIP-4788 环形缓冲系统合约。**

## 1. 对照基线

### 1.1 n42-26 当前实现

n42-26 的手机执行与证明链路是：IDC 将 committed block 的执行包广播给本机 StarHub 会话；手机重放后，对统一的 72-byte
`block_hash || block_number || receipts_root` 消息签名；IDC 批量验签、按本机注册表聚合，再把本机证据写入
`attestation_store.json` 和本机 MDBX evidence row。

与本评估直接相关的事实如下：

- `StarHubConfig::default().max_connections = 10_000`，一个活动 QUIC 会话绑定一个握手 BLS 公钥；收据公钥必须与握手公钥相同。
- 手机单收据的规范字段为 224 B：32 B block hash + 8 B number + 32 B receipts root + 48 B pubkey + 96 B signature + 8 B timestamp；版本化 wire header 另 4 B。StarHub 当前实际入口使用 bincode，因此线上帧还含序列化/QUIC 开销。
- `VerifierRegistry` 在手机连接时按**本节点连接先后**分配 `u32` index，存于本节点 `attestation_store.json`。同一组手机在两个 IDC 上通常得到不同 index。
- `AggregatedAttestation` 携带 96 B 聚合签名和 `participant_bitfield: Vec<u8>`，不携带参与者公钥列表。bitfield 长度到本证书最大参与 index 为止。
- 当前动态门槛是本节点连接数的约 2/3，证明在达到门槛时立即关闭；没有全网统一 collection window，也没有跨 IDC 证书 gossip/合并。
- 注册资格由 committed registration/staking transaction 驱动的 `StakingManager` 把关，但“公钥 → 证书位号”的 `VerifierRegistry` 仍是本地连接顺序；其 root 没有进入 header，也没有可供别的 IDC/手机验证的证明。
- 当前执行状态承诺使用 n42-26 的 **QMDB/Twig 二叉树（SBMT combined root）**，不是 MPT。手机收据目前只证明 receipts root；注册表证明是另一个尚未建立的信任面。

源码锚点：

- `crates/n42-network/src/mobile/star_hub.rs`
- `crates/n42-network/src/mobile/receipt_batch.rs`
- `crates/n42-mobile/src/receipt.rs`
- `crates/n42-mobile/src/attestation.rs`
- `crates/n42-node/src/mobile_bridge.rs`
- `crates/n42-node/src/attestation_store.rs`
- `crates/n42-node/src/staking.rs`
- `crates/n42-jmt/src/evidence_store.rs`

### 1.2 gov5 最终设计，而非任务书中的过期中间态

gov5 的最终 mobileverify 栈包含：

- PoP 后进入 pending，epoch commit 时按确定顺序得到全网稳定 `MobileIndex`；
- 对排序 index 集做唯一编码：`uvarint(count)`，首项直接编码 index，后续编码 `gap - 1`；
- 先交换 index/commitment 排除跨 IDC 重复签名，再聚合本地证书，最后合并 signer-disjoint 的同 root 证书；
- 可选 `Header.MobileRegistryRoot` 和 rawdb `MobileRegistryAnchors` 历史表。

任务书 M3 写的是“EIP-4788 式 8191 槽环形缓冲系统合约”。这已不是 gov5 最终实现：gov5 提交
`03d765c7c` 删除了 state contract/ring-buffer 写入，改成 header-only optional commitment + rawdb side table，nil 时旧 header 编码不变；而且 root
正确性是链下核对，不作为 consensus validity 条件。本文按最终代码评估，并明确**不移植已被删除的环形缓冲合约**。

gov5 参考：`206cb01f`（registry/cert/mask）、`dd8069837`（cohort merge）、`03d765c7c`（删除 state ring buffer）、
`ec9716b9a`（delta overflow/重复 index 加固）、`99653af83`（reporter 上界）。

## 2. 500 IDC × 10K 手机的带宽核算

口径：500 个 IDC，每个 StarHub 10,000 个注册/连接手机，全网 5,000,000 个 index。下表只算证明的逻辑字段，不算 bincode、libp2p、QUIC、MDBX framing。

### 2.1 先分清三段流量

1. **IDC → 手机执行包**：mask 编码完全不影响。一个执行包在同 IDC 内扇出给 10K 手机；优化点是缓存、CDN/分层广播，不是 signer mask。
2. **手机 → IDC 收据**：mask 编码也不影响。若 5M 手机每块都回 224 B 逻辑收据，全网入口约 **1.12 GB/块**，但被 500 个 IDC 分摊为 **2.24 MB/IDC/块**。这段不会因最终证书从 dense 改 delta 而变小。
3. **IDC 证书存储/传播**：dense/delta 与是否跨 IDC 合并只优化这一段。

### 2.2 当前 dense bitfield

n42-26 证书固定字段为约 180 B：32 + 8 + 32 + 96 + 4-byte participant count + 8-byte timestamp。每 IDC 注册表 10K 位时，完整 bitfield 是
`ceil(10,000 / 8) = 1,250 B`。

| 参与模式 | 每 IDC | 500 IDC 合计 | 说明 |
|---|---:|---:|---|
| 10K 全参与 | 1,430 B | 715,000 B（约 698 KiB） | 与当前 2/3 动态门槛同属高密度场景 |
| 随机 1/144 参与（约 69 人） | 约 1,410 B | 约 705 KB | bitfield 到最大参与 index；随机样本的最大 index 通常仍接近 10K |
| 全网统一 5M 位图（假设已有稳定全局 index） | 625,180 B | 一张约 610.5 KiB | 当前实现做不到跨节点验证，只是理论下界 |

第二行说明 fixed bitfield 的核心缺点：它取决于最大 index，不取决于实际参与人数；参与稀疏时仍接近 1.25 KB/IDC。

### 2.3 gov5 delta-varint mask

对 10K 个连续 index，mask 约为：2 B count + 首 index 1–4 B + 9,999 个 1 B delta，即约 10,002–10,005 B。证书固定字段为
176 B。

| 参与模式 | 不合并（500 张） | 合并后（一张） | 相对当前方案 |
|---|---:|---:|---|
| 5M 全参与 | 约 5.09 MB | 约 5.00 MB | **约 7–8 倍更差**；连续全集最适合 dense，不适合 delta |
| 随机 1/144（全网约 34,722 人） | 约 138 KB | 约 49 KB | 最终证书比当前 500 张约小 **14 倍**；delta 开始有明显价值 |

1/144 行使用平均 gap≈144 的估算；uvarint delta 多为 1–2 B。实际值受 index 分配、在线率与 duty scheduler 影响，落地前必须用真实 cohort 分布重算。

### 2.4 结论：不能把 delta-varint 设为无条件默认

n42-26 当前会给所有连接手机广播块，且证书门槛约为连接数 2/3，属于高密度 cohort；在这个工作点 dense bitfield 明显更省。gov5 设计假设“大注册表、低 duty-cycle、小参与子集”，因此选择 delta 合理，但假设不能直接搬到 n42-26。

正确方案是给 signer-set codec 加版本/format tag，在 IDC 侧计算两种长度后选较短者：

- `dense`: 明确携带 registry epoch/size 与完整 `ceil(registry_size/8)` 位图；
- `delta`: 严格递增、canonical uvarint，拒绝 trailing bytes、duplicate、overflow、越界；
- 手机不下载全网 mask；mask 只在 IDC/审计端生成与验证。

在切换到低 duty-cycle 采样前，继续用 dense，不为理论稀疏收益增加线上复杂度。

## 3. 跨 IDC cohort 合并是否解决真实问题

### 3.1 现在的碎片化程度

StarHub 的常态是手机维持到单一 IDC 的活动 QUIC 会话，因此同一手机的收据通常只进入一台节点；相较 gov5 的“手机可向任意节点提交”，自然重复远低。当前每个 IDC 只产自己的本地证书，本地奖励、本地 evidence row 也只消费这张证书。没有“全网消费者要求每块只有一张证书”，所以今天的 500 张证书是**分布式本地结果**，不是正在占用共识网络的 500 张全网广播对象。

因此，完整照搬 gov5 三阶段 500-way 协调现在解决的是尚不存在的产品需求，却会新增：

- 两轮 index/cert gossip、collection 延迟与 500 个 reporter 状态；
- 手机断线重连到另一 IDC 时的跨节点重复争议；
- reporter 身份认证、过期 window、恶意 index claim/cert 的额外攻击面；
- naive all-to-all 在 500 IDC 下的放大风险。

### 3.2 何时它会成为真实需求

以下任一目标确定后，碎片证书就会成为真实问题：

- 对外 RPC/浏览器希望每个 block/root 只有一张可独立验证的公共手机证书；
- 奖励从“IDC 本地记分”升级成全网统一结算；
- divergence alarm 要回答全网比例，而不是某 IDC 的局部比例；
- 手机可向任意 IDC/多 IDC 冗余提交。

此时必须先有稳定全局 MobileIndex 和 registry epoch；否则两个本地 bitfield 连位号含义都不同，无法安全 OR/聚合。

### 3.3 适合 n42-26 的改造版

不采用 500 节点平铺协调。利用单 IDC 亲和性：

1. epoch 内给手机分配 home IDC/IDC shard；正常收据只在 home shard 出现，index range 天然 disjoint；
2. 手机 failover 时使用 epoch-scoped lease/单调 session generation；只对 failover index 交换冲突集合，而不是交换全部 10K index；
3. 先在区域/分片内合并，再由少量上层 aggregator 合并区域证书；所有 reporter 消息绑定已认证 validator PeerId；
4. 仍执行 `cert.Verify`、同 block/number/root 检查、signer-disjoint 检查和 reporter/window 上界；
5. 手机协议保持“一包、一重放、一签名、一上行”，协调成本全部留在 IDC。

结论：**当前不做完整 cohort merge；建立公共全网证书/统一奖励需求后，按上述分层方案做。**

## 4. 注册表承诺与手机信任

### 4.1 当前信任缺口

手机重放并签 receipts root 时不需要知道其他手机是谁，因此当前执行正确性不依赖注册表 root。但任何第三方若要验证
`AggregatedAttestation`，必须把 bitfield index 解析成 BLS pubkeys；今天它只能信任产证 IDC 的本地 JSON 注册表和连接顺序。即使 registration transaction 已 committed，也不能证明“本证书的 bit 742 在该高度对应哪个 key”。

所以：

- 对“手机自己重放是否正确”，registry anchor **不是必需**；
- 对“公共聚合证书、跨 IDC 合并、统一奖励是否可审计”，registry anchor **是必需**。

另一个边界是 PoP。当前每张收据在入聚合前都经过随机系数批量验签，因而不能仅靠 rogue key 混入；但未来若只验证 peer aggregate cert、或跳过单收据验签，注册时 PoP 就必须成为硬门槛。稳定 index、epoch、revoke/rotate 也必须在合并前完成。

### 4.2 不直接采用 gov5 header-only advisory root

gov5 最终 header 字段能证明“leader 对这个 root 作了承诺”，但 follower 不重算它，因此不能单独证明 registry 内容正确。若 n42-26 把它宣传为手机/公共验证者的 registry correctness trust anchor，这个信任强度不够。

n42-26 应复用已经逐块 hard-floor 校验的 **QMDB/Twig 二叉状态 root**：把 epoch、稳定 index、key/status/revocation 与一个
`MobileRegistryRoot` 元数据叶纳入确定性执行状态。手机或审计端只取自己的/相关 signer 的 QMDB 二叉 Merkle proof；不下载 5M keys，也不重放注册表。这样 header 已有的 state commitment 间接承诺 registry，不需要再引入一个仅 leader 声明、follower 不验证的 root。

如实现上仍保留独立 optional `MobileRegistryRoot` header 字段，则 follower 必须从同一确定性 registry state 重算并校验，且以 fork/epoch 门控；否则该字段只能标为 advisory telemetry，不能作为信任锚。

### 4.3 历史与低能耗

- 不部署 EIP-4788 式 state ring-buffer 合约；gov5 已删除它，n42-26 也无需再付一次每块状态写入。
- IDC 本地保留最近 K 个 `(height, block_hash, registry_epoch, root)`，长期历史由 archive/static data 提供；是否需要任意高度 Twig proof 由 P2-1 决定。
- 手机只缓存最近可信 header/epoch root 与自己的短 proof。5M 叶二叉 proof 约 23 层，粗略 sibling 数据约 736 B（另加 key/value/codec），远小于下载 625 KB dense mask、5 MB delta 全集或整个注册表。
- 证书 signer mask 与跨 IDC 协调永远不进入手机热路径；手机低能耗铁律不变。

## 5. 结论表

| 项目 | 决定 | 理由 / 前置条件 |
|---|---|---|
| 72 B 统一签名消息、BLS 聚合 | **做（已具备）** | n42-26 已实现且 batch verify 已加固；继续逐收据/随机批量验签 |
| 稳定全局 MobileIndex + registry epoch + revoke/rotate | **改造后做，优先级最高** | 当前连接顺序 index 不能跨 IDC 解释；公共证书与合并的共同前置条件 |
| 注册 PoP | **改造后做** | 当前逐收据验签能挡 rogue-key；任何 aggregate-only/peer-cert 快路径上线前必须强制 PoP |
| delta-varint signer mask | **不作为默认；改造成自适应 codec 后做** | 当前 2/3 高密度下比 dense 大约 7–8 倍；低 duty-cycle 才显著获益 |
| 当前 dense bitfield | **做（保留并修正 codec 边界）** | 10K/IDC 高密度下最省；编码必须携带真实 bitfield 长度/registry size，不能按 participant count 截断 |
| gov5 平铺式跨 IDC cohort merge | **现在不做** | StarHub 单 IDC 亲和、证书本地消费；尚无全网一证需求，平铺协调成本不值 |
| 分层跨 IDC merge | **有公共证书/统一奖励需求后做** | 先完成稳定 index；利用 home IDC、failover lease、区域树形 merge，避免 500-way all-to-all |
| gov5 header-only advisory `MobileRegistryRoot` | **不原样做** | follower 不重算时只能证明 leader 声明，不能证明 registry 正确 |
| QMDB/Twig 状态承诺下的 registry root + 短 proof | **改造后做** | 复用现有二叉状态 root 的逐块有效性下限；手机只取约 O(log N) 的自身/相关 signer proof |
| EIP-4788 式 8191 槽 state ring buffer | **不做** | 任务书引用的是 gov5 已删除中间态；额外每块状态写入无必要 |

## 6. 本次审计附带发现（不在本纯文档任务中改码）

`crates/n42-jmt/src/evidence_store.rs` 当前按 `ceil(participant_count / 8)` 编码 `packed_participants`，但
`participant_count` 是置位数，bitfield 位号却是本地 registry index。稀疏参与时会截断高 index：例如唯一参与者为 index 9,999，内存 bitfield 长 1,250 B，证据 codec 只保留 1 B。该问题会破坏持久化证据的参与者解析/复验，也使“看起来更小”的当前 wire size 没有比较意义。

修复必须单独提交并包含稀疏高 index round-trip 测试；不能借 P1-5 文档 PR 偷改协议格式。修复时还需定义旧 evidence row 的兼容读取策略和显式 bitfield length/registry epoch，避免把本地 index 映射的根本问题只修一半。
