# Workspace 覆盖率与 70% 检查（2026-09-23）

## 范围与结果

默认主机、默认 feature 的 Rust workspace 源文件行覆盖率实测 **73.0631%（59,157 / 80,967）**。统计保留所有 `crates/*/src` 和 `bin/*/src`，包括零覆盖率工具；按 LLVM 源文件口径包含内联测试，排除第三方依赖、集成/E2E 测试工具的源码。不是每个 crate 单独达到 70%，也不是所有语言或所有平台的覆盖率。完整定义见 [testing-coverage.md](testing-coverage.md)。

初始源文件口径报告为 72.8108%；新增行为测试后超过 73%。最终测量使用 `codegen-units=1` 和新的采样目录，因此以最终绝对值为验收依据，不把两次百分比的全部差额归因于测试。

## 实施

- 观察者新增 9 个测试：祖先链环路、区块编号溢出、发现完成后不再追加、谱系内存上限、完整且已执行的连续祖先链、认证链执行与提升、请求去重/过期重试、发送失败不占重试窗口、在途请求上限。
- 交易入口新增 4 个测试：额度失效、截止时刻边界、待处理数量到达/超过上限时的饱和减法、旧协议软上限。
- `scripts/workspace-coverage.py` 使用 Cargo 构建产物列表收集当前运行对象，用新的 profile 目录统计加权行覆盖率。不会清空调用者的 target 目录，失败测试不能被高覆盖率抵消。
- 门槛工具新增 7 个测试，验证边界百分比、权重、构建/测试失败、空数据、重复文件和产物选择。
- 新增 GitHub Actions 工作流，配置 Rust LLVM 工具、固定 reth 基线与补丁、70% 门槛及报告上传。工作流尚未在远端实际执行。

## 环境处理

原 `target` 是指向只读目录的符号链接，Cargo 缓存也只读；使用 `.artifacts/coverage-cargo-home`、`.artifacts/coverage-target` 和独立 check target。当前待提交源码需要仓库已有的 reth import-batch 补丁，但原兄弟 reth 目录未应用且不可写，因此在 `.artifacts/coverage-build/` 中复制源码和 reth，再应用现有补丁。最终逐字节核对全部 Rust 源文件与原工作区一致。未降级依赖或改动原 reth 目录。

当前待提交的 `n42-node` manifest 含 `rayon`，lockfile 缺其依赖列表项。离线 Cargo 只补入该项，其余已有 lockfile 改动保留。

## 验证与阻塞

- `cargo check --workspace --all-targets --offline --locked`：通过（使用已打补丁的构建副本）。
- 观察者测试：25 通过；Python `test_*.py`：21 通过；新增 13 个 Rust 测试均通过。
- 完整 workspace：**1,710 通过、7 失败、26 忽略**。失败均在监听本地端口时得到沙箱 `Operation not permitted`，未删除或跳过这些测试。
- 失败测试：E2E RPC client 的 `test_get_transaction_receipt_none_for_null_result`、`test_get_transaction_receipt_propagates_rpc_error`、`test_wait_for_receipt_retries_until_result_available`；network transport 的 `tunes_owned_udp_socket_buffers`、`interop_observer_completes_tcp_noise_yamux_handshake`；node ingest 的 `oversized_batch_header_is_rejected_before_allocating`、`batch_over_byte_ceiling_drops_the_connection`。
- LLVM 正常导出成功，但报告 10 个 mobile FFI 的未改名 C 导出函数存在 profile hash 不匹配；保留该诊断限制。额外的 `llvm-cov --dump` 调试调用打印函数名后崩溃，正常导出路径未崩溃。
- 门槛退出 **2**，整体验收未通过。需要在允许本地端口监听的环境中重跑，并核实匹配工具链的 LLVM 对 FFI 映射警告的处理。

本机报告：`.artifacts/coverage-final/run-5vx2sv4c/summary.txt`；同目录保留 `coverage.json`、`tests.log`、`failures.txt`、`llvm-diagnostics.log` 和 `unmapped-sources.json`。运行方式遵循 Rust 官方的 [instrumentation-based coverage](https://doc.rust-lang.org/rustc/instrument-coverage.html)。
