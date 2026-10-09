# Rust workspace 覆盖率

在仓库根目录运行：

```bash
rustup component add llvm-tools-preview
bash scripts/apply-reth-patches.sh ../reth
python3 scripts/workspace-coverage.py
```

要求 Rust 1.97.1、Python 3、与 rustc 内置 LLVM 主版本匹配的 `llvm-cov` / `llvm-profdata`，以及允许测试监听本地 TCP/UDP 端口的环境。脚本依次查找 `LLVM_COV` / `LLVM_PROFDATA` 环境变量、Rust 工具链中的工具、系统带版本后缀和不带版本后缀的工具。

默认验收条件：**workspace 源文件行覆盖率至少 70%，且全部 Cargo 测试通过**。执行使用 `--locked`，不会自动修改依赖版本。低于门槛退出 1；测试失败、构建失败或缺少有效覆盖率数据退出 2。测试失败但已有完整构建和采样数据时仍输出诊断报告，门槛保持失败。

## 统计范围

- 全部 Rust workspace 成员中的 `crates/*/src` 和 `bin/*/src`，包含低覆盖率模块、节点启动程序和压力测试工具，不按覆盖率高低排除文件。
- 采用 LLVM 的源文件行统计，**包含源文件中的 `#[cfg(test)]` 内联测试代码**；不是剔除所有测试代码后的纯生产逻辑覆盖率，也不是分支覆盖率。
- 所有 workspace 单元测试和集成测试都运行；集成测试、E2E 测试工具本身的源码以及第三方依赖不进入覆盖率分母。
- 使用当前主机和默认 feature。SP1 guest、Android/iOS、Solidity 合约、Python/Shell 运维脚本，以及 `jit` / `sp1` 等未启用 feature 不在此数值内。此门槛不能代表这些独立工具链已达到 70%。
- 每次输出 `unmapped-sources.json` 列出没有 LLVM 覆盖映射的源码，例如仅导出模块的文件、目标平台专有代码和未启用 feature；这类文件不能被解释为已经覆盖。
- 汇总计算为 `所有文件已覆盖行数之和 / 所有文件可执行行数之和`；70% 是 workspace 总体门槛，不要求每个 crate 都达到 70%。

## 输出与复现

每次运行创建独立的 `.artifacts/coverage/run-*`，避免复用旧计数。目录包含 `summary.txt`（总体和逐 crate 结果）、`coverage.json`（逐文件和函数数据）、`tests.log`、`coverage.profdata` 和原始采样文件。编译错误打印到 stderr。

```bash
# 已有缓存时离线执行
python3 scripts/workspace-coverage.py --offline

# 自定义可写路径，适用于默认 target/cache 不可写的环境
CARGO_HOME=/path/to/writable/cargo-cache \
CARGO_TARGET_DIR=/path/to/writable/build \
python3 scripts/workspace-coverage.py --output-dir /path/to/reports

# 门槛工具自身的测试
python3 -m unittest discover -s scripts -p test_workspace_coverage.py
```

`--manifest-path` 支持测量另一个 checkout；其源码、lockfile、工具链及 reth 补丁必须与待验收版本一致。`--min-lines` 可用于本地诊断，CI 固定使用默认的 70%。工具通过 `RUSTC_WORKSPACE_WRAPPER` 仅插桩 workspace 成员，不替换已有同名 wrapper，也不清空用户指定的构建目录。

GitHub Actions 的 `Rust workspace coverage` 工作流在 PR、main/develop push 和手动运行时执行门槛，并上传报告。更高百分比不能抵消测试失败。
