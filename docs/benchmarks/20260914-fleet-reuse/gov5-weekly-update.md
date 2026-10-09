# DATC 周更新：把证明归档延伸到新的链头

目标：mainnet 数据每周更新后，在**已完成的 DATC 归档上增量追加**新区块，而不是重建。
适用对象：`cmd/n42-datc` v2 格式归档（25M 全链构建：lo `[0,17.9M)` + hi `[17.9M, 25,864,982)` 合并后的库）。

## 1. 现状（2026-09-14）

| 输入 | 覆盖范围 | 说明 |
|---|---|---|
| `acctcs` / `storcs`（`/data/blockchain/datc-input/N42-eth1177/chain/freezer`） | 区块 0..**25,864,981** | cidx 头 start=0，25,864,982 项；这就是本次构建的终点 |
| `headerc`（`/data/blockchain/witness`，由 `datc-input/n42-eth1` 软链） | 到 25,870,336 | 金标校验（每块 stateRoot） |
| 周更新 `ethel-test/ethel-full-25943310`（09-11） | 头/体到 ~25,944,064 | **不含 acctcs/storcs**：快照启动（`--bootstrap.mode snapshot`），不逐块执行，没有逐块变更集 |

结论：当前构建的最后一块是 **25,864,981**。周更新的节点数据本身不能喂给 DATC。

## 2. 为什么能增量

- **续跑**：`build` 不带 `--start` 时从 `DatcMeta/progress` 接着跑；输出库里的当前态表（`HashedAccount/Storage`、`TrieOf*`）就是上次终点的状态。
- **终点自动截断**：`--end` 会被截到 `min(acctcs.Items(), headerc.MaxBlock())`（`main.go` 的 `avail` 逻辑），所以周更新可以直接给一个足够大的 `--end`。
- **leaf 段合并**：跑完一轮时 `finalizeLeafSegments` 把 spill 写成 `leafseg/*.seg`；再次续跑结束时，`finalizeBucket` 会把新行与已存在的 `.seg` 按序合并（`TestE2E_LeafSeg_ResumeMerge` 覆盖了"先收尾、再续跑、再收尾"）。
- **epoch 按绝对块号对齐**（`--sched`、`--acc-root-epoch`），延伸后的记录与已有记录格式一致。

**前提（务必保留）**：归档库里的当前态表不能删。若为了省空间丢掉当前态表，就无法增量，只能用 `prep-state` 从一份对应高度的状态拷贝重新接上，或者重建。

## 3. 变更集从哪来（关键）

acctcs/storcs 只能由**逐块执行**的节点写出：eth-el staged catch-up 默认开启 CS freezer sink
（`internal/ethel/eldevp2p/downloader.go`，`N42_CS_FREEZER` 未设为 0），按块号连续追加到数据目录的
`chain/freezer/acctcs|storcs`；有缺口会报错 `table head ... below wanted ... (gap)`。执行需要一份停在上次
终点的 PlainState。

可行路线：

1. **推荐：Windows 执行节点 `D:/N42-eth1177` 继续追块**（这批变更集的原产地，PlainState 与 freezer 同高度）。
   做法见 `docs/ethel/catchup-from-eth1177-recipe.md`（eth-el `--datadir D:/N42-eth1177 --bootstrap.enabled=false`
   追到新链头）。追完后 `acctcs/storcs` 与 `D:/n42-eth1/chain/freezer/headerc`（或同链的 headerc）即覆盖新范围。
2. Linux 上另起一个逐块执行、开 CS sink 的 eth-el 节点。前提是有一份 25,864,981 高度的 PlainState；
   原 `ethel-archive-25864981` 已被周更新轮换删除，目前 Linux 上没有，需要先准备。
3. 周更新节点（快照模式）不能用，除非改成非快照、逐块执行并保留变更集。

## 4. 每周操作步骤

以"上次终点 E（不含）→ 新链头 T"为例（首次 E = 25,864,982）。

1. **准备变更集**：在执行节点上追到 T，确认 `acctcs.Items() > T` 且 `storcs` 同高
   （Linux 侧可用 `go run ./cmd/freezer-items <freezer 目录>` 查看项数）。
2. **同步到 Linux（先放暂存目录验收，再替换）**：
   - **变更集尾段不是纯追加**：批量模式下，旧表最后一个不满 64 项的批次会在**原偏移处被重新编码成满批**
     （2026-09-14 实测：`acctcs.0078.cdat` 最后约 35 万字节、`storcs.0149.cdat` 最后约 49 万字节被改写，
     cidx 前缀不变）。因此这两个尾段**必须整文件替换**（写临时文件再 rename，rsync 默认行为），
     **禁止** `rsync --append/--inplace` 或 `cp` 覆盖——追加会把旧尾批和新数据拼坏，截断式覆盖会让读者读到半截文件。
   - **区块头**：`headerc.NNNN.cdat` 是纯追加（新段写在文件末尾），但 `headerc.cidx` 最后一项会被改指向新段，
     所以 cidx 同样整文件替换。
   - **顺序**：先新段和尾段 `.cdat`，最后 `.cidx`；没变化的旧段不要碰。
   - `datc-input/n42-eth1/chain/freezer/headerc.*` 是指向 `/data/blockchain/witness` 的软链：只把变化的
     `headerc.cidx` 和最后一个 `.cdat` 换成实体文件，其余软链保留，**不要改 witness**（共享副本）。
   - **运行中的 DATC 不必停**：它启动时就缓存了项数/段数，并一直持有 cidx 与尾段的文件句柄；rename 替换后它继续读旧 inode，
     结果不变。新数据在**下一次续跑**时才生效。
   - **验收暂存数据**（替换前）：用与 DATC 相同的打开方式（`NewFreezerTableReadOnly` 后 `ForceBatchSize(freezer.BatchSize)`
     + `SetCompressed(true)`，否则读出的是空值或整批压缩字节）逐块读取：重叠区与现有数据逐块相同、新块全部非空；
     区块头与独立来源（周更新 eth-el 节点的 headerc）逐块比对 stateRoot/txRoot/receiptRoot。
     压缩区块头去掉了 ParentHash，不能用 `Hash()` 做父子链接检查。
3. **确认机器空闲**：`ls /data/blockchain/.box-claim-*`，写 `.box-claim-datc`。**只在用户明确下令时启动 DATC。**
4. **续跑延伸**（与主构建相同参数，`--end` 给大值，自动截断到可用高度）：
   ```bash
   B=/data/blockchain/datc-out
   # 以当前二进制和运行脚本为模板；把 --out 指向合并后的归档库，--end 改成 99999999
   setsid nohup env WRITEMAP=1 $B/supervise.sh $B/run-weekly.sh $B/datc-weekly.build.log >/dev/null 2>&1 </dev/null &
   ```
   日志应显示 `auto-resume from saved progress: --start E`，并以 `DATC build done` + `[leafseg] done` 结束。
   逐块金标校验不能出现 `ROOT MISMATCH`。
5. **验收**：
   ```bash
   $B/datc.bin verify --out <归档库> --headers <headerc 目录> --samples 50      # 50/50，且无 corrupt frame 警告
   $B/datc.bin bench  --out <归档库> --headers <headerc 目录> --changesets <acctcs 目录> --samples 500 --mode mixed --parallel 8
   ```
   另查 `DatcMeta/progress == T+1`、`head` 同步推进、`start` 未变（合并后应为 0）。
6. **收尾**：删 `.box-claim-datc`；记录新的终点 T。

## 5. 耗时与资源参考

- 25M 构建后段（24–25.2M）实测 **20–27 块/秒**（hi9 + `--writemap`）；一周约 5 万块 ≈ **35–45 分钟**，
  另加页缓存预热（冷启动约 20 分钟）和 leaf 段合并收尾。
- 内存：Go 堆稳定 13–24 GB（`--mem.gb 40`）；磁盘增量按每百万块约 70–100 GB 预留。

## 6. 注意事项

- 周更新节点数据目录（`ethel-test/*`）会被下一轮删除，DATC 的任何输入都不要指向那里。
- 变更集和区块头必须来自**同一条链、同一高度连续**；金标校验会在第一块发现不一致（ROOT MISMATCH 时回滚，不写坏数据）。
- 延伸中途要停，用守护脚本的 TERM 优雅停（等当前批提交）；不要 `kill -9`。
- 二进制用 `datc.bin` 指向的当前版本（含写库 upsert、钩子分片、dense 钩子泄漏修复）；旧二进制会慢数倍或 GC 饥饿。
