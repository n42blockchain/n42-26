# TASK (codex, Linux/mac + 7-node testnet): continuous-ingest cadence 重跑（补 devlog-81 欠的测量）

> 这是 devlog-80/81 的收口测量。devlog-81 那次 7 节点跑因端口配错作废，核心问题至今没答案。
> 工具已就位（pool-depth 埋点 + `--sync-ingest-mode per-node-continuous` 都已合 main c049794）。
> 这次只要把测量做对。基线：reth `chore/merge-upstream-fc2cc1e` @ 449ecfdce（reth 2.3）。

## 要回答的唯一核心问题

devlog-80 显示「30s 块间尾部」其实是 harness 的 30s pool-drain 假象（stress drain p50/p95=30.001/30.006s），不是链。
**用 continuous ingest 干掉这个 30s 窗口后：**
1. `n42_inter_block_commit_ms` 的 p50/p95 降到多少？（假象若消失，p95 应从 ~31s 大幅下降）
2. **leader payload build ~2s（EVM 786ms + packing 1s）是不是就浮出成新的真瓶颈了？**
3. 用新加的 `pool_pending_at_commit` / `pool_pending_at_build_start` 确认 leader 在 build 时 pool 是否一直被喂饱（pending 不归零 = 喂饱了，cadence 才反映链本身）。

## ⚠️ 防呆：端口（上次就栽在这）

`scripts/testnet.sh:598` 是 `N42_INJECT_PORT="${N42_INJECT_PORT:+$((+i))}"`——**只有在跑 testnet.sh 之前 export 了 `N42_INJECT_PORT`，每个节点 i 才会监听 `N42_INJECT_PORT + i`**。不 export → 所有节点都不开 ingest 端口（上次只有 node0 通就是这个原因）。

所以**必须**：
```bash
export N42_INJECT_PORT=19900        # 7 节点 → 19900,19901,...,19906
```
启动后、灌流量前，**先验证 7 个端口都在监听**（防呆）：
```bash
for p in 19900 19901 19902 19903 19904 19905 19906; do
  (exec 3<>/dev/tcp/127.0.0.1/$p) 2>/dev/null && echo "$p OK" || echo "$p REFUSED"
done
```
7 个全 OK 再灌。任何一个 REFUSED 就停下查，别再产生作废数据。

## 执行步骤

0. **先清干净 reth**（你最近两次 Cargo.lock 都因脏 ../reth 漂移）：
   `git -C ../reth stash || git -C ../reth checkout .` 然后确认 `git -C ../reth log -1 --oneline` 是 449ecfdce、`git -C ../reth status` 干净。
1. `cargo build --release --bin n42-node --bin n42-stress`（应无 Cargo.lock 改动；若 lock 变了说明 reth 还脏，停）。
2. 启动 7 节点（沿用 devlog-80 的 env）：
   ```bash
   export N42_INJECT_PORT=19900
   export N42_TWIG=1 N42_FAST_PROPOSE=1 N42_MIN_PROPOSE_DELAY_MS=0 N42_DEFER_STATE_ROOT=1
   export N42_SKIP_TX_VERIFY=1 N42_MAX_TXS_PER_BLOCK=90000 N42_INJECT_HIGH_WATER=90000
   export N42_INGEST_TARGET_PENDING=90000 N42_POOL_MAX_TXS=300000 N42_GAS_LIMIT=2000000000
   export N42_DISABLE_TX_FORWARD=1 N42_INGEST_EXTENDED_ACK=1 N42_ASYNC_FINALIZE_FCU=0
   ./scripts/testnet.sh --nodes 7 --clean --no-explorer --no-tx-gen --no-monitor --no-mobile-sim --block-interval 2000
   ```
3. 验证 7 端口监听（上面那段 for 循环）。
4. 灌流量，**continuous 模式**，简单转账（冲节奏、不是 ERC-20）：
   ```bash
   target/release/n42-stress \
     --rpc http://127.0.0.1:18000,...,http://127.0.0.1:18006 \
     --ingest 127.0.0.1:19900,127.0.0.1:19901,127.0.0.1:19902,127.0.0.1:19903,127.0.0.1:19904,127.0.0.1:19905,127.0.0.1:19906 \
     --sync-ingest-mode per-node-continuous \
     --presign-load /tmp/n42-devlog79-5m-7rpc.bin \
     --wave 90000 --batch-size 500 --target-tps 0 --erc20-ratio 0
   ```
5. 稳态跑 ≥60s。

## 对照（可选但有价值）

主跑：`per-node-continuous`。若时间够，再跑一轮 `per-node`（旧 30s 窗口）做 A/B，直接证明 continuous 把 inter-block p95 从 ~31s 压下来了。**这次 async-FCU 固定 =0**（devlog-80 已证它不是瓶颈，不用再 A/B）。

## 交付：docs/devlog-82-continuous-cadence.md

必须包含：
- continuous（和可选的 windowed）下的 cadence 表：inter-block commit / commit→build_start / build_start→broadcast / follower import 的 p50/p95，**外加 pool_pending_at_commit / at_build_start 的 p50**（证明 leader 喂饱没）。
- 持续 TPS（wall + active-wave）。
- **明确回答上面 3 个问题**，尤其：30s 假象消失后真瓶颈是不是 leader build ~2s，下一个该攻的点是什么（EVM 还是串行 packing）。
- 基于 main 开分支 PR，提交别带 Claude 字样。Cargo.lock 若有改动必须是干净 reth 重生成的。
