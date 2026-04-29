---
name: tgreader
description: Use when the user needs to read, search, inspect, back up, export, or troubleshoot access to local macOS Telegram chat history. This skill uses tgreader to sign Telegram if needed, extract local database keys, decrypt local databases, list conversations, read or search messages, export chats and cached media, and maintain the tgreader codebase when requested.
---

# tgreader

## When To Use

Use this skill for user goals like:

- "帮我读一下和某个人的Telegram聊天记录"
- "查Telegram里有没有某个关键词"
- "导出这个Telegram群的聊天记录"
- "把Telegram聊天备份成 json/csv/txt"
- "为什么本机Telegram聊天记录读不出来"
- "修一下 tgreader 的读取、解密、导出逻辑"

Do not wait for the user to name tgreader. tgreader is the implementation; the user goal is local macOS Telegram history access.

## Operating Principles

tgreader touches private chat data. Keep work local by default, avoid printing more message content than the user asked for, and treat these files as sensitive: `all_keys.json`, `decrypted/`, `exported/`, media exports.

Optimize for the shortest successful user path. Do not turn normal use into a reverse-engineering walkthrough unless the user is debugging internals.

## User Workflow

For a fresh setup, use this order:

```bash
sudo codesign --force --deep --sign - /Applications/Telegram.app
```

If Telegram is installed somewhere else, use that `.app` path, for example `/Applications/Telegram.app`.

Then have the user open and log in to macOS Telegram:

```bash
sudo tgreader keys
tgreader refresh
tgreader sessions --top 50
tgreader "联系人或群名" --limit 50
```

After the first successful decrypt, `sessions`, `messages`, `search`, and `export` will try a quiet incremental refresh before reading `decrypted/`. If live access fails, they can still read the existing decrypted cache.

## Commands By Intent

Install:

```bash
brew install xiaotianxt/tgreader/tgreader
tgreader --version
```

From source inside the repo:

```bash
make install-local
```

Find a chat:

```bash
tgreader sessions --top 50
```

Read a chat:

```bash
tgreader "张三"
tgreader "张三" --limit 100
tgreader messages "张三"
tgreader messages "张三" --limit 100
tgreader messages "张三" --since today
tgreader messages "张三" --search "关键词"
tgreader messages "张三" --head --limit 20
tgreader messages "张三" --tail --limit 20
```

Search globally:

```bash
tgreader search "关键词" --limit 50
tgreader search "关键词" --since today
```

Diagnose or refresh:

```bash
tgreader doctor
tgreader doctor "张三"
tgreader refresh
tgreader refresh --keys
```

Export:

```bash
tgreader export "张三" --format txt
tgreader export "张三" --format csv --output exported/zhangsan
tgreader export "张三" --format json --output exported/zhangsan
tgreader export "张三" --format json --output exported/zhangsan --media-dir exported/zhangsan/media
```

Export cached images:

```bash
tgreader image "张三" --list --limit 20
tgreader image "张三" --index 3
tgreader image "张三" --all --limit 10 --output exported/images
```

Time filters support dates, datetimes, and relative values:

```bash
--since 2026-04-28
--since "2026-04-28 09:30:00"
--since 5min
--since 1h
--since today
--since yesterday
```

## Troubleshooting

- `Telegram is not running`: open and log in to macOS Telegram, then run `sudo tgreader keys`.
- `Scanner binary not found`: from source, run `make build` or `make install-local`.
- `task_for_pid failed`: confirm `sudo tgreader keys`, quit Telegram, run `sudo codesign --force --deep --sign - /Applications/Telegram.app`, reopen Telegram, retry.
- Unknown read failure: run `tgreader doctor` or `tgreader doctor "联系人或群名"`.
- `No sessions found`: check `all_keys.json` exists, run `tgreader decrypt --verbose`, then `tgreader sessions --top 50`.
- Cannot auto-detect DB path: pass `tgreader decrypt --db-dir "/path/to/db_storage"`.
- Wrong chat matched: use `tgreader sessions --top 100` and rerun with the exact `tgid_...` or `...@chatroom`.
- Missing media: Telegram may not have cached the file. Open/download it in Telegram, then retry `tgreader image` or `tgreader export --media-dir ...`.
- `tggf` sticker conversion fails: install `ffmpeg` or set `TGREADER_FFMPEG=/path/to/ffmpeg`.

## Codebase Map

- `src/main.rs`: CLI commands and top-level flow.
- `src/cache.rs`: quiet decrypt refresh and key-refresh retry policy.
- `src/doctor.rs`: read-only setup and chat diagnostics.
- `src/scanner.rs`: wrapper around `scanner_macos`.
- `vendor/find_all_keys_macos.c`: macOS Telegram process memory scanner.
- `src/decrypt.rs`: SQLCipher/WCDB database decrypt.
- `src/db.rs`: contacts, sessions, message reads, search, username resolution.
- `src/message.rs`: message type decoding and display text.
- `src/media*.rs`: media metadata, cache lookup, `.dat` decrypt, media keys.
- `src/export.rs`: txt/csv/json/media/image export.

## Maintenance Rules

- Keep CLI behavior explicit and predictable. Results go to stdout; progress, warnings, and errors go to stderr logs.
- Preserve local privacy. Do not upload, paste, or move chat data unless the user explicitly asks.
- Be conservative with media claims: cached image/video/sticker export is best-effort.
- When touching key extraction, preserve macOS process scanning compatibility.
- When touching decrypt logic, keep SQLite verification and avoid writing corrupt outputs.
- When touching message reads, continue handling TEXT, BLOB, and compressed `WCDB_CT_message_content`.
- When touching group chats, preserve sender tgid parsing and contact display name resolution.
- Prefer small focused changes over new abstraction layers.

## Validation

For docs-only changes, run:

```bash
git diff --check
```

For Rust or C behavior changes, run the smallest relevant check first, then broader checks if the change affects shared behavior:

```bash
cargo test
cargo build
make build
```
