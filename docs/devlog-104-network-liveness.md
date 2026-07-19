# Devlog 104: P1-4 网络活性修复包

日期：2026-07-18
范围：`codex-task-sync-from-gov5-2026H1.md` / P1-4

## 审计结论

逐项核对 `n42-network` 与 consensus orchestrator 后，已有基础设施如下：

1. `ReconnectionManager` 已对 trusted peer 无限重试，退避从 5 秒开始，失败后带 ±20% jitter
   指数增长并封顶 5 分钟；普通 discovered peer 最多重试 10 次。配置的 static peer 已按 trusted
   注册，重连覆盖同一个 libp2p Swarm，因而同时恢复 GossipSub 与共识 request-response 直连。
2. GossipSub 的所有 topic 已显式关闭 `mesh_message_deliveries` 负分，IP co-location 权重也为 0；
   合法但低流量的 validator 不会因“没有足够转发”或同机部署被踢出 mesh。仍保留 invalid-message
   负分。没有启用 explicit peer：当前 libp2p 会把 explicit peer 排除在 mesh 形成之外，这反而会
   削弱广播；共识关键消息已有 validator direct-send 备份。
3. 出站 state sync 已有单请求 in-flight、超时、每次最多 128 blocks 和 peer 轮换，但入站请求
   没有 peer 级速率上限。
4. R1/R2 vote 初次发送有 direct + GossipSub 双路径，但 view 不推进时不会重发。

## 修复

### Validator 身份与重连

- Identify 命中配置的 validator PeerId 后，以其 listen addresses 将 peer 升级为 trusted；由验签
  共识消息确认身份后也保持 trusted，禁止后续注册将其降级。
- 认证增加 transport-source 绑定：只有消息来源 PeerId 与配置/确定性 validator PeerId 完全一致
  时，才更新 `validator_index -> PeerId` 路由。GossipSub/Rotor 中继只负责传递消息，不能冒充
  消息签名者污染直连表。

首次 Scenario 10 失败现场正好验证了这个缺陷：同一个中继 PeerId 被连续映射到多个 validator
index，恢复后投票直发到错误节点，链停在高度 84。加 source binding 后该污染消失。

### 投票重发

- 缓存当前 view 最后一个本地签名的 R1 `Vote` 或 R2 `CommitVote`，默认每 2 秒向 collector
  重发完全相同的签名字节；`N42_VOTE_RESEND_MS` 可调，最小 100ms。
- direct-send 成功时不增加 GossipSub 流量；peer 映射缺失或直发失败时回退 GossipSub。
- engine 的 `(view, voter)` 去重保持幂等；收到 `ViewChanged` 或发现本地 view 已变化后立即清除。
- metric：`n42_consensus_vote_resends_total{delivery="direct|gossip"}`。

### Split-view 恢复证明

实弹中进一步发现节点分批重启会停在不同 view：存活组在 view 82，较早重启节点在 view 80，
最后重启节点在 view 57。S2 正确地禁止凭单个未来 Timeout 跳 view，但旧实现只缓存这些消息，
下一任 leader 即使收到 N−f 个同 view 的 Timeout 也无法形成 TC，造成永久活锁。

修复后，落后节点可收集 `FUTURE_VIEW_WINDOW` 内同一 future view 的 Timeout，但必须同时满足：

- 每个 sender 独立且 BLS 签名、内嵌 high-QC 均验证通过；
- 达到该 view validator set 的 N−f quorum；
- validator set 与 next-view set 均能严格解析，禁止跨未 staged epoch 回退到 current set；
- 只有 TC 下一 view 的合法 leader 才可组证书、签 NewView 并推进。

因此单签、重复签名和未知 epoch 都不能改变 view，也没有放宽 S2 的 NewView/TC 证明门槛。
最终 Scenario 10 中，node-6 从本地 view 66 收到 view 89 的 quorum Timeout，形成 TC89，广播
NewView90；全网随后恢复。

`collect_future_timeout` 是明确的共识行为与兼容性断代，不只是网络重连优化：节点现在会缓存
future-view Timeout，逐票验证 sender、BLS 签名和 high-QC，按该 view 的严格 validator set 去重并
要求 N−f；只有目标 next-view leader 才能聚合 TC、签 NewView 并推进。旧节点不会执行这条恢复
路径。它与 consensus protocol v4 一起要求验证者协调升级，禁止把新旧版本当作可滚动混跑。

### Catch-up 节流

- 入站 state-sync 按 PeerId 使用 1 秒固定窗口：未认证 peer 4 requests/s，trusted/authenticated
  validator 32 requests/s。
- 超限请求在进入 response-channel 表与节点数据库读取前丢弃；断连时删除窗口状态。
- metric：`n42_state_sync_requests_throttled_total{peer_class="validator|untrusted"}`。

## 验证

- `cargo test -p n42-consensus -p n42-network -p n42-consensus-service`：
  consensus unit 198/198、`chaos_7node` 12/12、consensus integration 67/67、network 121/121、
  consensus-service 138/138。
- 新增单测覆盖：当前 view vote 重发/换 view 停止、入站 sync 普通与 validator 限额、future
  Timeout 未达 quorum 不推进、真实 TC 推进、未知 future epoch 不允许 current-set fallback。
- `cargo check --workspace --all-targets`、
  `cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`：通过；workspace
  首轮捕获的 parallel-EVM 真竞态已由独立前置提交 `71bf98e` 修复后重新全绿。
- E2E Scenario 9（120s、node-2 停 20s）：7/7 checks；最终三节点高度均为 206，恢复后新增
  78 blocks，10 个采样区块 hash 一致，783 笔 ETH/ERC20 交易通过且 ERC20 总量守恒。
- E2E Scenario 10：10/10 checks；4/7 时高度 85 不增长，恢复后 7 节点最终高度均为 120，
  新增 35 blocks，区块 hash 一致，7 个 leader 均出现，595 个手机验证回执 accepted、0 reject。

## 2026-07-19 返工：epoch、超时请求与执行谱系

对 Scenario 10 连续实弹时又发现并修复三条恢复链：

1. epoch 是 view 区间，不只是“validator set 变更次数”。没有成员变化的边界现在也推进 epoch
   并保存历史集合；启动挂载 schedule 时预先 staged 第一轮配置，live 与 restart 不再对同一 view
   计算出不同 epoch。
2. 普通 state sync 丢失响应后，旧 `sync_in_flight` 会长期挡住 execution catch-up。两条入口现统一
   淘汰超时请求组，再允许多 peer fan-out。
3. 一块在 R1 后因节点掉线而只留下 LockedQC，下一块可沿它构建并取得 CommitQC。重启节点的 reth
   只持久化到更早的 committed head；只同步 CommitQC 子块会永久缺父块。sync/2 现在由 CommitQC
   证明完整 raw execution lineage，逐父链导入预提交祖先，再导入子块；cached execution output 与
   StateDiff 一并保留。执行高度按 payload block number 校准，QMDB/Twig 二叉树逐块应用全部 diff，
   质押扫描也改用真实 execution block number。普通块不在 10K ring 中重复保存大 execution output，
   raw retention 只用于“无独立 CommitQC 的预提交祖先”这一必要情况。

新增 mock reth 验收明确观察到 `prepared #144 -> committed #145` 的导入顺序、执行高度
`143 -> 145`、两份 QMDB/Twig StateDiff 均应用；不完整/不连到本地 exact head 的 raw lineage
不获信任。协议使用新的 `/n42/sync/2` ID，避免 bincode v1/v2 混解。

返工后 release 实弹：

- Scenario 9（3 节点、120 秒、node-2 停 20 秒）：7/7；最终高度 228/228/228，恢复节点补齐
  78 块，10 个 hash 样本一致，780 笔 ETH/ERC20 交易与总量守恒。
- Scenario 10（7 节点）：10/10；7/7 高度 66、6/7 高度 83、5/7 高度 85、4/7 严格停在 85；
  全恢复后高度 138/138/138/138/138/138/138，hash 一致，7 个 leader，595 accepted、0 reject。
