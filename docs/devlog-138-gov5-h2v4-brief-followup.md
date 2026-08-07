# devlog-138: 按 gov5 H2-v4 hand-off brief 收口互操作缺口

日期：2026-08-07
输入：`c:\n42\n42-gov5\docs\H2_V4_RUST_SYNC_BRIEF.md`（gov5 main @ `f019b3fb`，
H2-v4 profile 合并提交 `c387d6c6`）

## 一、brief 的六项行动与 n42-26 现状

| # | 行动项 | 状态 |
|---|--------|------|
| 1 | 核对四个 fixture 的 SHA-256 | ✅ 内容一致，但发现 CRLF 隐患（见二） |
| 2 | 实现/复核 v4 八个签名域 | ✅ 已有（`h2_v4.rs` 对齐 `h2_v4_domains_v1.json`） |
| 3 | 信封编解码 + **五条拒绝路径** | ⚠️ 缺 canonical 检查，已补（见三） |
| 4 | 消费 finality proof（订阅 + 验证） | ✅ 已有（`verify_h2_v4_decide` + `/n42/h2/4/ssz_snappy`） |
| 5 | 联合测试网（gov5 `interopV4:true` + n42 observer） | ✅ **已跑通**（见七） |
| 6 | v4 链上**不要**配 epoch 验证者变更 | ⚠️ 原为纯运维约定，已变成代码保证（见四） |

## 二、fixture 完整性：git 内容正确，工作区被 CRLF 改写

四个 vendored fixture 的 **git blob SHA 与 gov5 逐字节一致**，但 Windows 检出
（`core.autocrlf=true`、仓库无 `.gitattributes`）把工作区文件改成了 CRLF：

| fixture | 工作区 SHA（改写后） | git blob / gov5 SHA |
|---------|---------------------|---------------------|
| cross_client_h2_v1.json | `cb877831…` | `0c587743…` ✅ |
| h2_v4_domains_v1.json | `76833878…` | `f3f20d46…` ✅ |
| h2_v4_envelope_v1.json | `43fa283d…` | `09a98f54…` ✅ |
| h2_v4_finality_v1.json | `80631a0f…` | `feacd6d0…` ✅ |

这正是 brief 里"learned the hard way"那一段描述的场景。**现有测试永远发现不了它**
——四个 fixture 全部走 `include_str!` + `serde_json::from_str`，serde 会吃掉行尾
差异，所以 CRLF 改写、重新生成、手工编辑都不会让任何测试变红。

处理：
1. 新增 `.gitattributes` 钉住两处 `testdata/*.json -text`（与 gov5 同款），
   并把工作区归一回 LF；
2. 把"人工核对一次"升级为**持续门禁**：`vendored_fixtures_match_the_gov5_contract`
   （n42-network，3 个）与 `vendored_finality_fixture_matches_the_gov5_contract`
   （n42-consensus-service，1 个）用 `include_bytes!` 对**原始字节**算 SHA-256，
   钉住 brief 公布的四个值。gov5 改版本时，两边的常量必须一起更新。

## 三、补上第五条拒绝路径：canonical payload 检查

gov5 的 `DecodeH2V4Envelope`（`interop_v4_wire.go:72-75`）在解码内层消息后会
**re-encode 并逐字节比对**，不一致即 `non-canonical H2-v4 payload`。n42 的
`decode_envelope` 没有这一步。

核查过 n42 侧的严格性：长度前缀是固定 4 字节 u32（无 varint，不存在非最小编码）、
`decode_envelope` 有 `consumed != rest.len()` 尾字节检查、各 payload decoder 有
`finish()`、`validate_bitmap` 要求长度精确等于 `2 + ceil(count/8)` 且 padding 位为零。
**没能构造出当前可利用的非规范编码。**

仍然补上，理由是：这是共识流量，两个客户端必须对"哪些字节串是合法消息"给出相同
判定。缺了这道检查，一致性就依赖两套编解码器各自独立地保持同步——任一侧未来的
编码器改动都可能悄悄打开缺口，而症状会表现为联合测试网上难以定位的偶发拒绝。
成本是一次 re-encode + memcmp。

同时为 brief 点名的五条路径各写一个用例
（`envelope_decoding_rejects_all_five_documented_paths`）：身份不匹配（chain_id 与
genesis_hash 两种）、声明长度不符、尾随字节、Snappy 解压超限（按声明长度拒绝，
不分配）、非规范内层消息。

## 四、v4 下的 validator-change hash：日志说零，代码没做

`bin/n42-node/src/main.rs` 的启动日志已经声称：

> "H2-v4 static-schedule epoch profile enabled; committee changes activate at
> view boundaries **with changes_hash=0**"

但 `ConsensusSigningProfile::H2V4` 的两个分支实际传的是
`validator_changes_hash(validator_changes)`：

```rust
Self::H2V4(identity) => h2_v4_proposal_signing_message(
    identity, view, block_hash, validator_changes_hash(validator_changes),
)
```

而 gov5 在 v4 下**硬编码 `types.Hash{}`**（`interop_v4.go:77`、`:91`）。

- 无 committee 变更时：`validator_changes_hash(None)` = `B256::ZERO`，两侧一致 ——
  **所以这个分歧至今没有暴露**；
- 一旦真发生变更：n42 签 `blake3(changes)`、gov5 签零 → 同一个 proposal 的签名
  preimage 不同 → 两侧互相验签失败，**恰好卡在链要重配置的那个 view**。

brief 第 6 条（"不要在 v4 链上配 epoch 变更"）本是纯运维约定，靠人记住；现改为
代码保证：v4 profile 的 proposal/commit 签名硬编码零，与 gov5 和启动日志一致。
Native 路径不变，仍绑定真实 changes hash。测试
`h2_v4_signing_ignores_validator_changes` 同时钉住两侧行为，并先断言这组 changes
确实 hash 出非零值，避免断言落空。

未来 gov5 引入动态验证者变更时，两侧必须先统一 `changes_hash` 的推导方式
（brief 第 53-62 行的语义注解正是此意），届时同步放开这里。

## 五、验证

- `cargo check --all-targets` ✅
- `cargo clippy --all-targets -- -D warnings` ✅
- `cargo test --workspace` ✅（46 组）
- 新增测试 6 个：fixture SHA 门禁 2、五条拒绝路径 1（含 6 个断言分支）、
  v4 changes_hash 钉住 1

## 六、后续

1. **行动项 5（联合测试网）**：需要起一条 `hotstuff.interopV4: true` 的全新静态
   验证者 gov5 链，挂 n42-26 observer，验收标准是"observer 仅凭 envelope 跟随
   finality"。可选：用 `n42-qmdb-export` 快照做 observer 引导。
2. gov5 若更新 fixture，**必须同时更新** n42 侧的四个 SHA 常量——门禁会准确指出
   是哪一个漂移了。
3. 本次未改 `verify_h2_v4_decide` 对 `changes_hash` 的处理：它把信封里的值绑进
   验签消息，篡改会导致验签失败，因此非零值不构成安全问题；等动态变更规范落地后
   再决定是否显式拒绝非零。

## 七、联合测试网：observer 仅凭 envelope 跟随 gov5 finality

### 拓扑（与在跑的 7 节点性能舰队完全隔离）

舰队正跑在 `mainnet_qmdb_staggered`（chainId 94，`interopV4` **ABSENT** —— 与 brief
一致：flag 缺失时 legacy wire 字节不变，性能网不受影响），当天刚滚动升级到 v5.7.947。
brief 要求 v4 只能用于**新链**（它改变签名字节），故另建：

| 项 | 舰队（勿动） | v4 联合测试网 |
|----|-------------|--------------|
| chainId | 94 | **941** |
| interopV4 | absent | **true** |
| epochLength | 200 | **0**（遵守 brief 第 6 条） |
| p2p / http | 32000-32006 / 20012-20018 | 32100-32103 / 20112-20115 |
| 数据 | `E:/qs-node*` | `C:/n42/v4-interop-testnet` |

4 节点（f=1、quorum=3），genesis `0x09c4f0bc…dc2ee1`。

### 验收结果

| 指标 | 结果 |
|------|------|
| 验证通过的 H2-v4 finality proof | **50 个**，view 连续至 56 |
| 提供证明的 gov5 源 | **4 个节点全覆盖** |
| H2 影子验签 | `h2_shadow_verified=[…,48]`，**`h2_shadow_rejected` 全零** |
| observer 跟随 | `highest_seen=54`，gov5 实际 55 |
| gov5 四节点高度 | 一致 |

日志证据保留在 `C:/n42/v4-interop-testnet/observer-evidence.log`。这满足 brief
的验收定义："success = the observer follows finality from envelopes alone"。

### 途中撞到的三件事

**1. interop profile 对 genesis 有硬性形状要求（非缺陷，是配置约束）。**
observer 起初拒绝整条链：先 `difficulty is not zero`（gov5 对未指定的 genesis
difficulty 默认 `0x20000`），改零后又 `extra data is not exactly 32 zero bytes`。
根因是 `validate_gov5_interop_header` 按 extra_data 是否带 gov5 magic 分流——gov5
H2 引擎产的块带 magic 走宽松检查，而**手写的 genesis 不带**，落进严格的 replay-v2
历史形状（8 条约束，见 `adapter.rs:82`）。finality following 不受影响；要让
observer 也导入区块体（当前 `imported=0`），genesis 需满足其中一种形状，或按 brief
用 `n42-qmdb-export` 快照引导。**新建 v4 链时须知。**

顺带记录 gov5 的一个好设计：datadir 有 network binding（`network.json` 记着 genesis
hash），换链重 init 会 fail-closed 拒绝，防止数据目录被误用于另一条链。

**2. 一个真实的重试缺陷（已修，见八）。**

**3. Git Bash 会静默破坏 multiaddr。** MSYS 路径转换把 `/ip4/127.0.0.1/tcp/...`
当 Unix 路径改写成 `C:/Program Files/...`，节点全 0 peers，日志只说"连接被拒绝"，
看不出参数已被篡改。**在 Windows 上启动带 multiaddr 的进程一律用 PowerShell**
（或设 `MSYS_NO_PATHCONV=1`）。

## 八、修复：确定性拒绝不再无限重试

### 现象

联合测试网上，observer 对 genesis 的 fetch 失败进入紧循环：

- **174,943 条**相同错误，**119 秒**，即 **~1,465 行/秒**
- **66 MB** 日志（约 2 GB/小时，会填满磁盘）
- 全部指向**同一个** block hash

### 根因

`retry_gov5_block_fetch` 无条件换一个 peer **立即**重试，既不区分错误类型、也无
退避与次数上限：peer A 失败 → 立刻问 peer B → 失败 → 又回到 A。

而 header 形状违规是**确定性**的：block_hash 承诺了 header 内容，任何 peer 若给出
不同字节会先在哈希校验处失败，根本到不了这个错误。所以换 peer 只是以网络速度
重新推导同一个结论。

这与本轮早先修的 HIGH-1 恰好互为镜像：那里是"不该拉黑的拉黑了"，这里是"该记住的
没记住"。

### 修法

1. `Gov5BlockError::is_permanent()`——用穷举 match 而非 `_` 通配，**新增变体必须
   显式表态**属于哪一类；
2. `NetworkEvent::Gov5BlockFetchFailed` 增 `permanent: bool`。三个发射点按语义分类：
   内容裁决 → 永久；**hash 不匹配 → 非永久**（该 peer 给错了块，别的 peer 可能有对的）；
   **传输失败 → 非永久**（根本没看到内容）；
3. observer 记住被裁定的哈希（`gov5_rejected`，上限 4096 条、FIFO），**每个哈希只
   告警一次**；orchestrator 侧同样退休该 fetch；
4. 关键：守卫加在 **`request_gov5_block` 入口**而不只是重试路径——祖先回溯与
   finality catch-up 都会独立发起 fetch，任一条重新请求已裁定的哈希就会重启循环。

### 测试

- `every_content_verdict_is_permanent`：逐个列出全部变体，防止新变体默认站错边；
- `permanent_rejection_is_remembered_and_stops_refetching`：重试路径与入口双双拦截，
  且不误伤无关哈希；
- `rejection_set_is_bounded`：该集合由 peer 提供的哈希喂养，必须有界。

写测试时踩到一个坑值得记：辅助函数里创建的 channel receiver 若不返回，函数一返回就
被 drop，channel 关闭，observer 的所有发送静默失败——测试会以看似无关的断言失败。
现在辅助函数把 receiver 一并交出。
