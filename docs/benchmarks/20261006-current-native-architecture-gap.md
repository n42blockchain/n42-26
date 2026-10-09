# 当前 n42-rs 原生路径追赶基线

日期：2026-10-06。参考源码：`../n42-rs`，HEAD
`a97a146466b3c2065e09ccfe4d6964d5f476effd`。本仓库：合并提交
`d5881b5` 加当前未提交工作区。这里是源码对照，不是新性能测量。
此前引用 `09a1284e4` 的 QMDB 对照仍可解释那次局部优化，不能代表当前原生架构。

## 必须保持的比较口径

| 项目 | 当前参考实现及证据 | 追赶要求 |
| --- | --- | --- |
| 状态承诺 | QMDB；本仓库 `crates/n42-node/src/qmdb_state_root.rs` 已使用 `QmdbLeafTree` 的二叉承诺 | 比较批量叶哈希、祖先折叠、读视图与持久化等待；不能把替换 Ethereum MPT 当作尚未完成的核心工作 |
| 执行拓扑 | 七个验证者可共享一份 EL；`../n42-rs/bin/n42/src/import_once.rs` 按块协调导入，受 `N42_IMPORT_ONCE=1` 控制 | 主目标按 E1 比较；记录每块每 EL 的实际执行次数、重复请求复用和换主等待。E7 是另一个部署口径 |
| 转账 | `../n42-rs/crates/n42/engine-types/src/fast_transfer.rs` 直接计算符合条件的 EVM 等价转移；解释器处理回退。该模块开关默认关闭 | 核对实际运行开关、命中率、回退原因、账户读取和并行批次。已有本地 `crates/n42-execution/src/fast_transfer.rs`，不能默认每笔转账都跑解释器 |
| 签名 | `tx-types/src/alt_sig.rs` 的原生 `0x50` Ed25519 交易；`frame.rs` 的网关帧认证覆盖 chain ID 和帧根 | 分开逐笔验签与达到网关认证阈值的帧路径；认证帧不重复逐笔验签，不等于没有身份验证。普通 RPC 的签名恢复缓存不能直接代表原生收益 |
| 交易承诺 | `tx-types/src/frame.rs` 包含二叉交易/帧根 | 状态二叉根与交易帧二叉根分别核对，兼容 Gov5/RLP 路径的编码优化不能代替原生帧协议 |
| 封块与执行结果 | `engine-types/src/payload.rs`、`output_shards.rs` 支持早封块、分片输出、后续字段与状态发布 | 分别度量 seal、完整执行、根就绪、canonical 和可读取时间；早封块计数不能直接充当完成执行 TPS |

上述功能在源码中存在不代表所有部署都启用。尤其共享执行、快速转账、
帧认证和延后执行要保存运行参数、genesis 分叉配置及实际指标。
本仓库 native fleet 的 libp2p Ed25519 身份也不能作为原生交易 Ed25519 已接入的证据。

## 接下来按这个顺序追赶

1. 固定 E1、七验证者、同一交易集和资源预算；核对导入去重、执行结果复用、
   换主与失败重试。共享 EL 的执行故障域也要记录。
2. 对齐原生 `0x50` 交易、认证帧接收与帧根契约。分别记录生产、接收、认证、
   排队、选择速率及积压，定位进队列的背压。
3. 对齐转账模拟快路径与并行输出：账户读视图、批次分配、冲突合并、
   输出分片冻结，以及父块状态/字段等待。保留 EVM 回退语义。
4. 在已经使用的 QMDB 二叉树上继续优化批量写入与根计算，区分内存承诺、
   WAL/持久化完成、历史读取和恢复；局部树微基准不能当作整链提升。
5. 分离封块关键链与最终字段发布的等待，检查有界积压。当前 Gov5 编码候选
   仅覆盖兼容广播路径，尚未编译或测量，不能据此宣称追平参考性能。

## 性能证据边界

`docs/performance-3m-catchup-20261006.md` 记录 loop342P 的三个历史 canonical
窗口为 3,093,333 / 3,126,667 / 3,080,000 TPS，使用七验证者共享执行、
认证 Ed25519 帧与并行延后执行。这些数据不能归属于本次最新 HEAD。
同文档中的 Mac E1/E7 短窗口属于不同硬件和资源条件，不能与上述 Linux
结果直接计算追赶比例。

本次仅检查源码；没有运行新的构建、测试或性能任务。下一次实测必须保存
两个仓库的提交及工作区差异、二进制哈希、配置、硬件和测量窗口，并同时验证
共识提交、canonical 交易数、状态根、交易回执、持久化与恢复。

## 首批实现进度

下列小提交在 `perf/seven-node-400k-cap-timeline` 上逐次推送。
只完成格式化、源码审查及 diff 空白检查；未构建、运行测试或测量收益。

| 提交 | 改动 | 适用范围 |
| --- | --- | --- |
| `6b164bc` | TCP 入队使用连接级连续批缓冲与交易偏移，去掉逐笔字节 Vec 分配/复制 | 当前受信任测试入队协议；保留原有计数/字节上限、信用前缀和 ACK 格式。连接可能保留其最大批缓冲容量 |
| `b7e2b47` | 分离读包、解码、入池耗时以及硬/软门控、部分接收和错误计数 | socket_read 包含等待发送端的时间，不是认证帧验签时间；submitted 不代表全部接收成功 |
| `95de4c1` | 相同块哈希的并发 Engine 请求按序提交 | `N42_IMPORT_SINGLE_FLIGHT=1`，默认关闭；每个请求仍完整交给 Engine 校验，不缓存 Valid/Accepted/Syncing，不绕过验证。不同哈希独立；取消释放门锁。门锁属于当前适配器，不能等同于跨进程共享执行拓扑 |
| `6ee7f05` | 延后收益计算只读账户标量，避免逐笔克隆 AccountInfo 和其共享字节码 | 当前并行 EVM 收益路径；空账户 nonce/code_hash/balance 保持原默认值，仍保留原回退规则 |
| `0d01863` | 单独计量输出组装，完成日志延至收益归集结束 | 当前通用并行 EVM；尚未迁移参考原生 output_shards 的冻结/发布流程 |
| `2b34946` | 计量等待 pending payload 与 canonical FCU 边界 | payload_resolve 包含完成输出转换，不代表共识 seal 或持久化完成 |

观测入口：

- `n42_ingest_batch_duration_ms{phase=socket_read|decode|pool_admission}`，
  `n42_ingest_pending_at_admission` 和 `n42_ingest_batches_total{outcome=hard_gate|soft_gate|submitted}`。
- `n42_ingest_transactions_total{outcome=accepted|decode_error|pool_error|deferred}`；
  deferred 是已提交批中未消费的尾部交易，不包含整批硬/软门控拒绝。
- `n42_engine_import_gate_contention_total`、`n42_engine_import_gate_wait_ms`。
  门控只让 Engine 有机会复用前一个完成结果，不证明实际执行次数为一。
- `n42_parallel_evm_output_assembly_ms`、`n42_parallel_evm_total_duration_ms`。
  旧 `n42_parallel_evm_duration_ms` 保留工作线程阶段口径；新完成日志的
  elapsed_ms 包含输出组装，worker_ms 保留旧口径。顺序回退不纳入这些成功并行指标。
- `n42_engine_boundary_duration_ms{phase=canonical_update|payload_resolve}`。

下一批依次核对：原生认证帧接收与有界积压、共享 EL 的实际一次执行/换主复用、
固定父块 QMDB 读视图、原生输出分片冻结，以及根/字段发布是否阻塞 seal。
已有未提交的 Reth 升级、QMDB 读视图和并行导入工作保持独立；本批没有将这些
前置改动整体夹入小提交，也没有据当前兼容路径指标宣称已达到原生参考容量。
