# tgreader

macOS Telegram聊天记录读取 CLI 工具。直接从本机加密数据库中解密并读取Telegram聊天记录，无需手机备份。

## 快速开始

```bash
# 一键安装
brew install xiaotianxt/tgreader/tgreader

# 1. 从Telegram进程内存中提取数据库密钥（需 sudo）
sudo tgreader keys

# 2. 解密所有数据库
tgreader decrypt

# 3. 查看聊天列表
tgreader sessions

# 4. 读取聊天记录
tgreader messages "联系人名称"

# 5. 导出聊天记录
tgreader export "联系人名称" --format txt
```

## 安装

### 前置要求
- macOS（支持Telegram 4.x）
- SIP 需部分关闭或Telegram重新签名（用于 `task_for_pid`）

### 方式一：Homebrew 安装

```bash
brew install xiaotianxt/tgreader/tgreader
```

安装完成后，`tgreader` 命令即可全局使用。如需从最新源码安装（开发版）：

```bash
brew install --HEAD xiaotianxt/tgreader/tgreader
```

### 方式二：从源码安装

前置要求：
- [Rust](https://rustup.rs/) 工具链
- Xcode Command Line Tools（`xcode-select --install`）

```bash
git clone https://github.com/xiaotianxt/tgreader.git
cd tgreader
make install
```

或用一键安装脚本（会自动配置 Claude Code skill）：

```bash
curl -fsSL https://raw.githubusercontent.com/xiaotianxt/tgreader/main/scripts/install.sh | bash
```

### 方式三：Claude Code Skill

安装后，在 Claude Code 中可使用 `/tgreader` 加载技能，AI 即可理解并使用 tgreader：

```bash
# 安装脚本会自动配置 skill，或在 Claude Code 中直接输入：
/tgreader
```

skill 配置位于 `~/.claude/settings.json`，也可手动加载项目中的 `.claude/settings.json`。

## 使用流程

### 1. 提取密钥

确保Telegram正在运行，执行：

```bash
sudo tgreader keys
```

这会扫描Telegram进程内存，自动匹配数据库文件的加密密钥，生成 `all_keys.json`。

### 2. 解密数据库

```bash
tgreader decrypt
```

自动检测Telegram数据库目录，使用提取的密钥解密全部 `.db` 文件。解密后的文件存放在 `decrypted/` 目录。

### 3. 查看会话

```bash
tgreader sessions
```

按消息数排序显示所有聊天会话，包含会话名称、消息数和时间范围。

### 4. 读取消息

```bash
# 按昵称/备注搜索联系人
tgreader messages "张三"

# 指定消息数量
tgreader messages "张三" --limit 20

# 分页查询
tgreader messages "张三" --limit 10 --offset 100

# 关键词搜索
tgreader messages "张三" --search "项目讨论"
```

### 5. 全局搜索

```bash
tgreader search "关键词"
```

跨所有会话搜索消息内容。

### 6. 导出

```bash
# 导出为文本
tgreader export "张三" --format txt

# 导出为 CSV（Excel 兼容）
tgreader export "张三" --format csv

# 导出为 JSON
tgreader export "张三" --format json

# 导出全部格式
tgreader export "张三"
```

## 命令参考

| 命令       | 功能                         |
|-----------|------------------------------|
| `keys`    | 从内存提取数据库密钥（需 sudo） |
| `decrypt` | 解密全部加密数据库             |
| `sessions`| 按消息数排序列出所有会话       |
| `messages`| 分页读取指定会话消息           |
| `search`  | 跨会话全文搜索                |
| `export`  | 导出聊天记录到文件             |

## 技术原理

Telegram macOS 版使用 **SQLCipher 4** 加密 SQLite 数据库，存储在：

```
~/Library/Containers/com.telegram.xinTelegram/Data/.../db_storage/
```

tgreader 的工作流程：

1. **内存扫描** — 通过 Mach VM API（`task_for_pid` + `mach_vm_read`）扫描Telegram进程内存，查找 WCDB 格式的密钥（`x'<64位十六进制密钥><32位十六进制盐>'`）
2. **密钥匹配** — 将扫描到的密钥与数据库文件的盐值交叉匹配
3. **数据库解密** — 逐页 AES-256-CBC 解密，HMAC-SHA512 验证完整性
4. **数据读取** — 读取联系人表（`contact.db`）和消息表（`message_*.db`），将 tgid 解析为联系人昵称

### 加密参数

| 参数             | 值                           |
|-----------------|------------------------------|
| 加密算法         | AES-256-CBC                  |
| 页面大小         | 4096 字节                    |
| 保留区大小       | 80 字节（16 IV + 64 HMAC）   |
| HMAC 算法        | HMAC-SHA512                  |
| 密钥派生         | PBKDF2-HMAC-SHA512，2 轮迭代 |
| 格式             | WCDB（Telegram Custom Database）|

## 项目结构

```
tgreader/
├── src/
│   ├── main.rs       # CLI 入口（clap）
│   ├── scanner.rs    # 密钥提取子进程调用
│   ├── decrypt.rs    # SQLCipher 4 解密
│   ├── db.rs         # 数据库读取与查询
│   └── export.rs     # 消息导出（TXT/CSV/JSON）
├── vendor/
│   └── find_all_keys_macos.c  # Mach VM 内存扫描器
├── .github/
│   └── workflows/
│       ├── ci.yml             # CI（build + test）
│       └── release.yml        # 发布预编译二进制
├── CLAUDE.md         # AI 辅助开发文档
└── Cargo.toml
```

## AI Native

本项目设计为 AI Native——配合 Claude Code 等 AI 编程助手使用体验最佳：

- **`CLAUDE.md`** 包含完整的项目上下文，AI 可直接理解架构并辅助开发
- 所有错误信息清晰可读，AI 能准确诊断问题
- 命令输出结构化，便于 AI 解析和处理

```bash
# 在 Claude Code 中直接提问：
# "帮我读一下张三今天的聊天记录"
# "搜索关于项目讨论的消息"
# "把所有聊天导出为 JSON"
```

## 隐私说明

- 所有操作在本地完成，不上传任何数据
- 导出的聊天记录仅保存在你指定的目录
- 数据库密钥仅存在于生成的 `all_keys.json` 中
- 建议使用后将 `all_keys.json` 和 `decrypted/` 目录安全删除

## License

MIT
