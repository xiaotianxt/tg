---
name: tg
description: Use when the user wants to read, search, inspect, back up, export, or troubleshoot local macOS Telegram chat history with tg. Keep the user's chat data local and guide them through the shortest working tg command flow.
when_to_use: Trigger for requests mentioning Telegram聊天记录, Telegram聊天, Telegram群, Telegram里, local chat backup, message search, message export, or troubleshooting why local chat history cannot be read.
---

# tg

Canonical source: https://github.com/xiaotianxt/skills/tree/main/skills/tg

## When To Use

Use this skill for user goals like:

- "帮我读一下和某个人的Telegram聊天记录"
- "查Telegram里有没有某个关键词"
- "导出这个Telegram群的聊天记录"
- "把Telegram聊天备份成 json/csv/txt"
- "为什么本机Telegram聊天记录读不出来"

Do not wait for the user to name tg. tg is the tool; the user goal is local macOS Telegram history access.

## Privacy

Chat data is private. Keep work local by default, avoid printing more message content than the user asked for, and treat exports as sensitive.
For summary requests, choose display names by the target. If the user clearly names a 1-on-1 person, omit `--anonymous` so the summary uses the user's intended name for that person. If the target is a group chat, a room, a global search, or the target type is unclear, use `--anonymous` whenever the command supports it to avoid exposing personal contact remarks in assistant-visible output and exported sender names.
`~/.tg/all_keys.json` and `~/.tg/decrypted/` are sensitive local state. `~/.tg/decrypted/.tg_index.db` is a local derived hot index maintained by `tg refresh`; treat it as sensitive and safe to delete if it needs to be rebuilt.

## First Setup

For a fresh setup, ask the user to open and log in to macOS Telegram first.

If tg needs permission to access the desktop app, have the user quit Telegram and run:

```bash
sudo codesign --force --deep --sign - /Applications/Telegram.app
```

If Telegram is installed somewhere else, use that `.app` path instead.

Then run:

```bash
sudo tg keys
tg refresh
tg sessions --top 50
tg "联系人" --limit 50
tg "群名" --limit 50 --anonymous
```

## Common Commands

Install:

```bash
brew install xiaotianxt/tap/tg
tg --version
```

Find a chat:

```bash
tg sessions "张三"
tg sessions --top 50
```

Read a chat:

```bash
tg "张三"
tg "张三" --limit 100
tg messages "张三" --limit 100
tg messages "张三" --since today
tg messages "张三" --all-time
tg messages "张三" --search "关键词"
tg messages "张三" --head --limit 20
tg messages "张三" --tail --limit 20
tg messages "产品讨论群" --limit 100 --anonymous
```

Search globally:

```bash
tg search "关键词" --limit 50 --anonymous
tg search "关键词" --since today --anonymous
tg search "关键词" --all-time --anonymous
```

Use structured lookup when the user wants precise filters, multiple keywords,
excluded words, selected output fields, or JSON lines for a local analysis step.
This is not a raw SQL interface; pass user intent as filters:

```bash
tg query --contains "关键词" --limit 50 --anonymous
tg query --session "张三" --contains "关键词" --fields time,sender,body --limit 20
tg query --session "产品讨论群" --contains "关键词" --fields time,sender,body --limit 20 --anonymous
tg query --contains "项目" --contains "上线" --match-mode all --since today --anonymous
tg query --contains "项目" --not "已取消" --format json --fields timestamp,session,body --anonymous
tg query --contains "项目" --all-time --anonymous
tg schema --db message_0
```

Use `tg schema` when the user asks what `query` can return or filter on. It
shows the public query contract, not raw database table or column names.

Query safety rules:

- `search`, `query`, and `export` default to the recent 365-day window; use `--all-time` only when the user asks for full history.
- With `--all-time`, `query` requires at least `--contains` or `--since`.
- Empty `--contains` / `--not` values are rejected.
- Use `--session`, `--since`, and a reasonable `--limit` when results could be large.
- Table output escapes terminal control characters; use `--format json` for machine parsing.

Refresh or diagnose:

```bash
tg refresh
tg refresh --keys
tg doctor
tg doctor "张三"
```

Export:

```bash
tg export "张三" --format txt
tg export "张三" --format csv --since 30d --output exported/zhangsan
tg export "张三" --format json --limit 1000 --output exported/zhangsan
tg export "张三" --format json --all-time --output exported/zhangsan
tg export "产品讨论群" --format json --output exported/group --media-dir exported/group/media --anonymous
```

Export cached images:

```bash
tg image "张三" --list --limit 20
tg image "张三" --index 3
tg image "张三" --all --limit 10 --output exported/images
```

Export cached voice messages:

```bash
tg voice "张三" --list --limit 20
tg voice "张三" --id 123
tg voice "张三" --index 3
tg voice "张三" --all --limit 10 --output exported/voices
tg voice "张三" --id 123 --format wav
```

Time filters support dates, datetimes, and relative values:

```bash
--since 2026-04-28
--since "2026-04-28 09:30:00"
--since 5min
--since 1h
--since 1y
--since today
--since yesterday
```

Date, datetime, and displayed message times use the current system time zone.

## Troubleshooting

- `Telegram is not running`: open and log in to macOS Telegram, then run `sudo tg keys`.
- `task_for_pid failed`: quit Telegram, run `sudo codesign --force --deep --sign - /Applications/Telegram.app`, reopen Telegram, then run `sudo tg keys`.
- No chats or messages found: run `tg refresh --keys`, then `tg sessions --top 50`.
- Wrong chat matched: use `tg sessions --top 100` and rerun with the exact `tgid_...` or `...@chatroom`.
- Missing media: open or download the media in Telegram first, then retry `tg image` or `tg export --media-dir ...`.
- Voice output defaults to normalized `.voice`; use `tg voice ... --format wav` after installing a compatible native voice decoder.
- Unknown issue: run `tg doctor` or `tg doctor "联系人或群名"` and follow the result.
