# Claude Code 项目配置

## Git 提交规则

- **重要**: 所有 git 提交不要包含 "Claude" 或 "Co-Authored-By: Claude" 等字样
- 提交命令模板：`GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" -m "message"`

## 项目结构

- **当前项目**: /Users/jieliu/Documents/n42/n42-26/ (N42 区块链 Rust 全节点，含 crates/)
- Flutter 钱包 App: /Users/jieliu/Documents/n42/n42appv2 (主 App)
- Chat 插件: /Users/jieliu/Documents/n42/n42_chat (聊天功能模块，独立 Flutter package)
- 储值卡应用: /Users/jieliu/Documents/n42/minto/ (链上储值卡，TypeScript 全栈：contracts + backend + mobile + web-admin)
- 技术栈: Rust（主链节点）、TypeScript/Node.js（minto 后端/合约）、Flutter/Dart（钱包 + 聊天）
- **开始工作前确认当前目录属于哪个子项目，git remote 确认是对的 repo**

## Code Quality

- 生成代码后立即运行静态分析（`cargo check` / `cargo clippy`），不等用户询问。

## Communication

- 用户使用中文（普通话）交流，始终用中文回复，除非用户切换到英文。
