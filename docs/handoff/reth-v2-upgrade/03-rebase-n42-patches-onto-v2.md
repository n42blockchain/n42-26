# 03 — 把 reth 切到 v2.2.0 + rebase N42 hooks

这是整个升级**最难的一步**。我在 Windows 上没做这步（因为后续 build 被 Windows compat bug 卡住，我决定保留 1.11.3 base 不动）。所以这里写的是规划，你需要根据实际 conflict 手动处理。

## 备份当前状态

```bash
cd ~/N42/reth
# 当前 working tree 已经 apply 了 reth-local-mods-pre-upgrade.patch + payload_cache.rs + roaring bump
# 给个 branch 名固定一下
git checkout -b n42-stage1-baseline
git add -A && git commit -m "n42 stage 1 baseline: 27781443a + n42 hooks + roaring 0.11.4"
```

现在 `n42-stage1-baseline` 分支是你的回滚点。

## 切到 v2.2.0

```bash
cd ~/N42/reth
git fetch origin --tags
git checkout -b n42-v2-upgrade v2.2.0
```

应用 roaring 修复（v2.2.0 tag 上漏了）：
```bash
git apply ~/N42/n42-26/docs/handoff/reth-v2-upgrade/patches/stage2-reth-roaring-bump.patch
```

## Apply 705 行 N42 patch

```bash
git apply --3way ~/N42/n42-26/docs/handoff/reth-v2-upgrade/patches/reth-n42.patch
```

**几乎必然 conflict**。reth v1 → v2 改了大量内部 API，N42 hook 的注入点会变。下面是每个 hook 块的语义，conflict 时按这个找 v2.2.0 新位置。

### N42 hook 清单（按 patch 顺序）

| Hook | 旧文件:位置 | 语义 | v2.2.0 找新位置的关键词 |
|---|---|---|---|
| `n42_skip_state_root()` + `n42_defer_state_root()` 公开 helpers | `crates/evm/evm/src/lib.rs` | 暴露 env-var 读取（benchmark mode） | 同文件，找 `pub fn` 顶层位置 |
| State root 跳过门控 | `crates/engine/tree/src/tree/payload_validator.rs` | 在 `compute_state_root` 调用前判断 env var 跳过 | grep `compute_state_root`、`state_root` |
| State root 跳过门控 v2 | `crates/evm/evm/src/execute.rs` | 同上但在 executor 内部 | grep `fn state_root\|StateRootTask` |
| Fast path: skip post-validation | `crates/engine/tree/src/tree/payload_validator.rs` | cache hit + skip root 时跳过 receipts/blob 后置验证 | grep `validate_block_post_execution\|validate_block_inner` |
| Leader broadcast cache | `crates/ethereum/payload/src/lib.rs` | leader build 时把 (BundleState, senders) 写入 BROADCAST_CACHE | grep `default_ethereum_payload\|EthBuiltPayload::new` |
| Payload cache 双槽 | `crates/evm/evm/src/payload_cache.rs` (新文件) | CACHE + BROADCAST_CACHE 双槽设计 | 整个文件 copy 进去即可，但要检查 reth v2 是否已有冲突路径 |
| Cargo deps for payload_cache | `crates/evm/evm/Cargo.toml` | 加 lru / once_cell / dashmap 等依赖 | 同文件，与 v2 现有 deps 合并 |
| fs-util tweaks | `crates/fs-util/src/lib.rs` | 文件 sync_all 优化 | grep `fn write_atomic\|sync_all` |
| RPC eth utils | `crates/rpc/rpc-eth-types/src/utils.rs` | trace utility（非关键） | 可能 v2 已重构，必要时跳过 |

### Conflict 处理策略

对每个 hunk:

1. 如果 v2.2.0 对应函数还在、签名一致：3-way merge 自动解决；
2. 如果函数被重命名：grep 新名字，手动放到新位置；
3. 如果函数被拆分/重构：理解 hook 语义后在新结构里找最贴近的注入点；
4. 如果整个 module 被删/重写：（不太可能，但需要时）考虑这个 hook 是否还需要，或要不要在 N42 端绕过。

**重要原则**：所有这些 hook **都是 opt-in env var 守门的**（`N42_SKIP_STATE_ROOT`, `N42_DEFER_STATE_ROOT`, `N42_BROADCAST_CACHE_ENABLED` 等），所以即使你"轻度"应用——只保留核心的 BROADCAST_CACHE，剩下的暂时不接入——也不会破坏正常路径。生产默认 env 全不设。

### 应用 reth 本地的 8 个 modified files (v1 内容)

```bash
git apply --3way ~/N42/n42-26/docs/handoff/reth-v2-upgrade/patches/reth-local-mods-pre-upgrade.patch
```

这个 patch 与 `reth-n42.patch` 有部分重叠（前者是 working tree 在 27781443a 上对 reth-n42 patch 已应用后的差异），用 3-way merge 处理。

> **替代方案**：如果你觉得 patch 太碎，可以从 `n42-stage1-baseline` 分支 cherry-pick 一个 commit 来代替这 705 行 patch + 625 行 modified files patch 的组合。但 cherry-pick 跨 v1→v2 conflict 一样多，没本质区别。

### 复制 payload_cache.rs

```bash
cp ~/N42/n42-26/docs/handoff/reth-v2-upgrade/patches/reth-payload-cache-pre-upgrade.rs \
   ~/N42/reth/crates/evm/evm/src/payload_cache.rs
```

然后**确保**`crates/evm/evm/src/lib.rs` 在 v2.2.0 上 `pub mod payload_cache;` 这行存在（reth-n42.patch 会加上，但如果 conflict 没合好需要补）。

## 此时尝试编译 reth 自身

```bash
cd ~/N42/reth
cargo check -p reth-evm -p reth-engine-tree -p reth-ethereum-payload-builder 2>&1 | tail -30
```

期望：通过。如果 fail：
- 看错误是否在你 hook 的位置 → 修 hook
- 看错误是否在 reth 内部其他位置 → 可能是漏 apply 某段 patch，或者 reth v2 内部依赖了 N42 patch 修改过的某个旧 API；按错误信息逐个修

## 触发 cargo update 让 lock 重新解析

```bash
cd ~/N42/n42-26
rm Cargo.lock
cargo check -p n42-primitives  # 让 cargo 重新生成 lock
```

新 lock 应该选 alloy-evm 0.34 / revm 38 / alloy-* 2.0.4。验证：
```bash
grep -A1 '^name = "alloy-evm"$' Cargo.lock | head -4
grep -A1 '^name = "revm"$' Cargo.lock | head -4
# 应只出现 0.34.0 和 38.0.0，没有 0.29.2 / 36.0.0
```

## 接下来读 `04-known-fixups.md`
