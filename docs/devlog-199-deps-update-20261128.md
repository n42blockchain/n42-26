# devlog-199：依赖更新（2026-11-28）

## 背景

检查 reth 是否有新版本，并更新全部依赖库。

## reth

- 上游最新正式版仍是 v2.11.0（2026-11-11），即当前基线，没有可升级的版本。
  上游 main 之后有新提交（2026-11-27 `e9fc4b5`），未发布，不跟。
- 补丁序列不变（含 #56 的第二十个补丁）。

## 锁文件更新

- `cargo update`：184 项，全部在现有版本约束内（semver 兼容），未改 Cargo.toml 的约束。
  主要：alloy 2.5.0 → 2.6.1（alloy-primitives 仍 1.7.3），hyper 1.12 → 1.13，jsonrpsee 0.26.1 → 0.26.2，
  clap 4.6.10，cc 1.12，serde_with 3.25 等。完整列表见本 PR 的 `Cargo.lock` diff。
- 固定在 git rev 的依赖保持不变：evm2 / evm2-jit / evm2-macros `66687c18`、reth-core `275ce4cf`
  与 reth v2.11.0 自己的 Cargo.toml 一致；libp2p `6348a0be` 不动。

## 直接依赖的大版本

用 crates.io 查了所有直接依赖的最新稳定版，落后大版本的只有：

| 依赖 | 现在 | 最新 | 处理 |
|---|---|---|---|
| base64（n42-el-rpc） | 0.22 | 0.23.1 | 升级；API 不变，测试通过 |
| rand_core（n42-tx-types） | 0.6 | 0.10.1 | 不升：0x50 批量验签里的 merlin transcript RNG 只接受 rand_core 0.6，与 ed25519-dalek 的批量系数逐字节一致是共识要求 |
| bincode | 1.3 | 3.0 | 不升：编码格式与 API 都变，涉及持久化与线上格式 |
| sp1-sdk / sp1-zkvm | 4.0 | 6.10 | 不升：zkproof 为 mock 后端，guest 已排除出 workspace，需单独规划 |
| sha2（n42-zkproof-guest） | 0.10 | 0.11 | 不升：guest 用 SP1 工具链构建，不在 workspace |

## 验证

- `cargo check --all-targets`、`cargo clippy --all-targets -- -D warnings` 通过。
- `cargo test --workspace`：72 个测试二进制，2220 通过、0 失败、42 ignored。
- `evm2-jit` 的 "patch not used" 警告在更新前就存在（锁文件 `[[patch.unused]]` 相同），与本次无关。

## 后续

- #56 合并后本分支合 main 再跑 CI。
- rand_core 0.6 要等 merlin 支持新版，或自己实现 transcript RNG 并用现有向量证明逐字节一致。
- bincode 3 / sp1 6 需单独评估格式兼容与证明系统迁移。
