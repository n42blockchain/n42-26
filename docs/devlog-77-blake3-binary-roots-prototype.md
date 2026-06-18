# Devlog 77: BLAKE3/binary roots 第一阶段原型 A/B

> 日期：2026-06-18
> 背景：devlog-76 显示简单转账冲峰时，关键瓶颈不是 EVM，而是大块 `builder.finish()`
> 后处理中的 RLP/tx_root/state_root keccak。目标是先做 flag-gated 原型，用 BLAKE3
> roots / binary-ish 内部路径压串行后处理，并用 7 节点 A/B 看收益。

## 1. 实现范围

代码在 reth clean 分支提交：

- reth branch：`proto/n42-blake3-block-roots-clean`
- reth commit：`d23dcc7dfc proto: add n42 blake3 block root flag`
- n42-26 branch：`proto/blake3-binary-roots`
- flag：`N42_BLAKE3_BLOCK_HASH=1`，默认关。

实现内容：

- `tx_root`：flag 开时用 BLAKE3 Merkle over EIP-2718 tx bytes。
- `receipts_root`：flag 开时用 BLAKE3 Merkle over EIP-2718 receipt bytes，logs bloom 仍按标准 OR。
- `state_root`：leader finish 里用 deterministic BLAKE3 commitment over `HashedPostState`，不是 Ethereum MPT。
- `block_hash`：Engine payload `block_hash` 用 `BLAKE3("n42-blake3-payload-hash-v1" || sealed_hash)` 包装；alloy header hash 本身仍是 keccak。
- follower validation：flag 开时用 BLAKE3 tx/receipt root 校验，并跳过 MPT state-root compare。
- payload cache：cache key 跟 Engine payload hash 使用同一 BLAKE3 wrapper，避免 flag 开后 cache miss。

明确限制：

- 这是 fresh-genesis benchmark prototype，不兼容 Ethereum block hash / state root 语义。
- `state_root` 不是完整状态树 root，只是 post-state diff commitment，用来量串行 root 热点。
- binary codec 没有整条替换 Engine API / EIP-2718 边界；当前只复用已有 TCP ingest、compact/state-diff bincode 路径，并替换 root/hash 计算。
- 因为 `sync-ingest` global wave + TX Forward ON 会把 90k wave 分到 7 个入口，本次没有复现 90k tx/block 大块；多数 tx-bearing block 仍是 12,857 tx，最高 25,714 tx。

## 2. 验证

构建 / check：

```bash
# reth clean worktree
cargo check -p reth-consensus-common -p reth-evm -p reth-evm-ethereum \
  -p reth-ethereum-consensus -p reth-ethereum-engine-primitives \
  -p reth-ethereum-payload-builder -p reth-engine-tree

# n42-26 current worktree
cargo build --release --bin n42-node --bin n42-stress --bin n42-mobile-sim
```

结果：通过。仅有既有 warning：`reth-config::default_minimum_pruning_distance`、`BlockExecutor`
unused、`reth-node-ethereum` unused import/variable。

4 节点 final smoke：

- config：`N42_TWIG=1 N42_BLAKE3_BLOCK_HASH=1`, 2s slot, clean genesis。
- 结果：四个 RPC 都到 `0x17`；`N42_FCU status=Valid` 连续推进；grep 未见
  `mismatch|PayloadError|BlockHash|Invalid payload|panic|ERROR`。
- 日志确认 `N42_BLAKE3_STATE_ROOT ... state_root_ms=0`。

Default-off scenario 1/3/4：

- 尝试运行 `E2E_SCENARIO_FILTER=1,3,4 E2E_SCENARIO4_PROFILE=correctness cargo run --release -p e2e-test -- --binary target/release/n42-node`。
- harness build 通过，scenario1 启动并出到 block 5。
- 未完整跑完：scenario1 默认监控 400s，本次快确认中止；没有残留进程或 8545 listener。

## 3. 7 节点 A/B

共同配置：

- `./scripts/testnet.sh --nodes 7 --clean --no-explorer --no-tx-gen --no-monitor --block-interval 2000`
- `N42_TWIG=1`, `N42_GAS_LIMIT=2000000000`, `N42_MAX_TXS_PER_BLOCK=95000`
- workload：450k pre-signed simple transfers，`n42-stress --wave 90000`，7 ingest endpoints。
- 注意：stress 日志显示 `SYNC_INJECT global wave mode (TX Forward ON) wave_cap=90000 per_ep_cap=12857 num_eps=7`。

Stress 结果：

| run | flag | total tx | elapsed | wall TPS | sustained TPS | blocks | overall TPS | max tx/block | gas util |
|-----|------|---------:|--------:|---------:|--------------:|-------:|------------:|-------------:|---------:|
| baseline | off | 450,000 | 68.5s | 6,572 | 6,984 | 34 | 6,816.2 | 25,714 | 13.9% |
| final | on | 450,000 | 65.8s | 6,836 | 7,302 | 33 | 7,029.2 | 25,714 | 14.3% |

Leader payload / finish stats from `N42_PAYLOAD_PACK` + `N42_FINISH_BREAKDOWN`:

| metric | off p50 | off p95 | off max | on p50 | on p95 | on max |
|--------|--------:|--------:|--------:|-------:|-------:|-------:|
| tx/block | 12,857 | 12,857 | 25,714 | 12,857 | 12,857 | 25,714 |
| `evm_exec_ms` | 20 | 32 | 42 | 24 | 33 | 35 |
| `packing_ms` | 30 | 44 | 62 | 35 | 47 | 52 |
| `state_root_ms` | 3 | 5 | 10 | 0 | 0 | 0 |
| `assemble_block_ms` | 20 | 22 | 30 | 23 | 25 | 36 |
| `total_finish_ms` | 24 | 26 | 35 | 24 | 26 | 37 |

## 4. 结论

原型正确性成立：flag 开 fresh genesis 可以连续出块，tx/receipt/state roots 和 Engine payload
hash 都能走 BLAKE3 原型路径；state-root 计时从 p50/p95 `3/5ms` 压到 `0/0ms`。

但本次 A/B 没有证明 devlog-76 的 90k 大块后处理瓶颈已解决。原因是这轮注入没有形成
90k tx/block，`builder.finish()` baseline 本来就只有 p95 `26ms`，远低于 devlog-76 的
~920ms 大块后处理。最终 flag-on `total_finish_ms` 仍是 p95 `26ms`，TPS 小幅上升约 4%，更像
低负载方差 + 少一个 block 的排布差异，不能当作 BLAKE3 收益。

下一步如果要验收这条优化，先修 measurement：必须让单 leader block 真正吃到 90k tx（或复用
devlog-76 那套 TCP 122K 注入方式），再重跑 off/on。只有在 90k block 下看到
`assemble_block_ms + state_root_ms + total_finish_ms` 从数百毫秒级降下来，才值得讨论默认开。

## 5. 90k 单入口重跑

按要求修正 measurement：不用 7 个 ingest endpoint 分摊 `wave=90000`，而是用单 RPC / 单 TCP
ingest endpoint，把 90k tx 放进同一个 validator 的本地池。

共同配置：

- `cargo build --release --bin n42-node --bin n42-stress --bin n42-mobile-sim`
- `N42_TWIG=1`, `N42_DISABLE_TX_FORWARD=1`
- `N42_MAX_TXS_PER_BLOCK=95000`, `N42_GAS_LIMIT=2000000000`
- `./scripts/testnet.sh --nodes 7 --clean --no-explorer --no-tx-gen --no-monitor --block-interval 2000`
- workload：单 RPC pre-sign `450000` simple transfers，单 endpoint `--ingest 127.0.0.1:19900`,
  `--wave 90000`, `--erc20-ratio 0`

这次确实打到了 90k 大块：

| run | flag | tx-bearing leader blocks | max tx/block | total tx in leader pack logs |
| --- | --- | ---: | ---: | ---: |
| single-entry off | off | 5 | 90,000 | 450,000 |
| single-entry on | on | 5 | 95,000 | 450,000 |

Stress / chain summary：

| run | stress summary caveat | elapsed | wall TPS | sustained TPS | chain tx blocks |
| --- | --- | ---: | ---: | ---: | --- |
| off | complete at stress exit | 74.4s | 6,052 | 6,590 | five `90000` blocks |
| on | stress summary was early; waited for pool drain and re-read RPC/logs | 137.4s at stress exit | 3,276 | 3,337 | `90000, 95000, 95000, 95000, 75000` |

Leader `N42_PAYLOAD_PACK`:

| metric | off p50 | off p95 | off max | on p50 | on p95 | on max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `tx_count` | 90,000 | 90,000 | 90,000 | 95,000 | 95,000 | 95,000 |
| `evm_exec_ms` | 98 | 108 | 108 | 89 | 111 | 111 |
| `packing_ms` | 147 | 160 | 160 | 136 | 164 | 164 |
| `pool_overhead_ms` | 48 | 51 | 51 | 47 | 53 | 53 |

Leader `N42_FINISH_BREAKDOWN`:

| metric | off p50 | off p95 | off max | on p50 | on p95 | on max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `state_root_ms` | 3 | 3 | 3 | 0 | 0 | 0 |
| `assemble_block_ms` | 91 | 94 | 94 | 109 | 110 | 110 |
| `total_finish_ms` | 99 | 101 | 101 | 114 | 114 | 114 |

Two important caveats:

- The corrected off baseline no longer has the previous false low-load problem: it is real
  90k/block. But it still does **not** reproduce a hundreds-of-ms finish/root bottleneck on
  this current code and macOS setup. Baseline `total_finish_ms` is only p95 `101ms`, with
  `state_root_ms` p95 `3ms`.
- The single-entry flag-on run is useful for leader finish timing, but not a clean 7-node
  propagation/TPS acceptance run: validators 0-4 advanced and drained, while validators 5/6
  lagged at height 26 during the run. A follow-up attempt to broadcast the same wave to all
  7 local pools kept all nodes at the same height but produced no tx-bearing blocks
  (`ingest accepted=0`, soft-gated), so it was discarded.

Conclusion for the 90k rerun:

- Measurement fix worked: the run now proves the leader can build real 90k transfer blocks.
- The BLAKE3 prototype removes the tiny measured state-root cost (`3ms -> 0ms`) but does not
  reduce total finish time; in this sample total finish is slightly higher (`101ms -> 114ms`
  p95), dominated by block assembly / tx+receipt root work.
- There is no demonstrated TPS gain worth default-enabling from this prototype. The original
  devlog-76 `~920ms` simple-transfer post-processing number is not reproduced here; before
  doing more format work, the next step should be narrower timing inside `assemble_block`
  (tx root vs receipt root vs header/block construction) or a symbolized profile of the
  current 90k transfer finish path.
