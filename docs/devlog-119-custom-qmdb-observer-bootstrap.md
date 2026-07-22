# devlog-119 — 自定义链 replay-v2 QMDB observer bootstrap

日期：2026-07-21

分支：`feat/gov5-n42-live-interop`

基线：`main @ f904813`、Reth `2.4.1 @ c533db8`

## 目标

把已完成的 gov5 replay-v2 portable exporter 与 Rust 流式 QMDB verifier 接入真实
observer 启动路径。此阶段只建立可信 checkpoint 锚；不会把 gov5 QMDB root 写入
Reth MPT，也不会据此让 Rust validator 参与 gov5 H2 投票。

## 合并后审计

先复核 `f904813` 的合并接缝及审计结论：observer 门控仍默认关闭且只读，H2-v4
wire/HighTC 上限、BLS CommitQC 验证和 chain/genesis domain 绑定没有出现新的
CRITICAL/HIGH/MEDIUM。S5 compact-output 投毒 HIGH-1 保持在独立
`fix/high1-compact-output-poison` 分支，未混入本互通分支。

新增启动接线继续遵守该边界：仅当显式设置 `N42_QMDB_BOOTSTRAP` 时启用，流式验证
portable 内容摘要、完整 positional slot log、chain ID、gov5 genesis、QMDB root，
并额外固定 checkpoint block number/hash/root。任一不符即在 observer 共识网络启动前
失败；校验器不持有完整 value/slot 历史。

## 真实自定义链验收

使用保留的第二套 7 节点 gov5 自定义链和 replay-v2 输出：

- chain ID：`1143`
- gov5 genesis：`b71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec`
- checkpoint：block `49`
- block hash：`65fa3122421bd17ffba3c4fa4730646dcf2e49a1b428cb3b67012802c4d68e1a`
- QMDB root：`6fb33357c8db5eb206f506af271cf5fff885fc11bbd82b405b74a42943c98314`
- positional slots/live keys：`437 / 34`

Rust observer 从
`runtime-02-replay-history-v2/qmdb.portable` 流式重算并逐项得到上述相同结果，随后才
建立 H2-v4 observer swarm。日志同时显示本地 Reth execution genesis 为独立的
`9b6b34f8...b7ccbfae`，证明 checkpoint 校验没有把 QMDB root 或 gov5 genesis 冒充
为 Reth/MPT execution 状态。

验收使用新的 `rust-observer-bootstrap-runtime`，没有删除、迁移或覆盖原 runtime-01、
runtime-02 七节点、replay history 或性能数据；进程以 Ctrl-C 正常退出。

## 回归门禁

- `observer_qmdb_bootstrap_accepts_exact_checkpoint_identity`：固定跨客户端 portable
  vector，验证 checkpoint identity、next slot 和 live count。
- `observer_qmdb_bootstrap_rejects_mismatched_checkpoint_metadata`：即使 portable 本身
  内容摘要与 QMDB root 自洽，只要运维固定的 expected root 不同也必须拒绝。
- `cargo test -p n42-node-bin observer_identity_tests`：5/5 通过
- `cargo test -p n42-twig-core qmdb_compat`：9/9 通过
- `bash -n scripts/run-gov5-interop-observer.sh`
- `cargo check --all-targets`：通过
- `cargo clippy --all-targets -- -D warnings`：零警告
- `cargo test --workspace`：全 workspace 通过，0 失败

## 下一阶段

当前 bootstrap 是只读可信锚，不是 execution state importer。下一步实现
finalized-range/replay-v2 transport，传输并验证 checkpoint 之后的 finalized block、
receipt 与 QMDB delta；在 receipt root、QMDB root、H2 finality 三者连续一致前，继续
禁止 Rust validator 混入 gov5 的 7 节点投票集合。
