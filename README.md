# tg

tg 是一个 macOS 本地 Telegram 聊天记录读取 CLI。它把本机 Telegram 桌面版的聊天数据整理成几个直接的命令：读一个会话、全局搜索、结构化检索、导出聊天和导出本地缓存媒体。

底层的密钥提取、数据库解密和索引维护仍然可控，但日常使用不需要反复关心这些步骤。`tg refresh` 负责刷新本地解密缓存和近期消息索引；`messages`、`search`、`query`、`export` 会在读取前尽量静默刷新，失败时继续使用已有缓存并给出诊断线索。

适合这些场景：

- 备份自己的 Telegram 聊天记录，不依赖手机备份。
- 快速找某个人、某个群、某个关键词的历史消息。
- 用包含词、排除词、时间范围和输出字段做更精确的本地检索。
- 把聊天导出成 `txt`、`csv`、`json`，用于归档、整理或本地分析。
- 从本地缓存里导出图片、视频、表情等媒体文件。
- 从本地媒体数据库里导出语音消息为 `.silk` 文件。
- 给本地 AI/自动化助手一个稳定的聊天记录读取工具和 skill 描述。

默认数据只留在本机。`~/.tg/all_keys.json`、`~/.tg/decrypted/`、`exported/` 都是敏感文件，请当作聊天原文保存和处理。

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
Showing latest 4 messages

[2026-04-28 09:37]
我: 今天先把导出格式定下来
[2026-04-28 09:38]
李四: [img:abc123]
 这张是最新版
[2026-04-28 09:41]
王五: > 李四: [img:abc123]
        这张可以放到 README 里

--- End of messages ---
```

导出聊天：

```bash
$ tg export "张三" --format json --output exported/zhangsan
Exported 2381 messages for 张三 (tgid_abcd1234)
Exported to:
  [json] exported/zhangsan/chat.json
```

导出最近一张本地缓存图片：

```bash
$ tg image "产品讨论群"
exported/images/123456789_chatroom_Image_3_0001.jpg
```

导出最近一条本地语音：

```bash
$ tg voice "产品讨论群"
exported/voices/123456789_chatroom_Voice_34_0001_123.silk
```

## 日常工作流

第一次初始化后，最常用的是这些命令：

```bash
tg "张三" --limit 50
tg "产品讨论群" --since today
tg search "关键词"
tg query --session "产品讨论群" --contains "项目" --not "取消" --fields time,sender,body
tg export "张三" --format json --output exported/zhangsan
tg image "产品讨论群" --list
tg voice "产品讨论群" --list
```

`search`、`query`、`export` 默认只处理最近 365 天，这是当前版本为速度和常见需求做的默认路径。需要全量历史时加 `--all-time`；需要更窄范围时用 `--since today`、`--since 30d` 或具体日期。

如果结果看起来不完整，先跑：

```bash
tg doctor
tg doctor "联系人或群名"
tg refresh
```

`refresh` 会刷新解密缓存并维护 `~/.tg/decrypted/.tg_index.db`。如果发现消息或联系人数据库缺 key，它会自动刷新 keys 并重试一次；也可以用 `tg refresh --keys` 强制先重新提取 keys。

## 支持与限制

| 类别 | 当前支持 |
| --- | --- |
| 系统 | macOS，本机 Telegram 桌面版的本地数据 |
| Telegram 版本 | 主要面向 macOS Telegram 4.x；Telegram 升级后如果数据库结构变化，可能需要跟进适配 |
| Telegram 数据 | 本机 `db_storage` 中的聊天数据库；无需手机备份 |
| 会话匹配 | 联系人显示名、备注、别名、`tgid_...`、群 ID |
| 消息读取 | 文本、群聊发送者、系统提示、撤回提示、引用、链接、小程序、聊天记录卡片展开、位置、文件卡片、图片/视频/语音/表情的可读摘要 |
| 搜索 | 全局关键词搜索，单个会话内关键词搜索，结构化检索；支持包含词、排除词、时间范围、字段选择和 JSON 行输出 |
| 导出 | `txt`、`csv`、`json` |
| 媒体 | 尝试导出本地缓存图片、视频、表情；`.dat` 图片/视频会尝试解密；语音可通过 `voice` 导出为 `.silk` |
| 增量更新 | 默认只解密变化过的数据库 |

暂不支持或不保证：

- 不支持 Windows、iOS、Android、网页版 Telegram。
- 不恢复 Telegram 本地数据库里已经没有的消息。
- 不保证导出所有媒体。Telegram 没有缓存、缓存被清理、文件未下载时，只能显示消息摘要。
- `export --media-dir` 会尝试导出图片、视频、表情，但不导出语音音频和普通文件附件本体；语音请用 `tg voice`。
- 表情导出可能根据消息里的 URL 用 `curl` 下载；普通读取、解密、搜索不会上传聊天数据。
- `tggf` 表情转换需要本机可用的 `ffmpeg`。
- 不做 OCR、语义搜索、拼音搜索。
- 多账号或 Telegram 路径异常时，可能需要手动指定 `--db-dir`。

## 安装

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

安装 shell completion：

```bash
tg completions fish > ~/.config/fish/completions/tg.fish
tg completions zsh > ~/.zsh/completions/_tg
tg completions bash > ~/.local/share/bash-completion/completions/tg
```

补全脚本支持命令、常用 flags、会话名和 `query --session`。动态会话候选只读取本地
`~/.tg/decrypted/.tg_index.db` 或 contact cache，不会触发解密、刷新或读取在线服务。
release formula 会为 Homebrew 用户自动安装这些 completions。

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

   成功后密钥会保存到 `~/.tg/all_keys.json`。

3. 刷新本地缓存和近期消息索引：

   ```bash
   tg refresh
   ```

   成功后解密缓存会保存到 `~/.tg/decrypted/`，近期消息索引会保存到 `~/.tg/decrypted/.tg_index.db`。

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
tg query --session "联系人或群名" --contains "关键词" --fields time,sender,body --limit 20
tg export "联系人或群名" --format json
```

`search`、`query`、`export` 默认只看最近 365 天，这是大多数查询和导出的热路径。需要全量历史时加 `--all-time`；需要更窄窗口时加 `--since today`、`--since 30d` 或具体日期。

`sessions`、`search`、`query`、`schema`、`export` 在读取前会尝试静默增量刷新 `~/.tg/decrypted/`。如果当前无法访问 Telegram 数据库或没有可用密钥，它们会继续读取已有的解密缓存。`messages` 会先确认 contact 和 numbered message 数据库都已解密；如果发现缺 key 或解密失败，会自动重新提取 keys、刷新解密缓存并重试一次，仍不完整时会报错退出，避免输出不完整的聊天记录。读不到时先跑 `tg doctor` 或 `tg doctor "联系人或群名"` 看具体状态。

如果 `sudo tg keys` 遇到 `task_for_pid failed` 或权限问题，退出 Telegram 后重新签名一次：

```bash
sudo codesign --force --deep --sign - /Applications/Telegram.app
```

如果你的 Telegram 路径不是 `/Applications/Telegram.app`，把路径改成实际的 App 路径。重新打开 Telegram 后再运行 `sudo tg keys` 或 `tg refresh --keys`。

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

常见 `db_storage` 位置在 Telegram 容器目录下，例如 `Documents/xtelegram_files/.../db_storage` 或 `Library/Application Support/telegram-container/.../db_storage`。

查看会话：

```bash
tg sessions
tg sessions "张三"
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
tg messages "张三" --all-time
tg messages "张三" --search "项目"
tg messages "张三" --head --limit 20
tg messages "张三" --tail --limit 20
tg messages "张三" --offset 100 --limit 50
tg messages "张三" --time-bucket full
tg messages "张三" --time-bucket 1h
tg messages "产品讨论群" --anonymous
```

`messages` 默认显示最新 50 条；加 `--since` 会显示指定时间之后的消息，加 `--all-time` 会显示整个会话历史。默认按 `1m` 分组显示时间，同一分钟内不重复打印时间。同一个时间分组里，如果连续消息来自同一发送者，后续消息只打印一个前导空格，不重复打印发送者名。`--time-bucket` 支持 `1m`/`1min`、`1h`、`1d`、`1mo`、`1y`、`full`、`none`。

默认显示名使用你的个人备注，引用消息里的发送者也保持一致。加 `--anonymous` 后，群消息优先使用群内展示名，找不到时回退到联系人默认名，不使用你的个人备注。

搜索：

```bash
tg search "关键词"
tg search "关键词" --limit 50
tg search "关键词" --since today
tg search "关键词" --all-time
```

结构化检索：

```bash
tg query --contains "项目" --limit 50
tg query --session "张三" --contains "项目" --fields time,sender,body --limit 20
tg query --contains "项目" --contains "上线" --match-mode all --since today
tg query --contains "项目" --not "已取消" --format json --fields timestamp,session,body
tg query --contains "项目" --all-time
tg schema --db message_0
```

`query` 适合在本机做精确检索：限定某个会话、同时要求多个关键词、排除某些词、按时间收窄结果，或者只输出后续脚本需要的字段。`--fields` 支持 `time,session,sender,type,body,timestamp`，`--format json` 会按 JSON lines 输出，便于继续处理。

`query` 不接受原始 SQL。用户只传会话、关键词、排除词、时间、排序和输出字段，tg 内部生成固定的参数化数据库查询，并以只读方式打开消息数据库。默认查询最近 365 天；加 `--all-time` 后，为了避免误扫全库，必须至少传 `--contains` 或 `--since` 之一。空关键词会被拒绝；单次 `--limit + --offset` 最多 10000。table 输出会转义终端控制字符，避免聊天正文影响终端显示。`schema` 展示的是公开查询字段和过滤器，不输出原始表名、列名或建表语句。

诊断：

```bash
tg doctor
tg doctor "张三"
```

导出聊天：

```bash
tg export "张三" --format txt
tg export "张三" --format csv --since 30d --output exported/zhangsan
tg export "张三" --format json --limit 1000 --output exported/zhangsan
tg export "张三" --format json --all-time --output exported/zhangsan
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
tg image "张三" --id abc123
tg image "张三" --all --limit 10 --output exported/images
```

`messages` 默认把图片显示成紧凑标签：有可用标识时是 `[img:<id>]`，没有时是 `[img]`。这里的 `id` 是本地媒体定位标识，不保证是哈希；它可能来自 protobuf 文件名、XML `aeskey` 或 `cdnthumburl`。可以把这个值传给 `image --id`，直接从本地缓存导出对应图片。

`image --list` 会先列出最近图片是否还在本地缓存里：

```text
Index Time                Status   Source
----------------------------------------------------------------------------------------------------
1     2026-04-28 09:38:44 cached   .../msg/attach/...
2     2026-04-27 22:10:01 missing  cdnthumburl...
```

导出语音：

```bash
tg voice "张三"
tg voice "张三" --list --limit 20
tg voice "张三" --id 123
tg voice "张三" --index 3
tg voice "张三" --all --limit 10 --output exported/voices
tg voice "张三" --id 123 --format wav
```

`voice --list` 会显示可复用的 `ID`，可以把它传给 `voice --id` 精确导出某条语音。`voice` 从本地媒体数据库读取语音 BLOB。当前 macOS 数据里常见格式是在 `#!SILK_V3` 前多一个前置字节，导出时会自动去掉这个字节并保存为 `.silk`。

`.silk` 不是系统播放器通用格式。需要直接导出可播放 WAV 时，先安装一个 SILK 解码器：

```bash
brew install xiaotianxt/tap/rust-silk
tg voice "张三" --id 123 --format wav
```

源码安装时也可以用 `cargo install rust-silk`。还可以用 `--decoder /path/to/rust-silk` 或 `TG_SILK_DECODER=/path/to/rust-silk` 指定解码器。当前支持 `rust-silk`、`silk-decoder` 和常见 `silk_v3_decoder`/`decoder` 命令。

## 时间过滤

`--since` 支持：

- 日期：`2026-04-28`
- 日期时间：`2026-04-28 09:30:00`
- 相对时间：`5min`、`1h`、`2d`、`1w`、`1y`
- 命名时间：`today`、`yesterday`

日期、日期时间和输出中的消息时间都使用当前系统时区。

## 输出文件

默认会生成这些文件或目录：

- `~/.tg/all_keys.json`：数据库密钥。
- `~/.tg/decrypted/`：解密后的 SQLite 数据库。
- `~/.tg/decrypted/.tg_index.db`：`refresh` 维护的本地热索引，默认查询和导出会优先使用；可删除后重新 `refresh` 构建。
- `exported/`：导出的聊天和媒体，默认位于当前目录。

这些文件都应视为敏感数据。使用后如需清理：

```bash
# 确认后再清理
rm -rf ~/.tg exported
```

## 常见问题

### `Telegram is not running`

先打开并登录 macOS Telegram，再运行 `sudo tg keys`。

### `task_for_pid failed`

先确认用了 `sudo tg keys`。如果仍失败，退出 Telegram 后重新签名：

```bash
sudo codesign --force --deep --sign - /Applications/Telegram.app
```

然后重新打开 Telegram，再运行 `sudo tg keys`。

### `No sessions found`

通常是还没有成功解密数据库。按顺序检查：

```bash
ls ~/.tg/all_keys.json
tg refresh --keys
tg sessions --top 30
```

如果 Telegram 数据目录不是默认位置，给 `decrypt` 加 `--db-dir`。

### 找到了错误联系人

同名联系人或群较多时，先用 `tg sessions --top 100` 找到准确的 `tgid_...` 或 `...@chatroom`，再用这个 ID 读取：

```bash
tg messages "tgid_abcd1234" --limit 50
```

### 图片或视频导不出来

媒体导出依赖 Telegram 本地缓存。可以先在 Telegram 里打开对应图片或视频，让 Telegram 把文件下载到本机，再重新运行 `tg image` 或 `tg export --media-dir ...`。

### 语音导出后不能直接播放

`tg voice` 默认导出 `.silk`，这是语音消息的原始语音编码，不是 macOS 系统播放器通常能直接打开的格式。安装 `xiaotianxt/tap/rust-silk` 后可以直接用 `tg voice "张三" --id <ID> --format wav` 导出 WAV；需要 `mp3` 时再让 `ffmpeg` 处理这个 WAV。

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
make check
make build
```

`make check` 会依次运行 rustfmt、clippy 和测试：

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
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

也可以把随版本打包的 skill 安装到本机 Codex skill 目录：

```bash
tg skill install
```

## License

MIT
