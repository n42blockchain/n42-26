# Windows 21 节点测试网 — 安装与运行指南

## 系统要求

| 项目 | 最低 | 推荐 |
|------|------|------|
| RAM  | 16 GB | 32 GB |
| CPU  | 8 核 | 16 核 |
| 磁盘 | 20 GB 可用 | 50 GB SSD |
| OS   | Windows 10 21H2+ | Windows 11 |

21 节点内存估算：21×150MB(节点) + 3GB(Blockscout) + 2GB(WSL2 OS) ≈ 8-9 GB

---

## 一次性安装（30 分钟）

### 1. 启用 WSL2 + Ubuntu

以**管理员身份**打开 PowerShell：

```powershell
wsl --install -d Ubuntu-22.04
```

安装完成后**重启电脑**，Ubuntu 会自动启动并要求设置用户名/密码。

### 2. 配置 WSL2 内存限制

默认 WSL2 只用 50% 内存，21 节点需要放开上限。

在 PowerShell 中执行：

```powershell
notepad "$env:USERPROFILE\.wslconfig"
```

写入以下内容（根据你的实际 RAM 调整）：

```ini
[wsl2]
memory=20GB
processors=12
swap=8GB
```

保存后重启 WSL2：

```powershell
wsl --shutdown
```

### 3. 安装 Docker Desktop

1. 下载：https://www.docker.com/products/docker-desktop/
2. 安装时勾选 **"Use WSL 2 instead of Hyper-V"**
3. 安装完成后打开 Docker Desktop → Settings → Resources → WSL Integration：
   - 勾选 **Enable integration with my default WSL distro**
   - 勾选 **Ubuntu-22.04**
4. Apply & Restart

### 4. 在 WSL2 中安装 Rust 和依赖

打开 **Ubuntu 22.04 终端**（从开始菜单搜索）：

```bash
# 更新系统
sudo apt-get update && sudo apt-get upgrade -y

# 安装编译依赖
sudo apt-get install -y \
    build-essential pkg-config libssl-dev \
    python3 python3-pip python3-venv git curl

# 安装 Rust（如果没有）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# 验证
rustc --version   # 应 >= 1.93
docker --version  # 验证 Docker 集成
```

### 5. 克隆项目（必须在 WSL2 文件系统内）

**重要**：项目必须在 WSL2 的 `/home/` 下，不能放 `/mnt/c/`，否则编译/运行速度会慢 10 倍以上。

```bash
# 方式 A：重新克隆
cd ~
git clone <你的仓库地址> n42-26
cd n42-26

# 方式 B：如果项目已在 Windows，复制进来
cp -r /mnt/c/Users/你的用户名/Documents/n42/n42-26 ~/n42-26
cd ~/n42-26
```

---

## 运行测试网

### 标准启动

```bash
cd ~/n42-26
chmod +x scripts/testnet-21node.sh
./scripts/testnet-21node.sh
```

首次运行会：
1. 编译 Rust 二进制（约 5-10 分钟）
2. 生成 genesis.json
3. 启动 21 个节点（约 25 秒）
4. 等待出块（约 60-90 秒）
5. 启动 Blockscout、TX 生成器、手机模拟器

### 常用变体

```bash
# 清除所有数据重新开始
./scripts/testnet-21node.sh --clean

# 只跑节点，不启 Blockscout 和 TX 生成器（省内存）
./scripts/testnet-21node.sh --no-explorer --no-tx-gen

# 跑节点 + Blockscout，不跑手机模拟器
./scripts/testnet-21node.sh --no-mobile-sim --no-tx-gen
```

---

## 验证测试网正常运行

### 检查区块高度

```bash
curl -s http://127.0.0.1:18545 \
  -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'
# 期望：{"result":"0x5",...}  表示第 5 块
```

### 检查共识状态

```bash
curl -s http://127.0.0.1:18545 \
  -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"n42_consensusStatus","params":[],"id":1}'
```

### 查看所有节点状态

```bash
for i in $(seq 0 20); do
  port=$((18545 + i))
  bn=$(curl -s http://127.0.0.1:$port \
    -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' 2>/dev/null | \
    python3 -c "import sys,json; print(int(json.load(sys.stdin).get('result','0x0'),16))" 2>/dev/null || echo "?")
  printf "v%02d (:%d): block=%s\n" $i $port $bn
done
```

### 访问 Blockscout

在 Windows 浏览器中打开：http://localhost:3000

---

## 日志文件位置

```
~/n42-testnet-data-21/
├── validator-0.log     ← 节点 0 日志
├── validator-1.log
├── ...
├── validator-20.log    ← 节点 20 日志
├── tx-generator.log    ← TX 生成器
├── mobile-sim.log      ← 手机模拟器
├── monitor.log         ← 错误监控
├── testnet-errors.log  ← 汇总错误事件
├── testnet-crashes.log ← 节点崩溃记录
├── genesis.json
└── test-accounts.json  ← 10 个测试账户私钥
```

---

## 常见问题

### WSL2 内存不足 (OOM)

症状：节点被 kill，`validator-*.log` 中出现 `Killed`

解决：增大 `.wslconfig` 中的 `memory=` 值，然后 `wsl --shutdown`

### `docker: command not found` in WSL2

在 Docker Desktop → Settings → WSL Integration 中启用 Ubuntu-22.04

### Blockscout 一直 "Connecting to database"

等待约 2-3 分钟让 PostgreSQL 完成初始化（可用 `docker compose -p n42-21node-blockscout logs db` 查看）

### 节点无法出块（等 180s 超时）

```bash
# 查看节点 0 日志末尾
tail -50 ~/n42-testnet-data-21/validator-0.log

# 常见原因：
# 1. 端口已被占用 — lsof -i :18545
# 2. 内存不足 — free -h
# 3. 编译的是 debug 而非 release — 确认使用 --release
```

### 端口冲突（18545 已被占用）

```bash
# 查看占用
ss -tlnp | grep 18545

# 或者用另一个基础端口（修改脚本 BASE_HTTP_RPC=28545 等）
```
