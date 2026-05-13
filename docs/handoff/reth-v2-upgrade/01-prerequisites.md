# 01 — Mac 环境准备

## 工具链

```bash
# Rust 1.93+ (项目要求 edition 2024)
rustup install 1.93
rustup default 1.93
rustup component add clippy rustfmt

# Git
git --version  # 任何近代版本都行

# 编译时偶尔会用到（reth 部分依赖）
brew install pkg-config cmake protobuf  # 如果某个 native dep 报错再装

# 可选 — 跑 e2e 时用
brew install docker
```

确认：
```bash
cargo --version  # ≥ 1.93
rustc --version  # ≥ 1.93
```

## 拉代码

两个 repo 必须放在**同一父目录**，因为 n42-26 用 `path = "../reth"`。

```bash
mkdir -p ~/N42 && cd ~/N42

# 1. 拉 n42-26
git clone <your-n42-26-remote-url> n42-26
cd n42-26
# 确认包含 handoff 目录:
ls docs/handoff/reth-v2-upgrade/
# 应看到 README.md, 00..05.md, patches/ 子目录

# 2. 拉 reth
cd ..
git clone https://github.com/paradigmxyz/reth.git
# 不要在 ~/N42/n42-26 里面拉 reth；reth 必须是 ~/N42/n42-26 的同级 sibling
```

## 同步 n42-26 working tree 到 Windows 端"阶段 1 + 主路径优化已完成"的状态

Windows 端的改动还**没 commit**。要在 Mac 上接力，需要先把这些改动同步过来。两种方式选一个：

### 方式 A — 推一个临时分支到远程，Mac 上拉（推荐）

我已在 Windows 上让用户 `git push` 一个分支带这些 working tree 改动：

```bash
# 在 Mac 上
cd ~/N42/n42-26
git fetch origin
git checkout -b stage2-handoff origin/stage2-handoff  # 分支名以用户实际推的为准
```

### 方式 B — 用我已生成的 baseline patch（不需要 Windows 端再操作）

我已经把 Windows 端 working tree 的 tracked-file diff 导成 patch：

```
docs/handoff/reth-v2-upgrade/patches/stage1-and-mainpath-tracked-baseline.patch  (727 行)
```

这份 patch 包含了：
- **阶段 1 编译修复**：workspace alloy-evm 0.30→0.29.2 + 4 个 clippy lint 修
- **主路径优化**：6 个 metric + 4 个 tracing span + mv_memory clear_tx 索引化 + transport mesh env override + docs 修正

untracked 部分（docs/rfc/、docs/handoff/、.mcp.json）会随 git clone 一起拉到 Mac（因为它们已经在 handoff 目录里，commit 后跟着分支走），不需要单独传输。

在 Mac 上 apply：
```bash
cd ~/N42/n42-26
git checkout dev2603              # 或你拉到的对应 base 分支
git apply docs/handoff/reth-v2-upgrade/patches/stage1-and-mainpath-tracked-baseline.patch
# 然后 git status 应该看到 11 个 modified files (Cargo.lock, Cargo.toml + 8 crate files + 2 docs)
```

> **注意**：方式 A（commit + push）是首选——历史更清晰，回滚更容易。方式 B 适合用户暂时不想 commit "wip" 提交的场景。

> **注意**：Windows 端的 docs/handoff/ 已经包含所有阶段 2 patch + reth-n42.patch + reth 本地 mods patch。但 n42-26 的"已修改但未 commit"的代码（阶段 1 + 主路径优化）**不在这些 patch 文件里**。Mac 上要先有这个 baseline 才能继续 apply 阶段 2。

### 推荐：方式 A，让用户先在 Windows 上 commit & push

最干净的做法：让用户在 Windows 端跑（**新分支，不是 dev2603**）：

```powershell
cd D:\N42\n42-26
git checkout -b stage2-handoff
git add -A
$env:GIT_COMMITTER_NAME="Nyxen"
$env:GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com"
git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" `
    -m "wip: stage 1 + main-path optimizations + stage 2 patches (handoff to mac/codex)"
git push -u origin stage2-handoff
```

然后 Mac 上：
```bash
cd ~/N42/n42-26
git fetch origin
git checkout stage2-handoff
```

## reth 端的同步

`~/N42/reth` 需要从 27781443a 开始，并应用 reth 本地 8 个 modified + 1 个 untracked 文件（这些就是 N42 hooks 的当前形态）。

```bash
cd ~/N42/reth

# checkout 到我用的 base commit
git checkout 27781443a6e6e71c93bbbe05012f0ac9595f9dac

# apply 本地 8 个 modified files
git apply ~/N42/n42-26/docs/handoff/reth-v2-upgrade/patches/reth-local-mods-pre-upgrade.patch

# 复制新增的 payload_cache.rs 到正确位置
cp ~/N42/n42-26/docs/handoff/reth-v2-upgrade/patches/reth-payload-cache-pre-upgrade.rs \
   crates/evm/evm/src/payload_cache.rs

# 检查 status，应与 Windows 端一致：
git status --short
# Expected:
# M Cargo.lock
# M Cargo.toml
# M crates/engine/tree/src/tree/payload_validator.rs
# M crates/ethereum/payload/src/lib.rs
# M crates/evm/evm/Cargo.toml
# M crates/evm/evm/src/execute.rs
# M crates/evm/evm/src/lib.rs
# M crates/fs-util/src/lib.rs
# M crates/rpc/rpc-eth-types/src/utils.rs
# ?? crates/evm/evm/src/payload_cache.rs
```

## Sanity check — 阶段 1 在 Mac 上能编译吗？

```bash
cd ~/N42/n42-26
cargo check -p n42-execution -p n42-consensus -p n42-node -p n42-network \
            -p n42-jmt -p n42-parallel-evm -p n42-primitives -p n42-chainspec -p n42-mobile
```

期望：全部 `Checking ...` → `Finished` 无 error。

跑测试确认 baseline：
```bash
cargo test --lib \
    -p n42-execution -p n42-consensus -p n42-node -p n42-network \
    -p n42-jmt -p n42-parallel-evm -p n42-primitives -p n42-chainspec
```

期望：937/937 passed（如果你拉的是 stage2-handoff 分支带主路径优化的话）。

## 接下来读 `02-apply-stage2-patches.md`
