# devlog-119: 合并后深度审计（HIGH-1 全路径收口 + blob 广播漂移修复 + ingest 字节上限）

日期：2026-08-06
基线：main @ `01cb89c`（PR #27 合并 + HIGH-1 v0.5.1 修复快进之后）

## 一、审计范围与方法

上次审计基线是 devlog-118（覆盖到 `b12e38e`）。本次增量：

1. main 侧 HIGH-1 修复（`4d9e34a` 缓存投毒守卫、`3ec35be` 后台导入可重试）；
2. 合并点 `01cb89c` 本身（deps 升级分支 × HIGH-1 修复分支的语义冲突检查）；
3. 两个并行审计代理对合并后整体的新鲜视角复查：
   - 网络层/节点入口（gossip 上限统一、ingest/rpc 边界、blob_port 适配）；
   - twig 缓冲复用 + compact 输出缓存端到端生命周期。

合并点核验：`git diff b12e38e..01cb89c` 恰好等于两个 HIGH-1 提交之和，无额外
冲突解决、无语义交叠（deps 升级不触碰 execution_bridge 的守卫行）。

## 二、发现并修复（3 项）

### 1. HIGH-1 守卫漏掉 observer 路径（同级别，修复）

`observer.rs` 的 `import_block_data` 与 orchestrator 三条路径一样注入
peer 提供的 compact 输出（`cache.inject(..., "observer_import")`），但拒收后
仍**无条件** `insert_if_invalid`。observer 又在入口用
`should_skip(hash, "observer_import")` 拦截，因此伪造的 compact 字节可让
observer 把诚实哈希拉黑并永久卡在该高度——正是 HIGH-1 要消除的失败模式，
修复时漏掉了第 5 个消费点。已补上与其他 4 处一致的 `!compact_injected` 守卫。

审计代理对修复后的全量核验：5 个消费注入字节的 `insert_if_invalid` 点全部
有守卫；`leader_eager_import` 无注入（leader 自产输出），不守卫是正确的；
所有非 Valid 分支（Invalid/Syncing/Accepted/Err）在注入后都 evict。

### 2. blob 广播的发送端/接收端尺寸漂移（中，修复）

`44a01a4` 为 block 主题消除的"第二份硬编码拷贝漂移"在 blob 主题原样存在：

- 发送端 `broadcast_blob_sidecars` 把一个区块的全部 sidecar 打成**单条**
  gossip 消息，序列化后不做尺寸检查（gossipsub `max_transmit_size` 8 MiB
  放行）；
- 接收端 `validate_message` 对 blob 主题的 Reject 阈值是硬编码 1 MiB。

每个 EIP-4844 sidecar ≈ 137 KiB/blob，单块超过 ~7 个 sidecar（如 2 笔
6-blob 交易）即超 1 MiB：发送成功、全网接收端静默 Reject，blob 数据在其他
节点不可用，只有发送端一条 debug 日志。

修复：

- 新共享常量 `n42_network::MAX_BLOB_GOSSIP_MESSAGE_SIZE = 1 MiB`
  （transport.rs，与 `MAX_GOSSIP_MESSAGE_SIZE` 同一处、同一理由）；
  接收端 Reject 阈值改引该常量；
- 发送端新增 `pack_blob_sidecar_frames`：按接收端上限贪心分帧（保序），
  接收端本来就逐条插入 sidecar，分帧对下游透明；单个 sidecar 独帧仍超限
  的（未来 >7 blob/tx 的场景）打 error +
  `n42_blob_sidecar_exceeds_frame_total` 指标显式暴露，而非静默丢失；
- bincode 开销常量（entry 48 B / header 56 B）被
  `exact_budget_fill_stays_single_frame` 用真实 bincode 序列化钉死——
  注意 `B256` 在 bincode 下走 `serialize_bytes`，是 8 字节长度前缀 + 32
  字节（40 B），不是裸 32 B，第一版常量因此差了 16 B，被该测试当场抓住。
- 接收端补上限±1 的 Accept/Reject 测试（绑常量，杜绝再漂移）。

### 3. ingest 批只限条数不限字节（次要加固，修复）

`454e6ad` 的 `MAX_INGEST_BATCH_TXS = 65_536` 挡住了"4 字节头换 GB 分配"，
但恶意客户端仍可真实发送 65,536 × 64 KiB ≈ 4 GiB，全部缓冲进 `raw_txs`
后才做池准入。默认 loopback 绑定使暴露面很小（外开 `N42_INGEST_BIND` 时
攻击者本就可任意花账户），属纵深防御。

修复：新增 `MAX_INGEST_BATCH_BYTES = 64 MiB`（真实客户端 500 tx/批 远低于
此），在读取每笔长度后、缓冲该笔之前累计检查，超限即断连；配套连接级测试
（客户端把写错误视为成功信号，因为服务端会在中途断连）。

## 三、核查为清的项

- 合并点无语义冲突（见上）；
- compact 缓存生命周期：inject→new_payload 同帧无间隙（排除"伪造字节滞留
  被后续无 compact 帧消费而拉黑诚实哈希"的残留窗口）；同 hash 并发导入的
  最坏情形只损失一次缓存命中，正确性由 reth 头校验兜底；
  `discard_unvalidated_sidecar_diff` 未全路径调用**不是**缺陷——sidecar
  diff 的应用门在 `execution_validated_sidecar_hashes` 的 (view,hash) Valid
  绑定，伪造 diff 无绑定流不进 QMDB/Twig，discard 只是纵深防御；
- twig 上层树缓冲复用：`clear()+resize(2*up_cap, NULL_HASH)` 从 0 重填全部
  槽位，padding 语义与旧 `vec![NULL_HASH; ..]` 逐位等价，空 shard/单叶边界
  正确；`node_count()` 持锁读，无撕裂；
- 常量统一（block 主题）四处全绑 `MAX_GOSSIP_MESSAGE_SIZE`；codec 长度前缀
  先检查后分配；`secret_eq` 长度异或截断被末尾等长判定兜住，无绕过；
  `blob_port` 的 `.into()`（`BlobCellAvailability::full()` 包装）在全部调用
  点语义正确（RLP 均来自 leader 广播的完整 sidecar）；
- `decompress_payload` 有 64 MiB 解压上限，无解压炸弹；
- 存量边缘（不在本次变更内，记录备查）：mempool 主题 128 KiB 接收上限对
  "恰好 128 KiB 交易 + 封包开销"存在理论边缘（reth 池默认单笔上限即
  131072 B）。

## 四、验证

- `cargo check --all-targets` ✅
- `cargo clippy --all-targets -- -D warnings` ✅
- `cargo test --workspace` ✅（含新增 7 个测试：blob 分帧 4、接收端阈值 2、
  ingest 字节上限 1）

## 五、后续

- blob 分帧修复建议随下一轮多节点 E2E（场景 4/5）验证一次带 blob 交易的
  传播路径；
- mempool 128 KiB 边缘若要收口，应与 tx 转发路径的实测尺寸分布一起看
  （measure first）。
