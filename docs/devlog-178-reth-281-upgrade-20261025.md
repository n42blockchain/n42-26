# Reth 2.8.1 升级与依赖更新

日期：2026-10-25。分支 `upgrade/reth-v2.8.1`（自 main `75eae9a`）。

## 版本

| 项 | 升级前 | 升级后 |
|---|---|---|
| Reth 上游 | v2.7.0 `3d592ece6de8c4559987416a544fc215fd6d6921` | v2.8.1 `d763342fa185e4e90ee79dc65f6a5706168b0bcb`（上游最新 release） |
| 适配提交（六补丁后） | `3452d7d0217095446b0f2632a283e314951da273` | `5a8b035c3420f6cb5b5b074a51dab2d4272009e6` |
| revm-inspectors | 0.44.1（本地修复） | 0.44.3（同一修复） |
| alloy-trie / alloy-hardforks | 0.9.x / 0.4.9 | 0.9.8 / 0.4.11 |
| lru / parking_lot / zstd（n42 直接依赖，大版本） | 0.18 / 0.12 / 0.13 | 0.19 / 0.13 / 0.14 |
| 其余 crates.io 依赖 | — | `cargo update` 的 semver 兼容最新（tokio 1.54.1、rustls 0.23.46、hyper 1.12.0 等，共 203 个包变动） |

revm 43.0.3、Alloy 2.5.0、alloy-evm 0.39.0、alloy-primitives 1.7.3、reth-primitives-traits 0.8.2
未变：reth v2.8.1 自身没有移动这些版本，与 n42-rs 的 2.8.1 升级记录一致。

## 设计决策

- **补丁迁移方式**：在 v2.8.1 上按原提交顺序 cherry-pick 六个 N42 适配提交，而非重写；
  补丁文件仍为每个适配提交的 `git diff 父..提交`。apply 脚本的应用顺序与提交顺序不同，
  已在干净的上游 v2.8.1 检出上按脚本顺序实际应用，源码树与适配提交逐字节一致。
- **构建隔离**：适配源在 `.artifacts/reth-upgrade-281/reth-worktree`，同级 `reth` 链接指向它；
  n42-26 的 git worktree 放在同目录，`../reth` 直接解析到适配源，Cargo.toml 不需改写，
  也不触碰本机含未提交改动的原 `../reth`。
- **revm-inspectors**：reth 2.8.1 要求 `^0.44.3`，旧 0.44.1 替换不再生效。复用 n42-rs 已验证的
  0.44.3 副本；核对其 = crates.io 0.44.3 原版 + 同一 fee-disabled `traceCall` beneficiary
  prestate 补丁（补丁体与本仓库 0.44.1 版逐行相同）。
- **未做的大版本升级**：bincode 1→3 改变编码 API/格式，涉及手机数据包与持久化，属于协议变更；
  sp1-sdk 4→6 与 SP1 guest 工具链绑定。两者保持不变。zstd 0.14 仍使用 libzstd 1.5.7，压缩格式不变。

## 实施细节与问题

1. **补丁冲突（1 处）**：`reth-import-batch` 与上游新增的 prewarm 结果复用在
   `payload_validator.rs::execute_transactions` 冲突。上游加了 `prewarm_results` 参数，
   N42 加了 `pre_execution_applied` 参数；两者都保留，N42 批量导入的回退路径与普通路径都传
   `prewarm_results.as_deref()`。上游 `--engine.prewarm-reuse` 默认关闭，默认行为不变。
2. **API 变化（1 处）**：`StateProofProvider::multiproof_v2` 去掉 `TrieInput`，改为
   `(targets, requires_masks)`；`n42-node/src/qmdb_state_reader.rs` 的委托签名同步。
3. **clippy `-D warnings`**：修正四处既有写法（`frame.rs` 用 `contains`、`alt_sig.rs` 基准用
   `to_vec`、`dirty_storage.rs` 与 `gov5_rpc.rs` 合并嵌套 `if let`），语义不变。
   reth-ipc 在 interprocess 2.4.7 下有 deprecated 警告，属依赖 crate，不受 `-D warnings` 约束。
4. **Engine 默认值**：持久化阈值 50、state masking 30、state-root task 超时 4s 与 v2.7.0 相同；
   新增 `--engine.prewarm-reuse`、`--engine.cpu-cores` 默认关闭。节点与舰队启动参数无需修改。

## 验证

| 命令 | 结果 |
|---|---|
| `cargo check --all-targets` | 通过 |
| `cargo clippy --all-targets -- -D warnings` | 通过 |
| `cargo test --release --locked --workspace -- --test-threads=8` | **1823 passed / 0 failed / 30 ignored**（62 组），与升级前审计 job 020 计数一致 |
| `python3 -m unittest discover -s scripts -p 'test_*.py'` | 102 通过（含 CI reth 基线检查） |
| 干净 v2.8.1 + `apply-reth-patches.sh` + `check-reth-source.sh` | 通过；树与 `5a8b035c3` 相同 |

日志：`.artifacts/reth-upgrade-281/workspace-tests.log`。

## 状态与后续

- [x] 六补丁迁至 v2.8.1，lock、补丁文件、apply/check 脚本、四个 CI workflow 固定点同步
- [x] 依赖更新、编译、clippy、全量测试
- [x] 七节点混合 soak（新二进制）：第二轮 smoke + 一小时 soak 均 PASS，见下节
- [ ] chain94 兼容分支（n42-rs-wt-reth27-chain94）仍为 reth v2.7.0；n42-rs 主线已在 v2.8.1。

## 新二进制七节点混合 soak（2026-10-26）

拓扑：Go `n42-gov5` 节点 0–4、n42-26 Root 节点 5、RS 兼容节点 6；驱动
`.artifacts/reth-stable-completion-20261012/run-mixed.py`（第二轮为仅给 rs-validator6 打开
`n42.h2.node=debug` 的副本），共享机按 `BOX-CLAIM-PROTOCOL.md` 认领后独占运行。

### 第一轮：FAIL（v2.7.0 二进制 + 审计修复）

smoke PASS（13657309→13657356）；soak 93 s 后 RS 节点报 `committed block is unavailable in
the execution layer`：块 13657418（`0x86c435…78b2`，view 5283，Root 出块）提案到达时无块体，
35 ms 后 Decide，FCU 返回 SYNCING，块体此后再未到达。Root 对 RS 的直推一直是
`UnsupportedProtocols`，RS 只能向 peer 拉取块体；而 `h2-node/src/service.rs` 的
`awaiting_bodies` 是一次性去重集合，块体只请求一次，丢失后永不重试。现场保存在
`mixed27-fleet/newbin-soak-attempt1-rs-missing-body/`。

### RS 修复（n42-rs `upgrade/reth-v2.7.0-chain94-20261012`，已推送）

- `cf1b599ac` fix(h2-node)：`BodyRequests` 记录每个待取块体的上次请求时间与次数，
  每 250 ms 重发，最多 40 次；事件循环增加重试定时分支。
- `4fc928eae` fix(h2-execution)：commit 时 FCU 非 Valid 则保留该块为 `unconfirmed_commit`，
  块体随后到达并执行 Valid 时补发 FCU，使迟到块体的已提交块成为 canonical。
- RS 兼容工作区测试 1950 passed / 0 failed / 35 ignored（新增 4 个）。

### 第二轮：PASS

| 二进制 | sha256 前缀 |
|---|---|
| n42-node（本仓库 reth v2.8.1，`2a6f101`） | `64e4f311d9ae` |
| rs27/n42 | `518c49f523b4` |
| rs27/h2_validator | `d59436e0b0af` |
| n42-gov5（未重建） | `c6a7366bdf23` |

| 阶段 | 区块 | 时长 | 出块者 | 交易 |
|---|---|---|---|---|
| smoke | 13657418→13657468 | 90 s | 7 | 163 成功 / 0 失败 |
| soak | 13657479→13659278（1800 块） | 3600 s | 7 | 7180 成功 / 0 失败 |

审计范围：已提交链 / 收据 / QC 一致性；持久化需重启验证，本轮未做。

重试证据：smoke 启动时 RS 对第一轮丢失的 `0x86c435…` 重新请求（"block body still missing;
asking peers again"）并恢复。soak 中 view 5349 出现一次无块体提案（`0x742330…59cc`），
首次请求即取回、正常投票，未触发重试；全程无 SYNCING、无错误。

日志：`.artifacts/soak-newbin-20261025/job2.log`，`mixed27-fleet/newbin2-{smoke,soak}-*.log`
及 `newbin2-{smoke,soak}-audit/summary.json`。

### 后续

- [ ] 新二进制恢复演练（重启 / 持久化验证）
- [ ] Root→RS 直推 `UnsupportedProtocols` 根因（目前依赖 RS 拉取，功能正确但多一跳）
- [ ] chain94 兼容分支升级到 reth v2.8.1
