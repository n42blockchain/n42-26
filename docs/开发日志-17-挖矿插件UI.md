# 开发日志 17 — N42 挖矿插件 UI (n42_mining Flutter Package)

> 日期：2026-03-04

## 设计决策

### 为什么选择独立 Flutter Package 而非集成在 n42appv2 内？

1. **可嵌入性**：任何第三方 App 只需 `N42MiningWidget(...)` 一行即可嵌入挖矿功能
2. **解耦维护**：挖矿核心逻辑与钱包 App 解耦，独立版本管理
3. **多入口**：同时支持 full dashboard、mini card、headless 三种模式

### 状态管理：Riverpod 3.x Notifier API

- n42appv2 使用 ChangeNotifier + Mixin 混合模式（遗留风格）
- n42_mining 统一使用 Riverpod 3.x `Notifier` + `NotifierProvider` API
- 所有 provider 默认 `isAutoDispose: true`，widget 销毁时自动清理资源
- `MiningSettingsProvider` 不 auto-dispose（用户设置需要持久化）

### FFI 绑定：直接使用 `package:ffi` + C ABI

- 绑定 `n42-mobile-ffi` 的 8 个 C FFI 函数
- 使用 `Pointer<Uint8>` 传递字符串（避免 `Utf8` 兼容性问题）
- 单例模式确保只初始化一次 VerifierContext
- 支持 Android (.so) / iOS (static) / macOS (.dylib) / Windows (.dll)

### 主题系统：InheritedWidget + 工厂模式

- `N42MiningThemeData` 支持 dark/light 工厂 + 完全自定义
- `N42ThemeScope` InheritedWidget 向下传播
- `context.miningTheme` 扩展方法便捷获取

## 实施细节

### Package 结构（28 个文件）

```
n42_mining/
├── lib/
│   ├── n42_mining.dart              # Public API 导出
│   ├── ffi/
│   │   └── n42_mobile_bindings.dart # Rust FFI 绑定 (8 函数)
│   └── src/
│       ├── n42_mining_widget.dart    # 顶层入口组件
│       ├── core/
│       │   ├── mining_engine.dart    # 挖矿引擎 (poll+verify 循环)
│       │   ├── mining_state.dart     # Riverpod providers (7 个)
│       │   ├── staking_service.dart  # 质押/解押服务
│       │   ├── reward_tracker.dart   # 奖励追踪
│       │   └── node_connection.dart  # QUIC 连接管理
│       ├── models/ (4 个数据模型)
│       ├── widgets/ (9 个 UI 组件)
│       ├── pages/ (4 个页面)
│       └── theme/
│           └── mining_theme.dart     # 可定制主题
├── test/
│   └── n42_mining_test.dart          # 8 个单元测试
└── example/
    └── lib/main.dart                 # 示例 App
```

### Riverpod Provider 架构

| Provider | 类型 | 状态 |
|----------|------|------|
| `miningConfigProvider` | Provider | N42MiningConfig |
| `miningEngineProvider` | Provider | MiningEngine |
| `miningStatusProvider` | NotifierProvider | MiningStatus |
| `verificationFeedProvider` | NotifierProvider | List<LastVerifyInfo> |
| `nodeConnectionProvider` | NotifierProvider | ConnectionQuality |
| `stakingPositionsProvider` | NotifierProvider | List<StakingPosition> |
| `rewardSummaryProvider` | NotifierProvider | RewardSummary |
| `miningSettingsProvider` | NotifierProvider | MiningSettings |

### FFI 函数映射

| Rust FFI | Dart 方法 | 用途 |
|----------|-----------|------|
| `n42_verifier_init` | `init(chainId)` | 初始化 BLS 密钥 + tokio 运行时 |
| `n42_connect` | `connect(host, port)` | QUIC 连接 StarHub |
| `n42_poll_packet` | `pollPacket()` | 非阻塞轮询验证包 |
| `n42_verify_and_send` | `verifyAndSend(data)` | EVM 重放 + 签名 + 发送 |
| `n42_get_pubkey` | `getPubkey()` | 获取 BLS 公钥 (48B) |
| `n42_get_stats` | `getStats()` | 验证统计 JSON |
| `n42_last_verify_info` | `getLastVerifyInfo()` | 最后验证详情 JSON |
| `n42_disconnect` | `disconnect()` | 断开连接 |

## 遇到的问题及解决方案

### 1. Riverpod 3.x API 变更
- **问题**：`StateNotifierProvider` 和 `AutoDisposeNotifierProvider` 在 riverpod 3.2.1 中已移除
- **解决**：全部改用 `NotifierProvider` + `Notifier`，通过 `isAutoDispose: true` 参数控制

### 2. FFI `Utf8` 类型不可用
- **问题**：`dart:ffi` 中 `Utf8` 不再作为类型参数使用
- **解决**：改用 `Pointer<Uint8>` 传递字符串，手动编码/解码 UTF-8，引入 `package:ffi` 提供 `calloc`

### 3. BigInt.zero 非 const
- **问题**：`BigInt.zero` 不能作为 const 构造函数的默认参数
- **解决**：改用 nullable 参数 + `??` 语法：`BigInt? pendingRewards` → `pendingRewards ?? BigInt.zero`

## 完成状态

- [x] Phase 1: Package 框架搭建
- [x] Phase 2: 数据模型 + FFI 绑定 + 状态管理
- [x] Phase 3: 9 个核心 UI 组件
- [x] Phase 4: 4 个页面组装
- [x] Phase 5: 主题 + 示例 App + 静态分析
- [x] `flutter analyze` — 零问题
- [x] `flutter test` — 8/8 通过

## 后续计划

- [ ] 实际连接 `n42-mobile-ffi` 编译产物测试 FFI 调用
- [ ] 质押合约 ABI 编码实现（`staking_service.dart` 中的 TODO）
- [ ] 响应式布局：iPad 横屏 + macOS/Windows 桌面适配
- [ ] 后台挖矿：Android ForegroundService / iOS BGTaskScheduler
- [ ] 集成到 n42appv2 作为 package 依赖
- [ ] 国际化（中/英/日/韩）
