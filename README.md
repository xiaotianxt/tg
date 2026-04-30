# tg

tg 是一个 macOS 本地Telegram聊天记录读取 CLI。它在你的 Mac 上读取Telegram桌面版本地数据库，提取密钥、解密数据库，然后按联系人或群名查询、搜索、导出聊天记录。

适合这些场景：

- 备份自己的Telegram聊天记录，不依赖手机备份。
- 快速找某个人、某个群、某个关键词的历史消息。
- 把聊天导出成 `txt`、`csv`、`json`，用于归档、整理或本地分析。
- 从本地缓存里导出图片、视频、表情等媒体文件。

默认数据只留在本机。`all_keys.json`、`decrypted/`、`exported/` 都是敏感文件，请当作聊天原文保存和处理。

## 先看效果

列出会话：

```bash
$ tg sessions --top 5
Rank Count    Time Range                                     Display Name           Username
------------------------------------------------------------------------------------------------------------------------
1    12843    2024-02-03 10:21 ~ 2026-04-28 09:41            产品讨论群              123456789@chatroom
2    8921     2023-11-18 08:12 ~ 2026-04-27 23:10            张三                    tgid_abcd1234
3    502      2025-06-01 14:33 ~ 2026-04-20 18:05            文件传输助手            filehelper

Total: 37 sessions
```

读取最近消息：

```bash
$ tg messages "产品讨论群" --limit 4
Chat with: 产品讨论群 (123456789@chatroom)
Showing latest 4 of 12843 messages

[2026-04-28 09:37]
我: 今天先把导出格式定下来
[2026-04-28 09:38]
李四: [图片 1280x720 245KB]
 这张是最新版
[2026-04-28 09:41]
王五: > 李四: [图片]
        这张可以放到 README 里

--- End of messages ---
```

导出聊天：

```bash
$ tg export "张三" --format json --output exported/zhangsan
Exported 8921 messages for 张三 (tgid_abcd1234)
Exported to:
  [json] exported/zhangsan/chat.json
```

导出最近一张本地缓存图片：

```bash
$ tg image "产品讨论群"
exported/images/123456789_chatroom_Image_3_0001.jpg
```

## 支持与限制

| 类别 | 当前支持 |
| --- | --- |
| 系统 | macOS，本机Telegram桌面版本地数据 |
| Telegram版本 | 主要面向 macOS Telegram 4.x；Telegram升级后如果数据库结构变化，可能需要跟进适配 |
| Telegram数据 | 本机 `db_storage` 中的聊天数据库；无需手机备份 |
| 会话匹配 | 联系人显示名、备注、别名、`tgid_...`、群 ID |
| 消息读取 | 文本、群聊发送者、系统提示、撤回提示、引用、链接、小程序、位置、文件卡片、图片/视频/语音/表情的可读摘要 |
| 搜索 | 全局关键词搜索，单个会话内关键词搜索 |
| 导出 | `txt`、`csv`、`json` |
| 媒体 | 尝试导出本地缓存图片、视频、表情；`.dat` 图片/视频会尝试解密 |
| 增量更新 | 默认只解密变化过的数据库 |

暂不支持或不保证：

- 不支持 Windows、iOS、Android、网页版Telegram。
- 不恢复Telegram本地数据库里已经没有的消息。
- 不保证导出所有媒体。Telegram没有缓存、缓存被清理、文件未下载时，只能显示消息摘要。
- `export --media-dir` 会尝试导出图片、视频、表情，但不导出语音音频和普通文件附件本体。
- 表情导出可能根据消息里的 URL 用 `curl` 下载；普通读取、解密、搜索不会上传聊天数据。
- `tggf` 表情转换需要本机可用的 `ffmpeg`。
- 不做 OCR、语义搜索、拼音搜索。
- 多账号或Telegram路径异常时，可能需要手动指定 `--db-dir`。

## 安装

### 先重新签名Telegram

退出Telegram后执行：

```bash
sudo codesign --force --deep --sign - /Applications/Telegram.app
```

如果你的Telegram路径不是 `/Applications/Telegram.app`，把路径改成实际的 App 路径，例如 `/Applications/Telegram.app`。

### Homebrew

```bash
brew install xiaotianxt/tap/tg
tg --version
```

### 源码安装

需要 Rust 工具链和 Xcode Command Line Tools。

```bash
git clone https://github.com/xiaotianxt/tg.git
cd tg
make install-local
```

`make install-local` 会把 `tg` 安装到 `~/.local/bin`。请确认 `~/.local/bin` 已经在 `PATH` 中。

如果要安装到 `/usr/local/bin`：

```bash
make install
```

如果没有 C 编译器：

```bash
xcode-select --install
```

## 第一次使用

1. 打开并登录 macOS Telegram。
2. 提取数据库密钥：

   ```bash
   sudo tg keys
   ```

   成功后当前目录会出现 `all_keys.json`。

3. 解密数据库：

   ```bash
   tg decrypt --verbose
   ```

   成功后当前目录会出现 `decrypted/`。

4. 看有哪些会话：

   ```bash
   tg sessions --top 30
   ```

5. 读取一个会话：

   ```bash
   tg "联系人或群名" --limit 50
   ```

后续使用时通常只需要：

```bash
tg "联系人或群名" --limit 50
tg search "关键词"
tg export "联系人或群名" --format json
```

`sessions`、`search`、`export` 在读取前会尝试静默增量刷新 `decrypted/`。如果当前无法访问Telegram数据库或没有可用密钥，它们会继续读取已有的解密缓存。`messages` 会先确认 contact 和 numbered message 数据库都已解密；如果发现缺 key 或解密失败，会自动重新提取 keys、刷新解密缓存并重试一次，仍不完整时会报错退出，避免输出不完整的聊天记录。读不到时先跑 `tg doctor` 或 `tg doctor "联系人或群名"` 看具体状态。

## 常用命令

提取密钥：

```bash
sudo tg keys
```

解密数据库：

```bash
tg refresh
tg refresh --keys
tg decrypt
tg decrypt --full
tg decrypt --since 1h --verbose
tg decrypt --db-dir "/path/to/your/db_storage"
```

常见 `db_storage` 位置在Telegram容器目录下，例如 `Documents/xtelegram_files/.../db_storage` 或 `Library/Application Support/telegram-container/.../db_storage`。

查看会话：

```bash
tg sessions
tg sessions --top 50
```

读取消息：

```bash
tg "张三"
tg "张三" --limit 100
tg messages "张三"
tg messages "张三" --limit 100
tg messages "张三" --since today
tg messages "张三" --since yesterday
tg messages "张三" --search "项目"
tg messages "张三" --head --limit 20
tg messages "张三" --tail --limit 20
tg messages "张三" --offset 100 --limit 50
tg messages "张三" --time-bucket full
tg messages "张三" --time-bucket 1h
```

`messages` 默认按 `1m` 分组显示时间，同一分钟内不重复打印时间。同一个时间分组里，如果连续消息来自同一发送者，后续消息只打印一个前导空格，不重复打印发送者名。`--time-bucket` 支持 `1m`/`1min`、`1h`、`1d`、`1mo`、`1y`、`full`、`none`。

搜索：

```bash
tg search "关键词"
tg search "关键词" --limit 50
tg search "关键词" --since today
```

诊断：

```bash
tg doctor
tg doctor "张三"
```

导出聊天：

```bash
tg export "张三" --format txt
tg export "张三" --format csv --output exported/zhangsan
tg export "张三" --format json --output exported/zhangsan
```

导出聊天并尝试导出本地缓存媒体：

```bash
tg export "张三" --format json --output exported/zhangsan --media-dir exported/zhangsan/media
```

导出图片：

```bash
tg image "张三"
tg image "张三" --list --limit 20
tg image "张三" --index 3
tg image "张三" --all --limit 10 --output exported/images
```

`image --list` 会先列出最近图片是否还在本地缓存里：

```text
Index Time                Status   Source
----------------------------------------------------------------------------------------------------
1     2026-04-28 09:38:44 cached   .../msg/attach/...
2     2026-04-27 22:10:01 missing  cdnthumburl...
```

## 时间过滤

`--since` 支持：

- 日期：`2026-04-28`
- 日期时间：`2026-04-28 09:30:00`
- 相对时间：`5min`、`1h`、`2d`、`1w`
- 命名时间：`today`、`yesterday`

日期、日期时间和输出中的消息时间都使用当前系统时区。

## 输出文件

默认会在当前目录生成这些文件或目录：

- `all_keys.json`：数据库密钥。
- `decrypted/`：解密后的 SQLite 数据库。
- `exported/`：导出的聊天和媒体。

这些文件都应视为敏感数据。使用后如需清理：

```bash
# 确认当前目录后再清理
rm -rf all_keys.json decrypted exported
```

## 常见问题

### `Telegram is not running`

先打开并登录 macOS Telegram，再运行 `sudo tg keys`。

### `task_for_pid failed`

先确认用了 `sudo tg keys`。如果仍失败，退出Telegram后重新签名：

```bash
sudo codesign --force --deep --sign - /Applications/Telegram.app
```

然后重新打开Telegram，再运行 `sudo tg keys`。

### `No sessions found`

通常是还没有成功解密数据库。按顺序检查：

```bash
ls all_keys.json
tg decrypt --verbose
tg sessions --top 30
```

如果Telegram数据目录不是默认位置，给 `decrypt` 加 `--db-dir`。

### 找到了错误联系人

同名联系人或群较多时，先用 `tg sessions --top 100` 找到准确的 `tgid_...` 或 `...@chatroom`，再用这个 ID 读取：

```bash
tg messages "tgid_abcd1234" --limit 50
```

### 图片或视频导不出来

媒体导出依赖Telegram本地缓存。可以先在Telegram里打开对应图片或视频，让Telegram把文件下载到本机，再重新运行 `tg image` 或 `tg export --media-dir ...`。

### `tggf` 表情转换失败

安装 `ffmpeg`：

```bash
brew install ffmpeg
```

如果 `ffmpeg` 不在 `PATH`，可以指定：

```bash
TG_FFMPEG=/path/to/ffmpeg tg export "张三" --media-dir exported/media
```

## 日志

聊天记录、会话表格、搜索结果等命令结果写到 stdout。运行状态、警告和错误写到 stderr。

默认日志等级是 `info`。可以用 `TG_LOG` 或 `RUST_LOG` 调整：

```bash
TG_LOG=warn tg decrypt
TG_LOG=debug tg messages "张三"
```

## 开发

```bash
make build
cargo test
cargo build
```

项目入口是 `src/main.rs`。主要模块：

- `src/scanner.rs`：运行内嵌的 macOS 密钥扫描器。
- `src/decrypt.rs`：数据库解密。
- `src/db.rs`：会话、联系人和消息查询。
- `src/message.rs`：消息类型解析。
- `src/media*.rs`：媒体元信息、缓存查找和解密。
- `src/export.rs`：导出。
- `vendor/find_all_keys_macos.c`：链接进 `tg` 的 macOS Telegram 进程扫描器。

面向 AI/自动化助手的能力说明见 [SKILL.md](SKILL.md)。

## License

MIT
