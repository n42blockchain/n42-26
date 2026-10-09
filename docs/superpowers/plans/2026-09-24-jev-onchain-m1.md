# Jev 链上决策 M1 实施计划

**目标：**交付测试网可部署的异步决策合约、可运行的 Jev 网关、供应用使用的 SDK 和提案分流演示。

**依据：**`docs/superpowers/specs/2026-09-24-jev-onchain-decisions-design.md`。用户已要求按该方案实施并测试。

**边界：**不改变验证者共识/EVM；签名网关是首期信任根；API key 仅在网关。现有工作区有大量用户改动，所有修改限制于本功能新增文件及必要的 workspace member。

## 文件结构

- `contracts/decision/DecisionHub.sol`：模板注册、请求费用、EIP-712 结果校验、退款、消费。
- `contracts/decision/examples/ProposalRouter.sol`：公开提案分流示例。
- `bin/n42-decision-relay/`：请求/响应类型、Jev HTTP client、结果校验、签名与任务命令。
- `sdk/decision-ts/`：输入/模板 hash、ABI、请求及结果解码的 TypeScript 兼容 SDK。
- `examples/decision-dapp/`：可运行的用户示例和部署说明。
- `docs/decision/`：边界、配置、合约和服务运行手册。

## 步骤

### 1. 协议类型与合约

- [x] 写状态/签名/退款/重复消费的 Solidity 用例源码。
- [x] 实现不可变模板版本、请求/结算、签名恢复、费用提取及到期退款。
- [x] 为应用增加对象与 requestId 的绑定示例。
- [ ] 运行 Solidity 编译和行为用例；当前环境缺少 forge/solc，测试源码已保留。

### 2. 网关核心

- [x] 先写 Jev 响应类型和量化边界的 Rust 单元测试并确认失败。
- [x] 实现明确类型检查、模型版本固定、第一份响应落盘、EIP-712 编码和签名。
- [x] 接入官方 `/v1/systemone` HTTP API，含有限退避；本机 socket 被沙箱禁止，未运行 mock server。
- [x] 实现链上事件确认、自动扫描、提交结果及游标；无链环境仅测事件解析和游标，未运行模拟 JSON-RPC。

### 3. SDK 与用户例子

- [x] 写并运行 hash/报价约束/钱包调用的 Node 测试。
- [x] 实现 TypeScript 可消费的 SDK 与公开提案演示。
- [x] 跑 SDK 测试、页面语法检查和示例配置一致性检查。

### 4. 集成检查

- [x] 校验合约和网关签名的域、字段与字节编码；Rust 覆盖链/地址错配，Solidity 用例待运行。
- [x] 运行 Rust、Node、格式/Clippy 检查；Solidity 工具当前不可用。
- [x] 在 `docs/decision/README.md` 记录实际通过的验证和缺少 API key/编译器/测试网的限制。
