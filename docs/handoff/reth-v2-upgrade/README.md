# reth v1.11.3 → v2.2.0 升级接力包

> Handoff target: Mac + OpenAI Codex (or any Linux/macOS coding assistant)
> Author: Claude (Windows session, 2026-05-13)
> Stage: 2 of 2 — Stage 1 已合并 (workspace alloy 对齐本地 reth + 4 clippy 修复 + 937 测试通过)

## 这个目录里有什么

```
docs/handoff/reth-v2-upgrade/
├── README.md                       ← 你正在读
├── 00-context.md                   ← 完整背景 + 阶段 1 已做了什么 + 我撞到的卡点
├── 01-prerequisites.md             ← Mac 环境准备 + 拉代码
├── 02-apply-stage2-patches.md      ← 按顺序 apply 4 个 n42-26 + 1 个 reth roaring patch
├── 03-rebase-n42-patches-onto-v2.md ← 最难的部分: 705 行 reth-n42.patch + 8 个本地 mod 在 v2.2.0 上重新落地
├── 04-known-fixups.md              ← 已经定位但未实施的修复点（你要做的）
├── 05-verify-and-handback.md       ← cargo check / clippy / test / e2e + 回传成果给我的格式
└── patches/                        ← 所有可机器 apply 的 patch
    ├── stage2-workspace-deps.patch          ← Cargo.toml: alloy-evm 0.34, revm 38, alloy-* 2.0.4
    ├── stage2-n42-execution.patch           ← n42-execution 三处适配
    ├── stage2-n42-parallel-evm.patch        ← tx_gas_used 替换 deprecated gas_used
    ├── stage2-reth-roaring-bump.patch       ← reth/Cargo.toml: roaring 0.11.3 → 0.11.4
    ├── reth-n42.patch                       ← 705 行原始 N42 hook 集（基于 27781443a，要 rebase）
    ├── reth-local-mods-pre-upgrade.patch    ← 625 行 reth 本地 8 个 modified files
    └── reth-payload-cache-pre-upgrade.rs    ← reth 新增 untracked 文件 (74 行)
```

## 30 秒摘要

阶段 2 目标：把 `D:\N42\reth`（path 依赖）从 1.11.3 升到 v2.2.0，同步 bump 所有 alloy / revm。

我在 Windows 上已经做完**纯代码适配**（4 个 patch 已生成，n42-26 自家代码完全 OK），但**卡在 reth v2.2.0 上游本身的 Windows 不兼容 bug**——`crates/static-file/types/src/changeset_offsets.rs` 无条件 `use std::os::unix::fs::FileExt`，`provider/static_file/{writer,mod}.rs` 又无条件用 `ChangesetOffsetReader/Writer` 类型，Windows 上 11 个 unresolved import。

CI 是 `ubuntu-latest`，Linux 上应该不会撞到这个 bug。所以接力到 Mac/Linux 是正确路径。

## 这个接力包给你（Codex）的承诺

1. **所有需要 apply 的 patch 都已物化**为 git apply 可用的文件，按顺序在 `02-apply-stage2-patches.md` 列出。
2. **每个 patch 的语义**在文档里写清楚了——遇到 conflict 你知道我当初为什么这么改、可以怎么调。
3. **唯一开放的工作**是 03 文档里的 reth path rebase（705 行 hook 怎么在 v2.2.0 新结构上找到对应位置），和 04 里的"需要你实施的修复"。
4. **验证清单**在 05，跑完 cargo + e2e 就能确认升级落地。
5. **回传格式**在 05 末尾——如果遇到无法解决的问题，按那个格式把 log 截给我，我接下个班。

## 我留下的两个独立产出（与本升级无关，但你要知道）

- `docs/rfc/production-safe-deferred-state-root.md` — Phase B ZK sidecar 替代 N42_DEFER_STATE_ROOT 的 RFC
- `docs/rfc/reth-v2-upgrade.md` — 早期版本的升级 RFC，**本目录文档优先**（更详细、更新）

## 先读哪个

按顺序 `00 → 01 → 02 → 03 → 04 → 05`。如果时间紧，至少先读 `00-context.md` 的"卡点"小节和本 README。
