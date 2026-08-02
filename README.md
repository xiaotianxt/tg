# tg

tg 是一个 macOS/Linux 本地 Telegram 聊天记录读取 CLI。它直接只读打开 Telegram 的 SQLCipher 数据库，复用原生 SQLite 索引、FTS 和当前 WAL；不复制消息库，也不需要 `refresh`。

主要用途：

- 查看会话、未读消息和单个会话历史。
- 全局快速搜索或结构化完整检索。
- 把完整会话导出成 `txt`、`csv`、`json`，并归档本地缓存媒体。

聊天数据默认只留在本机。`~/.tg/all_keys.json`、`~/.tg/key_material.bin` 和导出目录都包含敏感数据。

## 安装

```bash
brew install xiaotianxt/tap/tg
tg --version
```

Homebrew 支持 macOS Apple Silicon、Linux x86_64 和 Linux arm64。

源码安装需要 Rust、C 编译器、make、Perl 和 pkg-config；SQLCipher 与 OpenSSL 会静态编入二进制，不需要系统 SQLite/SQLCipher/OpenSSL 动态库。

```bash
git clone https://github.com/xiaotianxt/tg.git
cd tg
make install-local
```

安装 shell completion：

```bash
tg completions fish > ~/.config/fish/completions/tg.fish
tg completions zsh > ~/.zsh/completions/_tg
tg completions bash > ~/.local/share/bash-completion/completions/tg
```

## 第一次使用

先打开并登录本机 Telegram 桌面版，然后提取数据库密钥。

macOS 新版客户端需要在登录时捕获一次账号级 key material。`codesign` 只影响重签后启动的新进程：

```bash
sudo DevToolsSecurity -enable
osascript -e 'quit app "Telegram"'
while pgrep -x Telegram >/dev/null; do sleep 1; done
sudo codesign --force --deep --sign - /Applications/Telegram.app
open -a Telegram
sudo tg keys --method login --timeout 180
```

命令等待期间，在 Telegram 中退出账号并重新登录。需要 Apple Command Line Tools；若缺少 `lldb`，运行 `xcode-select --install`。若系统提示 Developer Tools 权限，请允许当前终端应用。

Linux 使用同一个接口，内部通过 GDB 捕获：

```bash
sudo apt install gdb
sudo tg keys --method login --timeout 180
```

以后直接运行下面的快速路径。它会复用权限为 `0600` 的 `~/.tg/key_material.bin`，验证已有 key，只为新 salt 派生密钥：

```bash
sudo tg keys
tg doctor
tg sessions --top 30
```

密钥保存在 `~/.tg/all_keys.json`。所有读取命令随后直接打开加密源库，不创建解密副本。

## 常用命令

查找会话和未读：

```bash
tg sessions
tg sessions "张三"
tg sessions --top 50
tg unread
tg unread --top 50
```

读取消息：

```bash
tg "张三"
tg messages "张三" --limit 100
tg messages "张三" --since today
tg messages "张三" --all-time
tg messages "张三" --search "项目"
tg messages "张三" --head --limit 20
tg messages "产品讨论群" --anonymous
```

`messages` 默认显示最新 50 条。`--since` 支持日期、日期时间、`5min`、`1h`、`2d`、`1w`、`1y`、`today` 和 `yesterday`。群聊或全局结果建议加 `--anonymous`，避免输出个人备注名。

快速全局搜索：

```bash
tg search "关键词" --limit 50 --anonymous
tg search "关键词" --since today --anonymous
tg search "关键词" --all-time --anonymous
```

`search` 直接复用上游 FTS，速度最快，但只覆盖上游已经收录的正文。群公告、部分卡片深层内容等可能未进入 FTS。

完整结构化检索：

```bash
tg query --contains "项目" --limit 50 --anonymous
tg query --session "产品讨论群" --contains "项目" --fields time,sender,body --limit 20 --anonymous
tg query --contains "项目" --contains "上线" --match-mode all --since today --anonymous
tg query --contains "项目" --not "取消" --format json --fields timestamp,session,body --anonymous
tg query --has voice --session "张三" --limit 20
tg query --raw-contains "<appmsg" --fields time,session,raw_body --limit 20
tg query --contains "关键词" --all-time --anonymous
```

`query` 扫描本地消息分片中由数据库索引约束出的候选行，再匹配解码后的正文，因此比 FTS 覆盖更完整。它不是原始 SQL 接口。`--has` 支持 `voice,image,sticker,file,video`；`--fields` 支持 `time,session,sender,type,body,raw_body,timestamp`。

`search` 和 `query` 默认查询最近 365 天。全量历史用 `--all-time`；为避免误扫全库，`query --all-time` 仍要求至少提供 `--contains`、`--raw-contains`、`--has` 或 `--since` 之一。

诊断：

```bash
tg doctor
tg doctor "张三"
```

诊断会检查源目录、密钥覆盖、SQLCipher、当前 WAL、会话库和 FTS。

## 完整导出

```bash
tg export "张三" --output exported/zhangsan
```

每次导出都会写出：

```text
exported/zhangsan/chat.txt
exported/zhangsan/chat.csv
exported/zhangsan/chat.json
exported/zhangsan/media/
```

本地可用的图片、视频、表情、文件和语音会写入 `media/`。Telegram 没有缓存或缓存已清理时，只保留消息摘要；tg 不从远端 URL 下载。语音优先写成 WAV，解码器不可用或单条失败时保留 `.voice`。`tggf` 表情转换需要 `ffmpeg`。

## 自定义路径与并行度

读取命令共同支持：

```bash
tg sessions --db-dir /path/to/db_storage --keys /path/to/all_keys.json
tg query --contains "关键词" --jobs 4
```

密钥提取也支持自定义路径：

```bash
sudo tg keys --db-dir /path/to/db_storage --output /path/to/all_keys.json
```

`--jobs 0` 表示自动选择并行度。

## 数据与磁盘行为

tg 自己只长期保存：

- `~/.tg/all_keys.json`：每个源数据库的密钥。
- `~/.tg/key_material.bin`：账号级 key material，权限 `0600`。
- 用户显式指定的导出目录。

`sessions`、`unread`、`doctor`、`messages`、`search` 和 `query` 不创建消息数据库、索引或页缓存。读取期间的 SQLite 临时数据强制放在内存中。`export` 只写用户指定的归档目录。

旧版本留下的 `~/.tg/decrypted/` 不再读取，也不会被 tg 自动删除。确认新版工作正常后，可自行清理：

```bash
rm -rf ~/.tg/decrypted
```

## 支持与限制

| 类别 | 当前支持 |
| --- | --- |
| 系统 | macOS arm64；glibc 2.35+ Linux x86_64 / arm64 |
| 数据源 | 本机 Telegram 桌面版 `db_storage`，包括当前 WAL 中已提交数据 |
| 搜索 | 上游 FTS 快速搜索；消息分片完整结构化检索 |
| 导出 | 完整会话 `txt`、`csv`、`json` 与本地缓存媒体 |
| 安全 | SQLCipher 只读连接、`query_only`、每个数据库独立的一致性快照 |

不支持 Windows、移动端或网页版数据；不恢复本地数据库里已经不存在的消息；不保证导出 Telegram 未缓存的媒体；不提供 OCR、语义搜索或拼音搜索。

## 常见问题

### `Telegram is not running`

打开并登录 Telegram，再运行 `sudo tg keys`。

### `task_for_pid failed`

完全退出 Telegram，按“第一次使用”中的顺序重签、重启，再运行 `sudo tg keys --method login --timeout 180`。如果仍提示 `Not allowed to attach to process`，在 System Settings -> Privacy & Security -> Developer Tools 中允许当前终端，重启终端后重试。

### 没有会话或消息

```bash
ls ~/.tg/all_keys.json
sudo tg keys
tg doctor
tg sessions --top 30
```

非默认安装位置请给命令传 `--db-dir`。

### 找到了错误联系人

先用 `tg sessions --top 100` 找到准确的 `tgid_...` 或 `...@chatroom`，再用 ID 查询。

### 图片、视频或文件导不出来

先在 Telegram 中打开或下载该媒体，再重新运行 `tg export`。

### `tggf` 转换失败

```bash
brew install ffmpeg
TG_FFMPEG=/path/to/ffmpeg tg export "张三" --output exported/zhangsan
```

## 日志

命令结果写 stdout，状态和错误写 stderr：

```bash
TG_LOG=warn tg sessions
TG_LOG=debug tg messages "张三"
```

## 开发

```bash
make check
make build
```

核心模块：

- `src/source_store.rs`：发现源数据库、应用 SQLCipher key、建立只读快照。
- `src/database_keys.rs`：key 文件、salt、第一页 HMAC 验证。
- `src/db.rs` / `src/query.rs`：会话、消息、FTS 和结构化查询。
- `src/scanner.rs`：缓存派生或平台登录捕获。
- `src/export.rs` / `src/media*.rs`：聊天和媒体归档。

面向 AI/自动化助手的能力说明见 [SKILL.md](SKILL.md)。

## License

MIT。发布包内含 [LICENSE](LICENSE) 与 [THIRD_PARTY_LICENSES](THIRD_PARTY_LICENSES)。
