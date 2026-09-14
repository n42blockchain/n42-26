# CI reth transaction-root cache API repair

日期：2026-09-14

## 原因

三个 E2E Correctness 检查在编译 `n42-node` 时均触发 E0425：
`remove_payload_transactions_root`、`payload_transactions_root`、
`store_payload_transactions_root` 不存在于 CI checkout 的 `reth_evm::payload_cache`。
本机 reth 未提交修改包含这些 API，因此直接使用本机依赖的 check 无法复现。

## 修复

- 固定 CI reth 为 `chore/reth-upstream-20260804` 的
  `23316e3ff8adca8c3bd5085ff0565fcae019202a`，保持 Alloy 2.3 / REVM 42 基线。
- 新增 `patches/reth-payload-transactions-root.patch`，提供三个缓存 API，并在
  payload builder 中记录已构建区块的交易根，供 compact 输出序列化使用。
- E2E、nightly 和 Docker 构建统一调用 `scripts/apply-reth-patches.sh`。
  脚本支持重复运行，遇到不匹配的源码会在写入前失败。
- 补丁只包含缓存 API 和生产端写入，不包含本机额外的并行交易解码与交易根复用
  validator 改动；相应移除 lockfile 中该 payload crate 的额外 rayon 依赖。
  构建命令使用 `--locked`，防止 CI 静默改写依赖图。
- Docker 补齐 workspace 成员 `tests/e2e`；README 更新准备依赖的步骤。
- 增加 compact 序列化、注入、按 hash 清除交易根的回归测试。

## 验证

使用 `/tmp` 中的干净 reth 基线和 N42 副本，与本机未提交依赖修改隔离。
补丁前复现同样的三个 E0425；补丁后 `cargo check --all-targets --locked --offline`
通过。脚本重复应用、冲突时无部分写入，以及 workflow YAML 解析均通过。

`cargo clippy --all-targets --locked --offline -- -D warnings` 通过。
`cargo test --workspace --locked --offline` 完成全部测试目标编译；执行阶段在
`e2e-test` 的三个 RPC 测试处失败，原因是沙箱禁止 `TcpListener::bind("127.0.0.1:0")`
（`Operation not permitted`），因此不能声称全量测试或真实节点 E2E 已通过。
GitHub Actions 上的完整 E2E 和 Docker 镜像构建仍需重跑验证。

单独运行 `cargo test --locked --offline -p n42-node --lib
transaction_root_survives_compact_round_trip_and_is_evicted_by_hash` 通过（1 passed）。
