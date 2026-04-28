---
name: tgreader
description: Use when the user asks to use or maintain tgreader, a macOS Telegram local chat reader CLI. Supports extracting database keys, decrypting local Telegram databases, listing sessions, reading and searching messages, exporting chats as txt/csv/json, exporting cached media, installing the tool, and troubleshooting common tgreader workflows.
---

# tgreader

## 能做什么

tgreader 是一个 macOS 本地Telegram聊天记录读取 CLI。它可以：

- 从正在运行的Telegram进程中提取数据库密钥。
- 增量解密本地Telegram数据库到 `decrypted/`。
- 列出会话，并按显示名、备注、别名、tgid 或群 ID 匹配联系人。
- 读取指定会话的最新、最早、分页或按时间过滤后的消息。
- 在单个会话内搜索，或跨全部会话搜索。
- 导出 `txt`、`csv`、`json`。
- 使用 `--media-dir` 尝试导出本地缓存图片、视频和表情。
- 从源码构建、安装和测试 CLI。

除非用户明确要求移动或上传文件，否则聊天数据只应留在本机。

## 标准流程

1. 确认用户在 macOS 上，并且已安装Telegram。
2. 如果缺少密钥或密钥过期，让用户保持Telegram运行，然后执行：

   ```bash
   sudo tgreader keys
   ```

3. 解密或刷新本地缓存：

   ```bash
   tgreader decrypt
   ```

4. 查找目标会话：

   ```bash
   tgreader sessions
   ```

5. 读取或搜索消息：

   ```bash
   tgreader messages "联系人或群名" --limit 50
   tgreader messages "联系人或群名" --since today --limit 100
   tgreader messages "联系人或群名" --search "关键词" --limit 50
   tgreader search "关键词"
   ```

6. 用户需要文件时再导出：

   ```bash
   tgreader export "联系人或群名" --format txt
   tgreader export "联系人或群名" --format json --output exported
   tgreader export "联系人或群名" --format json --media-dir exported/media
   ```

`sessions`、`messages`、`search`、`export` 在读取 `decrypted/` 前会自动尝试静默增量刷新缓存。如果密钥或实时Telegram数据库不可用，它们仍可读取已有解密缓存。

## 命令参考

- `sudo tgreader keys`：提取数据库密钥到 `all_keys.json`；需要Telegram正在运行，并且 macOS 允许读取其进程。
- `tgreader decrypt`：增量解密变化过的数据库到 `decrypted/`。
- `tgreader decrypt --full`：强制全量重解。
- `tgreader decrypt --since 1h --verbose`：只处理近期数据库变化，并显示进度。
- `tgreader sessions --top 50`：按消息数列出主要会话。
- `tgreader messages "name"`：显示某个会话的最新消息。
- `tgreader messages "name" --head --limit 20`：显示最早消息。
- `tgreader messages "name" --tail --limit 20`：按时间顺序显示最新消息。
- `tgreader messages "name" --offset 100 --limit 50`：分页查看历史消息。
- `tgreader messages "name" --since yesterday`：按时间过滤消息。
- `tgreader messages "name" --search "query"`：在单个会话内搜索。
- `tgreader search "query" --limit 50`：跨会话搜索。
- `tgreader export "name" --format txt|csv|json --output exported`：导出一个会话。
- `tgreader export "name" --format json --media-dir exported/media`：导出消息并尝试导出缓存媒体。

时间过滤支持 ISO 日期如 `2026-04-28`，日期时间如 `2026-04-28 09:30:00`，以及相对表达式 `5min`、`1h`、`2d`、`1w`、`today`、`yesterday`。

## 开发流程

沿用当前简单的 Rust/C 结构：

- `src/main.rs`：CLI 命令定义和顶层流程。
- `src/scanner.rs`：封装 `scanner_macos` 调用。
- `vendor/find_all_keys_macos.c`：macOS 进程内存扫描器。
- `src/decrypt.rs`：SQLCipher/WCDB 数据库解密。
- `src/db.rs`：联系人、会话、消息读取和搜索。
- `src/message.rs`：消息类型解析和展示文本。
- `src/media*.rs`：媒体元信息解析、缓存查找和媒体解密。
- `src/export.rs`：txt/csv/json/媒体导出。

常用检查：

```bash
cargo test
cargo build
make build
```

用户需要安装到个人目录时用 `make install-local`；只有用户明确要安装到 `/usr/local/bin` 时才用 `make install`。

## 维护规则

- CLI 行为要明确，输出要方便人和 AI 助手解析。
- `all_keys.json`、`decrypted/`、导出的聊天和媒体都视为敏感本地数据。
- 除非用户明确要求，不上传、不粘贴、不移动聊天数据。
- 修改密钥提取时，保持 macOS Telegram进程扫描兼容。
- 修改解密逻辑时，保留完整性校验，避免写出损坏的 SQLite。
- 修改消息读取时，同时处理 TEXT、BLOB 和压缩的 `WCDB_CT_message_content`。
- 修改群聊处理时，保留发送者 tgid 解析和联系人显示名解析。
- 优先做聚焦的小改动，不引入不必要的抽象。
