# H2 验收绑定原始 Gov5 头部

日期：2026-09-14，接续 [devlog-157](devlog-157-h2-successful-receipts-20260914.md)。
继续保持 H2 共识、二叉树 QMDB 状态读取与原有执行校验。
**真实 4 节点 1M 成功提交 TPS 尚未实测达标。**

## 上一轮还存在的缺口

回执根重算能发现回执损坏，但如果 RPC 同时报告一个假的 receiptsRoot 和与之匹配
的回执，仅检查这两者相等仍不够。必须把这个 root 绑定到 H2 报告的 block hash。
同理，stateRoot、transactionsRoot、number 和 gasUsed 不能只相信 JSON 字段。

当前 reth 的 `debug_getRawHeader` 从 Alloy header 重新编码。Gov5 原始头部包含
nil 占位字段及 mobile-registry root，Alloy 视图不能完整保留；拿重编码结果算 hash
会丢失这些信息。不能在验证失败后忽略未知尾字段或补造近似头部。

## 实现

新增只读 RPC `n42_nativeHeader(blockHash)`，返回已有 registry 中的精确原始 RLP。
服务端先用现有 Gov5 decoder 检查，再要求原样编码相等且 Keccak hash 对应请求。
未知/已淘汰记录返回 null；畸形记录返回 RPC 错误，不返回空 header 或另一种编码。

审计新增独立的严格 RLP 解析，并核验原始字节的 Keccak-256：

- 必须恰好一个 list，15–23 个非 list 字段；拒绝断尾、尾随数据、非最短 RLP 编码、
  非规范整数、错误固定长度或错误 optional hash 长度。
- 保留 nil 占位和 mobile root，直接对全部原始字节算 hash，不经过 JSON 重编码。
- 将 hash 与 H2 报告的提交块绑定，并核对 JSON 的 parentHash、stateRoot、
  transactionsRoot、receiptsRoot、number、gasUsed 与原始头部对应字段一致。
- 开始/结束捕获保存已核验原始头部；审计重新核验这些已保存证据。共同区间内每块
  都从每个节点取得并核验原始头部，再核验回执。逐块报告保存原始 RLP、重算 hash
  及 header/receipt 核验节点数。

边界和结果 schema 升为 **4**，不接受旧的 receipt-only schema 3。
通过门槛仍为成功提交 TPS；没有改变分母、放宽 QMDB only/WAL 门槛或按节点求和。
头部捕获工作计入原有边界时间。压测前的捕获会验证原始头部接口是否可用。

## 可用性和资源边界

该 RPC 复用既有 **8,192 条** native header registry，没有新增无界缓存或磁盘写入。
它不是持久化归档：进程重启后尚未重新见到的头部、或已淘汰的老头部可能不可用。
此时资格审计失败，不从 lossy Alloy header 回退，不把缺证据当通过。
更长窗口或离线重启审计需要后续持久化/导出原始证据方案。

重算 hash 只认证头部字节及本次测量使用字段，不替代全套 fork/header 规则验证。
原始头部内的 H2 extra data 被 hash 覆盖，但本轮尚未验证其中的 BLS 签名或把它
解析成独立的提交证明。交易根现在绑定到头部，尚未从交易内容重新计算。

## 验证

- Python 验收测试：**35 passed**。合成链现使用实际原始头部 hash 和 parent 链，
  无需 mock 掉 header verifier，原有成功/失败回执、四节点身份和时间窗口回归继续通过。
- 新增反例覆盖各节点缺失/替换原始头部、所有 RPC 同时报假 root/gas、篡改捕获证据、
  不合法 RLP/字段及旧 schema。都拒绝结果。
- chain 94 高度 **13,560,375** 的既有源码 fixture，通过独立 Python 解码与 hash 核验：
  `0x0e37dae9d0cbf1c8e09c335654dc4cae3e18760dade40039e0e693368cc796d7`。
  保留其 nil slots/mobile root；删除尾字段后不能匹配原 hash。
  这是复用已有 fixture，未声称重新连接链上获取。
- node RPC 测试：**18 passed**，含原始 optional slots 往返、未知 hash、畸形 registry 记录。
- 生产 Gov5 header codec 测试：**7 passed**。
- workspace all-targets clippy `-D warnings`、shell/Python 语法及 `git diff --check` 通过。

[源码、日志、头部 fixture 和合成报告](benchmarks/20260914-h2-native-header/)。
仍使用固定补丁 reth `23316e3ff8adca8c3bd5085ff0565fcae019202a` 验证。
没有改变共识协议或提升任何已测 TPS 数字。

剩余验收包括独立 H2/BLS 提交证明、交易根与业务状态核对、目标负载下审计成本、
真实四节点连续窗口、重启追赶及耐久证据。这些都不能由本轮离线 fixture 代替；
完整目标继续进行中。
