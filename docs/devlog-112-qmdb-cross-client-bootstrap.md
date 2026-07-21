# devlog-112 — gov5 replay-v2 QMDB 跨客户端 bootstrap

## 结果

gov5 与 n42-26 现在共享 QMDB portable v1：固定 chain identity、checkpoint、QMDB root、
`next_slot` 和完整 positional slot log；每个 dead slot 仍携带 frozen leaf 的 key/value，只用
active bit 表示存活性。文件末尾的 Blake3 digest 覆盖全部前序字节。

Rust 导入器先校验格式、连续 slot、长度上限、内容 digest、chain id、genesis 和 checkpoint，
再按 gov5 split commitment 重算每个 twig 与 upper tree。稀疏历史、错误身份、错误 root、截断、
尾随字节或篡改均 fail closed。

## 跨语言向量

Go 与 Rust 使用字节完全相同的 `cross_client_v1.json`（SHA-256
`74b1c578c230747fb59896e382aa4f3ee9e4eb45976d36a4fa26fb78bbf74158`），覆盖：

- 2050 次 insert，跨越 2048-slot twig 边界；
- 两次 update、两次 delete；
- 2052-slot snapshot/reload；
- gov5 v2 membership proof；
- portable v1 精确编码 fixture。

## 真实 replay 验收

只读使用既有数据，未清理或改写任何 datadir：

| 数据 | checkpoint | slots / live | portable bytes | Rust 重算 root |
|---|---:|---:|---:|---|
| preflight | 1,000 | 2,800 / 2,768 | 155,398 | `d5040c4f…69ffa77d` |
| full replay | 13,109,968 | 87,786,434 / 5,892,945 | 5,480,812,584 | `be01f8f8…3161084a` |

full replay 的源统计仍保留 13,070,722 个 source blocks、24,847,544 笔交易；portable checkpoint
使用 replay 目标链的 canonical head 13,109,968。

gov5 exporter 使用 MDBX 顺序 cursor 和原子临时文件；Rust verifier 仅保留一个 128 KiB twig
哈希堆与约 1.4 MiB upper roots。全量 Rust release 校验约 42 秒完成，不触碰现有 7 节点、
高性能路径或 archive 数据。

## 命令

```bash
go run ./cmd/n42-qmdb-export --db <replay-chaindata> --out <snapshot> --map.gb 512

cargo run --release -p n42-twig-core --example qmdb_portable_verify -- \
  --input <snapshot> --chain-id 94 --genesis <genesis-hash> \
  --expect-block <height> --expect-block-hash <hash>
```

下一阶段是把 H2 的 Proposal/Vote/CommitVote/PrepareQC/Timeout/NewView/Decide 全部固定为双端
golden vectors，并统一 v4 的 commit 签名域后接 observer gossip。
