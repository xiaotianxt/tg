---
name: tg
description: Use when the user wants to read, search, inspect, back up, export, or troubleshoot local macOS Telegram chat history with tg. Keep the user's chat data local and guide them through the shortest working tg command flow.
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
tg "联系人或群名" --limit 50
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
tg messages "张三" --search "关键词"
tg messages "张三" --head --limit 20
tg messages "张三" --tail --limit 20
```

Search globally:

```bash
tg search "关键词" --limit 50
tg search "关键词" --since today
```

Use structured lookup when the user wants precise filters, multiple keywords,
excluded words, selected output fields, or JSON lines for a local analysis step.
This is not a raw SQL interface; pass user intent as filters:

```bash
tg query --contains "关键词" --limit 50
tg query --session "张三" --contains "关键词" --fields time,sender,body --limit 20
tg query --contains "项目" --contains "上线" --match-mode all --since today
tg query --contains "项目" --not "已取消" --format json --fields timestamp,session,body
tg schema --db message_0
```

Use `tg schema` when the user asks what `query` can return or filter on. It
shows the public query contract, not raw database table or column names.

Query safety rules:

- `query` requires at least `--contains` or `--since`.
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
tg export "张三" --format csv --output exported/zhangsan
tg export "张三" --format json --output exported/zhangsan
tg export "张三" --format json --output exported/zhangsan --media-dir exported/zhangsan/media
```

Export cached images:

```bash
tg image "张三" --list --limit 20
tg image "张三" --index 3
tg image "张三" --all --limit 10 --output exported/images
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

Date, datetime, and displayed message times use the current system time zone.

## Troubleshooting

- `Telegram is not running`: open and log in to macOS Telegram, then run `sudo tg keys`.
- `task_for_pid failed`: quit Telegram, run `sudo codesign --force --deep --sign - /Applications/Telegram.app`, reopen Telegram, then run `sudo tg keys`.
- No chats or messages found: run `tg refresh --keys`, then `tg sessions --top 50`.
- Wrong chat matched: use `tg sessions --top 100` and rerun with the exact `tgid_...` or `...@chatroom`.
- Missing media: open or download the media in Telegram first, then retry `tg image` or `tg export --media-dir ...`.
- Unknown issue: run `tg doctor` or `tg doctor "联系人或群名"` and follow the result.
