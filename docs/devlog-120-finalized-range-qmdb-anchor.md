# devlog-120 — finalized range 与 QMDB checkpoint 同链锚定

日期：2026-07-21

分支：`feat/gov5-n42-live-interop`

基线：`main @ f904813`、Reth `2.4.1 @ c533db8`

## 目标与边界

在不修改现有 gov5 七节点 datadir、replay history 或高性能路径的前提下，把 canonical
block/header/receipt 尾段与 portable QMDB checkpoint 绑定成同一条链的只读 observer
启动门禁。这一步验证可追赶数据的身份和连续性，但不向 Reth 导入执行状态，也不允许
Rust observer 投票。

## 实现

- gov5 `n42-qmdb-export` 增加最多 128 块的 finalized-range v1 原子导出，携带 chain ID、
  genesis、canonical header/block RLP、compact receipts 及 block/parent/state/tx/receipt
  roots，整帧使用 Blake3 内容摘要。
- Rust 增加流式 verifier，限制块数与单 blob 大小，拒绝截断、尾随数据、错误摘要、
  非连续高度和 parent lineage；同时验证 header Keccak、block 内嵌 header，以及 range
  声明的 number/parent/state/tx/receipt 字段与 header RLP 完全一致。
- observer 设置 `N42_FINALIZED_RANGE_BOOTSTRAP` 时，要求先通过 QMDB portable bootstrap，
  并强制 range 末块的 block hash 与 state root 等于 QMDB checkpoint 后才启动网络。
- replay-v2 修复 fresh target 的 block 1 父哈希：genesis 写入后重新读取 block 0 canonical
  hash；旧逻辑把 `from == 1` 也提前返回，可能生成零父哈希。新增内存 MDBX 回归测试。

## 现有七节点真实验收

保留的 runtime-01 七节点链生成了独立新文件 `finalized-771-898.v1`，没有覆盖任何已有
数据。Rust 对同一链的两个输入得到一致锚点：

- chain ID：`1143`
- genesis：`dd96ceb7730fb4a01f6c42aa42908f8e3f7fb02c665829ec6bd96493079f3658`
- range：block `771-898`，128 块，首父块为 block 770
- checkpoint/head：`663a60f7aa9259c1d2e57cd780750bdae5ff14025936afcd958b92bf54f080aa`
- QMDB/header state root：`76ae4240ad9c782c46911141ace395d9afb75d6f4fb0425287315e12cfeb4a4c`
- receipts root：gov5 native empty root `c5d246...a470`

observer 真机启动先记录 QMDB portable 验证，再记录 finalized range 与 checkpoint 对齐，
随后以 observer mode 订阅 legacy gov5 与 H2-v4 topics。Reth RPC 返回 chain ID 1143，日志
明确为 `OBSERVER mode (no consensus participation)`；验收后 Ctrl-C 正常退出。

## 当前结论与下一步

现有七节点现在可以安全接入 Rust **只读 follower/observer 的身份与历史数据门禁**，但
还不能称为 execution follower：Rust 尚未解码 compact receipts、逐块执行并推进 Reth
head，也没有 finalized-range request/response。下一步先实现 receipt 解码与逐 entry
导入回调，再接网络 catch-up；只有 1,000 块执行、receipt root、QMDB root 与 H2 finality
连续一致后，才评估混合 validator 投票。
