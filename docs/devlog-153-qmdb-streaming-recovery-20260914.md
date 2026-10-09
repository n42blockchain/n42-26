# QMDB 大块历史恢复与流式 WAL 读取

日期：2026-09-14，接续 [devlog-152](devlog-152-qmdb-undo-budget-20260914.md)。
**4 节点 1M TPS 尚未实测达标。** 本轮保持 H2、二叉树 QMDB 状态读取/承诺、
原有执行校验及持久化同步，继续处理操作历史的内存和恢复问题。

## 启动恢复中的遗漏

上一轮补齐了运行中历史读取、分支移动和 WAL 失败时的认证 base 恢复，但启动时
`validate_persisted_blocks` 仍使用另一条路径：深度优先重放，将整个祖先链的
`BlockUndo` 放进栈，遍历结束后逐层撤销。它既不遵守新字节预算，也可能需要
已经被淘汰的 sealed twig 叶堆。上一轮的重启样本没有超过该叶堆窗口。

本轮先加入连续 **4 块 × 147,000 次状态更新**的持久化回归。提交成功、WAL 完整，
关闭后重新打开却得到 `Undo(Ahead { prev: 1, next: 147001 })`。
见[修复前日志](benchmarks/20260914-qmdb-streaming-recovery/before.log)。

现在启动校验按父先于子的顺序遍历，沿用运行中的 `move_tree_to`、undo 条数/字节预算
及认证 base 加载。每个目标块仍实际应用完整操作并核对根；不把 WAL checksum
或已知 block hash 当成执行状态验证。分支间移动及结束回到 base 时，缺少可回退的
叶堆就加载已认证起点并重放。遍历队列只保存 hash/depth，不再保留整条链的 undo 栈。
已认证 base hash 也不能作为新块记录从 checkpoint 混入。

最终回归在上述长链之外加入 sibling，验证重新打开后的各个根、历史切换和回到 base。
旧 version-2 checkpoint 与 WAL 的相同重复记录继续兼容；错根分支和重新定义 base
的 checkpoint 会被拒绝。

## 编码数据不再整文件驻留

此前打开 checkpoint 和 WAL 都先 `std::fs::read`，再解码到 `blocks`。大日志的
完整编码副本会与已解码操作同时占用内存；WAL 每个新记录还额外 clone 一份 block。

现在：

- checkpoint 通过带缓冲的 reader 使用相同 bincode 编码配置解码。
- WAL 按 `length / payload / checksum` 逐帧读取，保留一个 payload 缓冲。
  长度仍受 64 MiB 上限约束；使用 exact reserve 避免增长策略将容量倍增到上限以上。
  reader 另有 128 KiB 缓冲。该限制只针对编码帧缓冲，不是总内存限制。
- 每个完整帧先验证 checksum，再解码、检查相同 block hash 是否被重新定义，
  最后直接移入 `blocks`，省去新记录的额外 clone。
- 单独读取 length 的首字节，区分干净 EOF 与半截 length。只有不完整的最后一帧
  被截回前一条完整记录边界，并保持原有 `sync_all`；校验和损坏、超长声明、
  完整但无法解码的 payload 或冲突记录仍报错，不当作断尾截掉。

逐字节断尾测试覆盖第二条记录的每个截断位置，包括 length、payload、checksum，
确认第一条保持不变。完整重复记录、完整后续记录和各类损坏另行检查；发生冲突时，
已读入的原记录及磁盘字节不被替换。

## 验证与剩余边界

- 完整 node QMDB 测试：**48 passed、6 ignored**（显式测量）。
- workspace all-targets clippy `-D warnings`、`git diff --check` 通过。
- [源码与日志证据](benchmarks/20260914-qmdb-streaming-recovery/)。仍使用固定且补丁化的
  reth `23316e3ff8adca8c3bd5085ff0565fcae019202a` 验证。

没有宣称这使总内存有界或提供新的 TPS 数字：已解码的 `blocks` 操作仍全量驻留，
启动遍历索引仍随记录数增长，冷分支重放也可能很慢。本轮消除了额外编码文件副本与
无界祖先 undo 栈，并修复了已复现的重启失败；将冷操作移出常驻内存仍待继续实现。
既有“base 下移后不可达记录”的处理规则未在此轮改写。

本地上游引用复查未变：n42-rs `466c1839791afadba7f1dd4d48c5518440dcafba`，
gov5 cached origin/main `a8e353cac7a00c7f6ca2816cc8e4634c45a4f337`。未声称刷新远端。
重新探测 loopback socket 仍得到 EPERM，因此真实四节点连续多窗口与联网重启追赶
尚未完成，目标保持进行中。
