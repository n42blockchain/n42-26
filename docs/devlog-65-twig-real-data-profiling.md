# devlog-65 — twig 引擎真实主网数据剖析 + value-arena 优化

## 数据来源

`D:\reth2k` 是**完整以太坊主网 reth archive**(25.2M 块）：HashedAccounts **3.91 亿**、
HashedStorages **15.8 亿**（共 ~20 亿叶，~200GB+，远超 DRAM）。用 `reth.exe db list
HashedAccounts --len 1000000 --json` 导出 1M 真实账户（key=hashed address，value=
nonce/balance/bytecode_hash），喂 twig 引擎剖析。

剖析工具：`crates/n42-twig-core/examples/profile_real.rs`（解析真实账户 → `ShardedTwig`，
阶段计时 + RSS + proof 采样；unix 下出 pprof flamegraph，Windows 用手动 breakdown）。

> `states/state_logs_data.bin`（317GB，`STLOGDT2` 自定义格式，pevm.exe 产出、无源码）逆向
> 风险高，未采用；改用 reth.exe 直接导表（可靠）。

## 剖析结果（1M 真实主网账户，9950X，release）

| 指标 | 优化前 | value-arena 后 | vs SBMT(arena) |
|------|--------|----------------|----------------|
| **RSS** | 304 MB | **280 MB（280 B/acct）** | SBMT 375 → **省 25%** |
| build | 1.33s | **1.30s**（772k accts/s） | 持平 |
| proof | 929 B / gen 1.71us / verify 1.80us | 同 | proof gen 比 SBMT 快 |

吞吐与合成基准一致（key 都是均匀 hash）。proof gen 1.7us（twig path 11 + upper + shard，
扁平堆直读，比 SBMT 的 3us 快）。

## 内存拆解（剖析驱动）

@1M 的 ~300MB：entries 的每值 `Vec<u8>`（24B header + 堆分配圆整）~45% + index HashMap
~18% + twig 节点数组（2 hash/leaf × 32B = 64B/leaf）~21%。

## 优化：value arena（root 不变，−8% RSS）

把每 entry 的 `value: Vec<u8>` 换成**扁平 `value_arena: Vec<u8>`** + `Entry{voff:u64, vlen:u32}`
索引切片：
- 消除每值 24B Vec header + 每值一次堆分配/圆整；值连续利于缓存。
- `voff` 用 u64，单 shard arena 可超 4GB。
- snapshot 对死 entry 存空值 → restore 时压缩 arena（回收 churn 死值）。
- **root 完全不变**：3 个 gov5 跨语言对拍 + 14 测试全过，clippy 干净。

实测：RSS 304→280MB（−8%）、build 略快（少 1M 次堆分配）。

## 结论与后续

- twig 全-DRAM @1M 真实账户 **280 B/acct**，比 SBMT 省 25%。但距 AlDBaran/QMDB ~100 B/acct
  仍有差距 —— 根因是 all-DRAM 把 entry 数据 + 全 twig 节点驻内存（QMDB 驱逐 twig 叶 + entry 下盘）。
  这是 all-DRAM 路线的内禀成本（用户已选 all-DRAM 换吞吐/简单）。
- **CPU 热点 = blake3**（每插入 12 次：1 leaf + 11 fold）。SIMD 批量（AVX-512 16-way）是 CPU 侧
  下一杠杆（需 blake3 内部/vendored，参考 C2）。pprof flamegraph 在 Linux/WSL 跑 `profile_real`
  可直接看到 blake3 占比。
- 后续可选内存杠杆：compact index（QMDB 式只存 key 高位 + slot，省 ~7%）。

## 复现

```
# 导真实账户（reth.exe 在 D:\reth2k）
reth.exe db --datadir D:\reth2k list HashedAccounts --len 1000000 --json > D:\reth2k-accounts-1m.json
# 剖析
PROFILE_ACCOUNTS=D:/reth2k-accounts-1m.json cargo run --release --example profile_real -p n42-twig-core
# Linux/WSL 额外产出 twig_flamegraph.svg
```
