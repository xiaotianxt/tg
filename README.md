# tgreader

macOS 本地Telegram聊天记录读取 CLI。它用于在本机提取、解密、查询和导出Telegram聊天记录，无需手机备份。

## 能做什么

- 从正在运行的Telegram进程中提取数据库密钥
- 增量解密本地Telegram数据库到 `decrypted/`
- 列出会话，按联系人、群名、tgid 读取消息
- 按关键词或时间范围搜索聊天记录
- 导出 `txt`、`csv`、`json`
- 可选导出本地缓存中的图片、视频和表情
- 全程本地运行，不上传聊天数据

面向 AI/自动化助手的能力说明见 [SKILL.md](SKILL.md)。

## 安装

### Homebrew

```bash
brew install xiaotianxt/tgreader/tgreader
```

安装开发版：

```bash
brew install --HEAD xiaotianxt/tgreader/tgreader
```

### 源码安装

需要 Rust 工具链和 Xcode Command Line Tools。

```bash
git clone https://github.com/xiaotianxt/tgreader.git
cd tgreader
make install
```

安装到用户目录：

```bash
make install-local
```

一键安装脚本：

```bash
curl -fsSL https://raw.githubusercontent.com/xiaotianxt/tgreader/main/scripts/install.sh | bash
```

## 快速开始

先确保Telegram正在运行。

```bash
sudo tgreader keys
tgreader decrypt
tgreader sessions
tgreader messages "联系人或群名" --limit 50
```

`keys` 需要读取Telegram进程内存，所以可能需要 sudo、部分关闭 SIP，或对Telegram重新签名以允许 `task_for_pid`。

## 常用命令

提取密钥：

```bash
sudo tgreader keys
```

解密数据库：

```bash
tgreader decrypt
tgreader decrypt --full
tgreader decrypt --since 1h --verbose
```

查看会话：

```bash
tgreader sessions
tgreader sessions --top 50
```

读取消息：

```bash
tgreader messages "张三"
tgreader messages "张三" --limit 100
tgreader messages "张三" --since today
tgreader messages "张三" --since yesterday
tgreader messages "张三" --search "项目"
tgreader messages "张三" --head --limit 20
tgreader messages "张三" --tail --limit 20
tgreader messages "张三" --offset 100 --limit 50
```

导出最近图片到可直接打开的本地文件：

```bash
tgreader image "张三"
tgreader image "张三" --list --limit 20
tgreader image "张三" --index 3
tgreader image "张三" --all --limit 10 --output exported/images
```

全局搜索：

```bash
tgreader search "关键词"
tgreader search "关键词" --limit 50
```

导出聊天：

```bash
tgreader export "张三" --format txt
tgreader export "张三" --format csv --output exported
tgreader export "张三" --format json --output exported
```

导出聊天并尝试导出本地缓存媒体：

```bash
tgreader export "张三" --format json --output exported --media-dir exported/media
```

媒体导出依赖Telegram本地缓存；缓存不存在时会跳过对应文件。

## 时间过滤

`--since` 支持：

- 日期：`2026-04-28`
- 日期时间：`2026-04-28 09:30:00`
- 相对时间：`5min`、`1h`、`2d`、`1w`
- 命名时间：`today`、`yesterday`

## 读取缓存

`sessions`、`messages`、`search`、`export` 在读取前会尝试静默增量刷新 `decrypted/`。如果当前无法访问Telegram数据库或没有可用密钥，它们会继续读取已有的解密缓存。

## 日志

聊天记录、会话表格、搜索结果等命令结果写到 stdout。运行状态、警告和错误写到 stderr 日志。

默认日志等级是 `info`。可以用 `TGREADER_LOG` 或 `RUST_LOG` 调整：

```bash
TGREADER_LOG=warn tgreader decrypt
TGREADER_LOG=debug tgreader messages "张三"
```

## 开发

```bash
make build
cargo test
cargo build
```

项目入口是 `src/main.rs`。主要模块：

- `src/scanner.rs`：调用密钥扫描器
- `src/decrypt.rs`：数据库解密
- `src/db.rs`：会话、联系人和消息查询
- `src/message.rs`：消息类型解析
- `src/media*.rs`：媒体元信息、缓存查找和解密
- `src/export.rs`：导出
- `vendor/find_all_keys_macos.c`：macOS Telegram进程扫描器

## 隐私

- `all_keys.json` 包含数据库密钥
- `decrypted/` 包含解密后的数据库
- `exported/` 或自定义导出目录包含聊天内容和媒体

这些文件都应视为敏感数据。使用后可按需删除。

## License

MIT
