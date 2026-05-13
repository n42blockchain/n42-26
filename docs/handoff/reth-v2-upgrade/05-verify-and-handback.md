# 05 — 验证 + 回传成果

## 验证清单

### 1. 全 workspace 编译

```bash
cd ~/N42/n42-26
cargo check --all-targets 2>&1 | tail -20
```

**期望**：`Finished`，零 error。

如果 tikv-jemalloc-sys 在 Mac 上也报错（罕见），临时排除 n42-node-bin：
```bash
cargo check -p n42-execution -p n42-consensus -p n42-node -p n42-network \
            -p n42-jmt -p n42-parallel-evm -p n42-primitives -p n42-chainspec \
            -p n42-mobile -p n42-mobile-ffi
```

### 2. Clippy 零警告

```bash
cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
```

**期望**：`Finished`。所有 N42 自家 crate 零 warning（reth path 内的 warning 可忽略）。

> 如果新出现 N42 自家代码的 clippy lint（Rust 1.93 + clippy 新规则），按 `04-known-fixups.md` L 节处理。

### 3. 单元测试

```bash
cargo test --workspace --lib 2>&1 | grep "^test result:"
```

**期望**（参考 stage1 baseline）：
- n42-chainspec: 31
- n42-jmt: 181
- n42-primitives: 91
- n42-consensus: 64
- n42-network: 117
- n42-node: 206
- n42-parallel-evm: 2（如果加了 `clear_tx` 索引化的测试，可能更多）
- n42-execution: 39
- n42-mobile: ~28

总计 **937+**。如果 < 937，看是哪个 crate 失败，对照 baseline 找差异。

### 4. n42-consensus 集成测试

```bash
cargo test -p n42-consensus --test integration_test
```

**期望**：通过。fault_tolerance / performance_bench 模块都跑。

### 5. release build（重 — 不要在大压力下做）

```bash
cargo build --release -p n42-node-bin -p e2e-test 2>&1 | tail -10
```

**期望**：成功生成 `target/release/n42-node` 和 `target/release/e2e-test`。

### 6. E2E smoke (1, 3, 4)

```bash
E2E_SCENARIO_FILTER=1,3,4 target/release/e2e-test --binary target/release/n42-node 2>&1 | tail -30
```

**期望**：3 个场景全通过。1 = 单节点启动，3 = ERC-20 合约，4 = 多节点共识。

### 7. (可选) Mobile-rpc E2E (5, 8, 12)

```bash
E2E_SCENARIO_FILTER=5,8,12 target/release/e2e-test --binary target/release/n42-node 2>&1 | tail -30
```

### 8. 7-node testnet 30 min 压测（可选，但合并前建议跑）

```bash
./scripts/testnet.sh --nodes 7 --clean &
# 等 30 秒启动稳定
./scripts/step_stress.sh
# 跑 30 min 后用 Ctrl+C 终止 testnet.sh
```

观察关键 metric（前一会话已有 baseline）：
- `n42_consensus_commit_latency_ms` p50 应 < 500ms（与 stage1 持平或更好）
- `n42_state_root_apply_diff_ms` p99 < 200ms
- TPS 不退化 > 10% vs stage1 baseline

## 提交 & 回传

### 全通过的情况

```bash
cd ~/N42/n42-26
git add -A
git commit -m "feat: upgrade reth to v2.2.0 + alloy-evm 0.34 + revm 38

- workspace: bump alloy-evm 0.29.2 → 0.34.0, revm 36.0.0 → 38.0.0,
  alloy-consensus/eips/genesis/rpc-types-engine/signer/signer-local/
  network/rpc-types-eth 1.8.2 → 2.0.4, reth-primitives-traits 0.1.0 → 0.3.1
- n42-execution: adopt EthEvmBuilder pattern; adapt PrecompileFn to EIP-8037
  3-arg signature; route OutOfGas / Other through PrecompileHalt; pass
  ExecutionWitnessMode::Legacy to ExecutionWitnessRecord::from_executed_state
- n42-parallel-evm: replace deprecated gas_used() with tx_gas_used()
- reth path: rebased to v2.2.0 (was 1.11.3 @ 27781443a) + 705-line N42 hook
  set + 625-line local mods + payload_cache.rs reapplied on top
- reth Cargo.toml: pin roaring 0.11.4 (works around v2.2.0 tag base bug
  where IntegerList derives Eq but 0.11.3 doesn't impl it)

Verified on macOS:
  cargo check --all-targets ✓
  cargo clippy --all-targets -- -D warnings ✓
  cargo test --workspace --lib → <NNN>/<NNN> pass
  E2E scenarios 1, 3, 4 ✓
"

# 提交 reth 这边
cd ~/N42/reth
git add -A
git commit -m "n42: reth v2.2.0 base + N42 hooks rebased + roaring 0.11.4"

# 推到远端
cd ~/N42/n42-26
git push -u origin <branch-name>
cd ~/N42/reth
# 如果 reth 是 fork，push 到你的 fork；如果是 readonly 上游 clone，需要先 fork
git push -u <fork-remote> <branch-name>
```

把 `n42-26` 上的 branch 名 + reth fork 上的 branch 名告诉我。

### 不全通过的情况 — 给我的回传格式

如果你卡住，请按这个格式回我（在 Claude 这边）。**只贴关键 log，不要贴几千行**。

```
## 卡点位置
<file:line> 或 <step in 04-known-fixups.md>

## 出错命令
$ cargo check -p <crate>

## 错误前 20 行 + 错误后 20 行
<paste>

## 我尝试过
- 改了 X 文件的 Y 行 (具体 diff)
- 跑了 cargo update / clean / 等等

## 现在状态
- n42-26 git status / branch
- reth git status / branch
```

我会基于这个继续推进或调整方案。

### 跳过我的回传，自己继续

完全可以。本 handoff 包是参考，不是脚本。Codex 上你做的决策也算数——只是合并到 main 前回个完整结果给我（让我知道现状）。

---

## 余下没写的小事（如果你完成主升级后还有精力）

1. **重新生成 patches**：升级落地后，把"reth-n42 在 v2.2.0 base 上的 patch"重新 export 一份，更新到 `D:\N42\n42-26\reth-n42.patch`（仓库根目录那个），覆盖旧的 705 行版。这样未来重新打补丁就基于 v2.2.0 而不是 27781443a 了。

2. **`docs/01-System-Architecture.md` 末尾 "Open issues #3"** 提到的"reth path 依赖与 workspace 在 `alloy-evm` 版本上不一致"已不再适用，可以删掉那条。

3. **`docs/rfc/reth-v2-upgrade.md`** "Stage 2 Attempt Notes" 章节可以收尾——把"受阻"改写成"已完成 by codex on YYYY-MM-DD"。

4. **`docs/rfc/production-safe-deferred-state-root.md`** 不受本升级影响，无需改。

---

完了。祝顺利。
