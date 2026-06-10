# Codex 任务:SBMT 状态树 proof 端到端验证(在 macOS 执行)

> 这份文档是给 **macOS 上的 OpenAI Codex** 的任务说明。Windows 开发机跑不了本地测试网
> (jemalloc C 构建 + 测试网脚本需 unix),所以把 JMT→SBMT 切换的 E2E 验证交给 mac 完成。
>
> **提交规范**:所有 git 提交**不要包含 "Claude" / "Codex" / "Co-Authored-By" 等 AI 署名**。
> 用项目模板作者:
> ```
> GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" \
>   git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" -m "..."
> ```

## 1. 背景(你需要知道的)

最近把 n42 的**并行状态树**从 16 叉 JMT(`ShardedJmt`,jmt 0.12 库)切换到**自研二叉 SBMT**
(`ShardedSbmt`,sparse binary merkle tree,纯 blake3)。完整设计与 7 个阶段记录在
**`docs/devlog-59-jmt-to-bmt-sbmt-phase1.md`**,请先读它。要点:

- **状态树与共识解耦**:真正的 state root 由 reth EVM 算并写入 block header;SBMT 是异步后台
  维护的并行树,**只服务 RPC 给手机钱包的账户 proof**,不参与 BFT。所以验证它**不影响共识安全**。
- SBMT 引擎/proof 验证抽到了零依赖 crate **`n42-bmt-core`**(只依赖 blake3/serde/thiserror,
  无 reth/mdbx),手机端 `n42-mobile::state_proof::verify_state_proof` 用它纯 blake3 验证。
- 节点侧:`N42_JMT=1` 启用 SBMT(`bin/n42-node/src/main.rs`),RPC namespace `n42`。

**本任务目标**:在 mac 上跑起带 SBMT 的节点,确认 `n42_jmtProof` 返回的 `ShardedBmtProof`
能被 `n42-bmt-core` 验证通过(inclusion + exclusion),且篡改会被拒。这是 SBMT 上生产前的唯一
未验证环节。

## 2. 前置

### ⛔ 步骤 0（必做，否则会破坏分支）：确认 `../reth` 是 **reth main 合并版**

本分支 `chore/merge-reth-main-deps-upgrade` 的依赖 pin 对齐 **reth main**
（`revm 40.0.3 / alloy-evm 0.35.0 / reth-primitives-traits 0.4.0`）。`../reth` 有一个**旧基线**
`n42-v2-upgrade`（reth v2.2.0，对应 revm 38 / alloy-evm 0.34 / reth-primitives-traits 0.3.1）。
**用错基线会导致编译失败，且绝不能为了让旧 reth 编过而降级 `Cargo.toml`** —— 那会推翻本分支的
deps upgrade（上一轮就因此返工，见 §Maintainer note in devlog-60）。

```bash
# 1) 确认 reth 在 main 合并版（HEAD 应是 reth main 合并 + N42 hooks，不是 v2.2.0 base）
git -C ../reth log -1 --oneline
git -C ../reth branch --show-current
# 2) 自检：以下版本必须保持原样，不得改动
grep -E "alloy-evm =|revm =|reth-primitives-traits =" Cargo.toml
#   预期：alloy-evm 0.35.0 / revm 40.0.3 / reth-primitives-traits 0.4.0
```

如果 `../reth` 只有旧的 `n42-v2-upgrade`，**先把 reth fork 切到/拉取 reth main 合并版再继续**；
不要修改 n42-26 的依赖去迁就旧 reth。完成后 `git diff Cargo.toml` 必须为空。

### 其余前置

1. 同步代码:分支 **`chore/merge-reth-main-deps-upgrade`**(`git pull`,HEAD 应含 commit
   `2e13906` "revert reth v2.2.0 downgrade ... realign to reth main" 或更新)。
2. 构建:`cargo build --release -p n42-node-bin`(mac 原生编译;Windows 编不过的 jemalloc/
   mdbx 在 mac 上正常)。先确认 `cargo test -p n42-bmt-core -p n42-jmt` 全绿。
3. **收尾自检**:E2E 跑完提交前,再次 `git diff Cargo.toml`(必须为空)+
   `git diff -- crates/n42-parallel-evm crates/n42-consensus crates/n42-execution`(不应有
   revm/alloy API 回退式改动)。

## 3. 关键技术点(已确认)

| 项 | 值 |
|----|----|
| 启用 SBMT | 节点进程环境变量 **`N42_JMT=1`** |
| RPC namespace | `n42`(jsonrpsee `#[rpc(server, namespace = "n42")]`,`crates/n42-node/src/rpc.rs:154`) |
| 方法 | `n42_jmtRoot` → `{version, root}`;`n42_jmtProof(address, storageSlot?)` → 见下;`n42_jmtVersion` |
| jmtProof 响应 | `JmtProofResponse { shardIndex, keyHash, value, proofHex, shardRoots, root }`,**camelCase** |
| **proofHex** | **`hex(bincode(ShardedBmtProof))`** —— 解码后用 `n42-bmt-core` 验证 |
| 验证 | `ShardedBmtProof::verify(&combined_root_bytes)` 或 `n42_mobile::state_proof::verify_state_proof(&proof, state_root_b256)` |

**需要你在 mac 上确认的不确定点**:
- `n42` namespace 是否默认挂到 HTTP RPC。`scripts/testnet.sh` 的 `--http.api` 列表是
  `eth,net,web3,txpool,rpc`(不含 `n42`),但 RPC 经 reth `extend_rpc_modules` →
  `merge_configured`(`bin/n42-node/src/main.rs:699-712`)注册,通常对所有 transport 生效。
  用 `curl` 试 `n42_jmtVersion` 确认可达;若不可达,检查是否需要把 `n42` 加进 `--http.api` 或
  设 `N42_ENABLE_HTTP_RPC=1`。
- `scripts/testnet.sh` 启动节点的命令(约 `557-595` 行)用环境变量前缀传参给 `target/release/n42-node`。
  **要启用 SBMT,在那段节点启动的环境变量里加 `N42_JMT=1`**(改脚本最省事),或单节点手动启动。

## 4. 执行步骤

### 4.1 起一个带 SBMT 的本地节点
- 推荐改 `scripts/testnet.sh` 节点启动处加 `N42_JMT=1`(以及确保 RPC 可达),用 1–3 节点起:
  ```bash
  ./scripts/testnet.sh --nodes 1 --debug      # 或 3 节点
  ```
- 或单节点手动启动(参照 `testnet.sh` 拼出的 `n42-node` 命令,前面加 `N42_JMT=1`)。
- 等几个 block 出块(让 SBMT `apply_diff` 至少跑过一次;genesis alloc 里已有账户,
  例如 `0xe3778939cdCa78b70fc36dE06B0E862333D6D8dc`,见 testnet genesis)。

### 4.2 冒烟:RPC 可达
```bash
curl -s -X POST http://127.0.0.1:<http_port> -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"n42_jmtVersion","params":[]}'
curl -s -X POST http://127.0.0.1:<http_port> -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"n42_jmtRoot","params":[]}'
```
`http_port` 见 testnet 日志(`BASE_HTTP_RPC + i`)。

### 4.3 验证 proof 往返(核心)
写一个 Rust 验证器(建议放 `crates/n42-mobile/tests/sbmt_rpc_e2e.rs`,或独立 example),
逻辑如下:

```rust
// 依赖(dev-deps):reqwest(blocking 或 tokio)、serde_json、hex、bincode、
// alloy-primitives、n42-bmt-core。n42-mobile 已依赖后三者中的部分。
use n42_bmt_core::ShardedBmtProof;
use alloy_primitives::B256;

// 1) n42_jmtRoot → root(hex string，形如 "0x...")
// 2) n42_jmtProof(address) → JmtProofResponse
//    - 已存在账户(genesis alloc 里的地址)→ inclusion，value 非空
//    - 随机不存在地址 → exclusion，value 为 null
// 3) 解码并验证：
let proof_bytes = hex::decode(resp.proof_hex.trim_start_matches("0x")).unwrap();
let proof: ShardedBmtProof = bincode::deserialize(&proof_bytes).unwrap();
let root = B256::from_str(&root_resp.root).unwrap(); // root_resp.root 是 "0x..." 32字节
assert!(proof.verify(&root.0).is_ok(), "inclusion proof must verify against jmtRoot");

// 4) 负面：篡改必须被拒
let mut bad = proof.clone();
bad.shard_root[0] ^= 0xFF;
assert!(bad.verify(&root.0).is_err());
```
也可以直接调 `n42_mobile::state_proof::verify_state_proof(&proof, root)`(等价封装)。

**注意 root 一致性**:`n42_jmtRoot` 返回的是 SBMT 的 combined root（不是 reth header 的 state_root，
两者不同——SBMT 是并行树）。`jmtProof` 响应里也带 `root` 字段（同一个 SBMT root）。验证时用
**SBMT root**（`n42_jmtRoot` 或响应里的 `root`），不要用 block header 的 state_root。

### 4.4 验收标准
- [ ] `n42_jmtVersion` / `n42_jmtRoot` 可达且随出块递增/变化
- [ ] genesis 已存在账户的 `n42_jmtProof` → **inclusion**，`verify` 通过
- [ ] 不存在地址的 `n42_jmtProof` → **exclusion**（value=null），`verify` 通过
- [ ] 篡改 `shard_root` / `shard_path` / `value` 后 `verify` 返回 Err
- [ ] proof 大小合理（账户 proof 约 ~800 字节量级）

## 5. 交付

1. 把验证器代码提交（`crates/n42-mobile/tests/sbmt_rpc_e2e.rs` 或 `examples/`，注明默认 `#[ignore]`
   或读环境变量 RPC 地址，避免无节点时 CI 失败）。
2. 把验证结果（成功/失败、proof 大小、遇到的问题）写成 **`docs/devlog-60-sbmt-e2e-verification.md`**，
   并在 `DEVLOG.md` 索引加一行。
3. 若发现 bug：记录复现 + 修复，单独 commit。
4. 提交推送到同一分支 `chore/merge-reth-main-deps-upgrade`（遵守上面的无 AI 署名提交规范）。

## 6. 排查提示
- proof 反序列化失败 → 确认 `proofHex` 是整段 `bincode(ShardedBmtProof)`，不是只有内层
  `BmtProof`（见 `crates/n42-node/src/rpc.rs` `jmt_proof`：`bincode::serialize(&proof)`，`proof` 是
  `ShardedBmtProof`）。
- `verify` 失败但数据看似正确 → 确认用的是 **SBMT root**（`n42_jmtRoot`）而非 reth state_root；
  确认地址→keyHash 用的是 `n42_jmt::account_key`（blake3，不是 keccak）。
- RPC 方法 404 / method not found → `n42` namespace 未挂到 http，见 §3 不确定点。
- 节点起不来 → 先 `cargo test -p n42-node --lib`（应 206 passed）确认 lib 正常，再查测试网脚本。
