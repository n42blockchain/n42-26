# devlog-137: 两条长期分支收敛合并 + 深度审计

日期：2026-08-07
工作分支：`merge/gov5-branches-20260807`（基于 main @ `8c306e3`，即 PR #28 合入后）

## 一、背景与合并顺序

两条分支长期与 main 并行演进，共享一段公共历史（分叉自 `9018701`，共同祖先
`ab05838`）：

| 分支 | 规模（相对 main） | 内容 |
|------|------------------|------|
| `feat/gov5-n42-live-interop` | 238 提交 / +35K 行 | Gov5 真机互操作（observer→participant）、QMDB state root、H2-v4 全量验签 |
| `chore/security-refresh-20260804` | 上者 + 12 提交 | libp2p 0.57（TCP/noise/yamux）、RustSec 修复、Gov5 fetch 竞态三修 |

因后者包含前者的全部提交，合并顺序取 **gov5-live → security-refresh**，两次
都用 `--no-ff` 保留历史。

**依赖 pin 一律以 main 为准**（CLAUDE.md 红线）：分支把 Alloy 锁成 `=2.2.0`
精确版、reth ref 固定为 SHA `acb016ee1d81`，若按分支侧解冲突会把 main 刚完成的
revm 42.0.1 / alloy-evm 0.38.0 / rpt 0.6.0 / Alloy 2.3.0 矩阵整体降级——正是
历史上 CI 误用旧 ref 导致 reth 2.3→2.2 降级的同一根因。CI workflow 的 reth ref
同样保 main 的分支名 `chore/reth-upstream-20260804`。

## 二、合并冲突解决（共 12 处，两轮）

第一轮（gov5-live）6 处：

- `DEVLOG.md`：编号撞车（main 的 118/119 Audit vs 分支的 118–135 Interop），
  文件名不同，取并集；
- `docs/devlog-111`：add/add，取 main 版（含 observer 第 5 守卫点的描述）；
- `n42-network/src/lib.rs`：导出并集；
- `gossipsub/handlers.rs`：**关键**——分支重构了 `validate_message`（新增
  `gov5_block_topic_hash` 参数）且其冲突区正好覆盖 main 新加的共享常量。按
  分支侧解会静默回退 blob 分帧修复。解法是让 gov5 区块主题**复用**
  `MAX_GOSSIP_MESSAGE_SIZE`，而不是分支的第二份硬编码 8 MiB；
- `execution_bridge.rs` / `orchestrator/mod.rs`：两侧各自在文件尾追加测试模块，
  并集拼接。

第二轮（security-refresh）6 处：`Cargo.toml`（Alloy pin，见上）、三个 workflow
的 reth ref、`Cargo.lock`（按 main 矩阵重新生成）、`DEVLOG.md`。

**合并后复验**：`MAX_GOSSIP_MESSAGE_SIZE`、`MAX_BLOB_GOSSIP_MESSAGE_SIZE`、
`pack_blob_sidecar_frames` 全部存活；`grep` 确认无第二份硬编码尺寸。

## 三、API 适配（reth 20260804 基线）

分支代码写于 reth 旧基线，`EngineValidatorBuilder::build_tree_validator` 的签名
在新基线上从 4 参数（含 `ChangesetCache` + `StateTrieOverlayManager`）改为 3 参数
（`OverlayManager<PrimitivesTy>`）。改 `qmdb_state_root.rs` 的导入与实现签名，
并给 workspace 加 `reth-storage-overlay` 路径依赖。

## 四、审计发现与修复（4 项）

四个并行审计代理分别覆盖：共享核心（QMDB/BLS/rpc_compat）、gov5-live 网络层、
security-refresh 独有 12 提交、orchestrator 分支改动。

### 1.【高】未认证 gossip 可撤销并压制 Gov5 fetch 30 秒

`execution_bridge.rs` 的 `handle_block_data` **第一行**就用
`broadcast.block_hash` 调 `retire_h2_v4_fetch_satisfied_elsewhere`。该 hash 是
bincode 信封的**自述字段**，此刻只做过反序列化；信封与 payload 的一致性校验
（`execution_data.block_hash() != hash`）在其后、且在 spawn 出去的 eager-import
任务里。撤销会写两份 30 秒墓碑（本地 + 网络层）并取消在途请求，随后 30 秒内
拒绝重新请求该 hash。

失败场景：Gov5 interop 模式下，任一能连上 p2p 端口的节点发一条
`BlockDataBroadcast`，`block_hash` 填成受害节点正在回溯抓取的 Gov5 区块、
payload 填垃圾 → 撤销真实 in-flight fetch 并抑制 30 秒；每 30 秒重复即可无限期
卡住祖先链回溯。信任边界与 devlog-111 的 HIGH-1 compact 投毒完全相同。

对照可见这是非对称的：observer 的同名撤销（`observer.rs`）拿的 hash 来自
`decode_gov5_block_rlp` 的 `keccak256(header_rlp)`，自证明、伪造不了。

**修复**：撤销从 `handle_block_data` 移到 `handle_eager_import_done`——只有 reth
对该确切 payload 返回 Valid 才会到那里，哈希是**被证明**而非被声称的；同时该
位置天然在 `h2_v4_identity.is_some()` 分支内，顺带修掉了原生模式下每块一次无谓
墓碑写 + 共识热路径上一次可阻塞 50 ms 的 `send_with_backpressure`（审计发现 2）。

### 2.【高】伪造签名钩子编译进生产二进制且落在原生共识路径

`orchestrator/mod.rs` 的 `broadcast_engine_consensus` 开头无条件读
`N42_QUALIFICATION_FORGE_CONSENSUS`，命中即用固定密钥 `[0xA5; 32]` 覆盖
Proposal/Vote/CommitVote/Timeout/NewView 的签名后广播。它不是 gov5 专属路径——
`SendToValidator` 在两种模式下都调它，而 Vote/CommitVote 是每块每验证者的最高频
消息。release 部署中该变量若被误设或注入（systemd unit、容器镜像、CI 环境泄漏），
节点将持续广播无效签名，对所有 view 不再产生有效票。

**修复**：加 `#[cfg(debug_assertions)]`，release 构建中整段编译掉。

### 3.【中】匿名 gossipsub 信封的作用域过宽

分支把 `MessageAuthenticity::Signed(key)` 改为 `Anonymous`（Gov5 跑
StrictNoSign，会拒绝签名信封并最终把 Rust peer 从 mesh 里剪掉）。但该改动位于
两条构建路径共用的 `build_behaviour` 闭包内，普通验证者经
`build_swarm_with_validator_index` 构建时也不再签名信封，与"gov5 应 opt-in、
生产零影响"的不变量相悖。

功能与安全上未发现回归（转发者身份用的是传输握手认证的 `propagation_source`，
全仓无处读 `message.source`；两侧 `validation_mode` 都是 `Permissive`，新旧节点
混合部署互通）——这是**爆炸半径**问题。**修复**：按 `enable_gov5_tcp` 门控，
生产 swarm 保留原有签名信封。

### 4.【低】gov5 已服务区块缓存随机驱逐

满 1024 时用 `HashMap::keys().next()` 选驱逐对象——HashMap 迭代序任意，刚广播、
最可能被 follower 回取的新块可能立刻被顶掉。**修复**：加 `VecDeque` 记插入序，
按 FIFO 驱逐。

### 5. 清理过期的 RUSTSEC ignore（门禁修复）

`audit.toml` 与 `nightly.yml` 仍在无条件 ignore 三条 advisory，但合并后的
lockfile 里成因已消失：libp2p 0.57 让 hickory-proto 只剩 0.26.1
（RUSTSEC-2026-0118/0119），tracing-subscriber 只剩 0.3.23（RUSTSEC-2025-0055）。
过期的 ignore 比没有更糟——它会静默吸收它当初想容忍的那个回归。已清空 ignore 列表，
并在注释里写明"每次 reth/libp2p bump 都要重审此表"。

顺带记录：这也是本次合并**唯一实质性的安全增量**——main 此前的 lockfile 里
hickory-proto 0.25.2 经 libp2p 0.56 真实存在，只有升到 0.57 才能消掉。

## 五、核查为清（要点）

- **默认关闭链条完整**：`N42Node::new` 默认 Ethereum profile + 无 QMDB store；
  Gov5H2 需三个条件齐备（env + observer/H2v4 模式 + genesis hash），缺一报错退出
  而非静默降级；`h2_v4_identity` 默认 `None`，相关事件分支带守卫；
  `N42HeaderProfile` 的 `#[default]` 是 `Ethereum` 且全量委托内层验证器。
- **H2-v4 执行门控投票 fail-closed**：两处乐观投票在 H2 模式下关闭，投票只能从
  `new_payload` 返回 Valid 的回调释放；leader 侧四处失败点（规范化失败、
  非 Valid、RLP 编码失败、广播失败）全部放弃提案而非降级发布。
- **与 HIGH-1 守卫兼容**：三处 `!compact_injected` 守卫原样保留；分支新增的唯一
  `insert_if_invalid`（`gov5_leader_pre_proposal`）消费的是 leader 本地构建的
  载荷，无对端注入，拉黑自算哈希是正确的；gov5 导入路径的 `execution_output`
  恒为 `None`，且块体与已认证哈希由 `transactions_root` + `keccak256(header_rlp)`
  硬绑定。
- **engine_validator 无绕过**：三条 header 重建路径全部以
  `seal_slow().hash() == expected_hash` 收口，篡改即失配。
- **BLS 无弱化**：H2-v4（POP DST）与原生（NUL DST）域完全分离，批验与回退绑定
  同一 `Ciphersuite` 值，四组交叉拒绝测试齐全。
- **AtomicU64 水位语义正确**：`load(Acquire)`/`fetch_max(AcqRel)` 配对；
  "N 已 Valid 蕴含 N 以下已在树内"这一不变量成立（reth 只在祖先齐备时才回 Valid）。
- **scripts**：54 个新增 gov5 资格脚本无危险操作（`rm -rf` 目标均为脚本自建的
  临时目录变量，无 `curl|bash`，十六进制串是区块哈希断言非密钥）。

## 六、留档：未修但需跟踪的项

1. **QMDB archive RPC 持锁全谱系重放**（中）：`qmdbArchiveState`/`qmdbArchiveProof`
   在持 store 锁期间从 base snapshot 逐块重放，与导入路径 `compute_and_commit`
   争用同一把锁；开启 archive 且保留数千块时，无认证客户端反复调用可饿死导入。
   功能 opt-in 且 RPC 通常内网，故未在本次合并中改。建议后续把重建移出锁外，或给
   archive 端点加节流/admin 门。
2. **branch store 无修剪**（低-中）：`blocks` HashMap 与 WAL 只增不减，重启校验
   对每块从 base 重放 = 链长二次方。资格测试无害，长跑需随 finalized 折叠
   base checkpoint。
3. ~~**每块一次 fsync + 第二次快照写**~~（**已测量，结论：不动**）：
   `advance_execution_validated_head` 无条件写 lineage proof（60 字节 +
   `sync_data()`）并再存一次共识状态，原生模式下基本每块触发；lineage 文件只追加
   无轮转，启动时全量读入。两半担忧各量一次（见第八节），都不成立：每块
   **6.5 ms**（占 8s slot 的 0.081%），一年 226 MiB 的 lineage 文件恢复
   **340 ms**。按"先测量"的规矩，不写优化。
4. ~~**eager-import 水位语义放宽**~~（**已由场景 9 验证，见第七节**）：为修 catch-up
   毒化，抑制从"即写即拦"改为"Valid 后才抬水位"，且视图切换时重置为 0。同高度
   不同哈希的两个 eager import 现在可能双双提交给 reth。这是修复必须付的代价，
   属原生生产路径的行为放宽——两轮场景 9 均 7/7 通过，未见恢复路径回归。
5. ~~**ed25519-dalek 双版本共存**~~（**已查清，判断被推翻**）：审计据 workspace 的
   `ed25519-dalek = "2"` 推断"手机侧用 2.x、值得与 libp2p 0.57 的 3.0.0 统一"。
   实际**没有任何 crate 依赖它**——`n42-mobile` 早已改走 `n42-primitives` 的 BLS，
   那行是死声明（CLAUDE.md 的 crate 描述也停在旧状态，一并更正）。图里两个版本
   分别来自 reth 的 enr/discv5 栈（2.2.0）与 libp2p 0.57（3.0.0），**改我们自己的
   pin 无法收敛**，只能等上游对齐。死声明已删。
   顺带清掉一个从未生效的 `[patch.crates-io] ark-relations`：patch 给 0.5.1 而图里
   解析出 0.6.0，cargo 每次都报 `patch was not used in the crate graph`，它只在
   lockfile 里留了个 git 源（绕开 crates.io yank、cargo-audit 匹配不可靠）。
6. **libp2p 0.57 仍是未发布的 git rev**（`6348a0be`），另有 `[patch.crates-io]`
   的 ark-relations git rev。git 源绕开 crates.io 的 yank 机制，cargo-audit 对
   git 依赖的匹配也不可靠。要么等 0.57 正式发版再跟，要么明确记录复核责任与
   退出条件。

## 七、验证

- `cargo check --all-targets` ✅（唯一警告在 reth fork 上游 `fs-util`，不在门禁内）
- `cargo clippy --all-targets -- -D warnings` ✅
- `cargo test --workspace` ✅
- CI（PR #29）：Lint+单测、E2E smoke-consensus(1/3/4)、E2E mobile-rpc(5/8/12) 全绿，
  一次通过无重跑。

### 场景 9（崩溃/恢复）手动验证 — 覆盖第六节第 4 项

CI 不含场景 9，而水位语义放宽最可能出问题的正是"落后节点重新加入 + 大量视图切换"
这条路径，故本地跑两轮，均 **7/7 通过**：

| 参数 | 压缩轮 | 完整轮（默认参数） |
|------|--------|-------------------|
| 时长 / 出块间隔 | 600s / 1000ms | 3600s / 500ms |
| 实际出块 | 537（理论 600） | 6841（理论 7200） |
| 崩溃点 / 停机 | 高度 307 / 60s | 高度 3546 / 121s |
| 恢复后共识推进 | 237 块 | **3321 块** |
| 追平耗时 | 8.7s | **25.2s（≈132 块/秒）** |
| 最终三节点高度 | 544/544/544 diff=0 | 6867/6867/6867 diff=0 |

完整轮命中了最危险的组合——落后 3321 块的节点重新加入、追块期间同高度不同哈希的
提交窗口——而追平吞吐达正常出块速率（2 块/秒）的 66 倍、状态分毫不差，说明放宽
既未破坏一致性、也未拖慢同步路径。V2 的出块缺口（6841 vs 7200）等于停机 121 秒的
产能损失加同步窗口，符合预期。

## 八、持久化开销测量（关闭第六节第 3 项）

审计提出"每块一次内联 fsync 值得按先测量的规矩量一次"。两个 `#[ignore]` 基准写在
`crates/n42-consensus-service/src/persistence.rs`，随代码走、可重复执行：

```bash
cargo test -p n42-consensus-service --release persist_cost -- --ignored --nocapture
cargo test -p n42-consensus-service --release lineage_recovery_cost -- --ignored --nocapture
```

本机（Windows / NTFS，release）实测：

| 项目 | 实测 | 相对基准 |
|------|------|----------|
| lineage 记录 append + `sync_data()` | 2.9 ms/块 | — |
| 快照 write + `sync_all()` + rename | 3.6 ms/块 | — |
| **每块合计** | **6.5 ms** | **8s slot 的 0.081%** |
| 一年 lineage 文件（3,942,000 记录 / 225.6 MiB）全量恢复 | **340 ms** | 一次性，仅启动时 |

两半担忧都不成立：每块开销比 slot 预算小三个数量级；恢复扫描虽是 O(记录数)，但
226 MiB 只要 340 ms——即便跑十年也不到 4 秒，且只在启动时付一次。恢复基准刻意把
唯一匹配记录放在文件末尾，测的是无法提前退出的最坏情况。

**结论：不动。** 折叠 base checkpoint、批量 append、去掉第二次快照写这些方案都有
真实成本（崩溃恢复语义变复杂），而数据显示它们要解决的问题不存在。这与
devlog-8x 的教训一致：先量，再决定值不值得写。

注：数值是 Windows NTFS 上的；生产 Linux 通常 fsync 更快，所以这是保守上界。若将来
把共识状态挪到更慢的介质（网络盘）或把 slot 压到亚秒级，重跑这两个基准即可。
