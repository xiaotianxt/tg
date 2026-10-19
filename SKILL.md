---
name: tg
description: Use when the user wants to read, search, inspect, back up, export, or troubleshoot local macOS or Linux Telegram chat history with tg. Keep the user's chat data local and guide them through the shortest working tg command flow.
when_to_use: Trigger for requests mentioning Telegram聊天记录, Telegram聊天, Telegram群, Telegram里, local chat backup, message search, message export, or troubleshooting why local chat history cannot be read.
---

# tg

Canonical source: https://github.com/xiaotianxt/tg/blob/main/SKILL.md

## When To Use

Use this skill for user goals like:

- "帮我读一下和某个人的Telegram聊天记录"
- "查Telegram里有没有某个关键词"
- "导出这个Telegram群的聊天记录"
- "把Telegram聊天备份成 json/csv/txt"
- "为什么本机Telegram聊天记录读不出来"

Do not wait for the user to name tg. tg is the tool; the user goal is local desktop Telegram history access.

## Privacy

Chat data is private. Keep work local by default, avoid printing more message content than the user asked for, and treat exports as sensitive.
For summary requests, choose display names by the target. If the user clearly names a 1-on-1 person, omit `--anonymous` so the summary uses the user's intended name for that person. If the target is a group chat, a room, a global search, or the target type is unclear, use `--anonymous` whenever the command supports it to avoid exposing personal contact remarks in assistant-visible output and exported sender names.
`~/.tg/all_keys.json` and `~/.tg/key_material.bin` are sensitive local state. Read commands open Telegram's encrypted databases directly and never create a decrypted database cache.
For exhaustive keyword lookup over locally cached messages, prefer `tg query --contains "..." --all-time`; `tg search` is the faster upstream-FTS path and does not cover every message type or nested card body.

## First Setup

For a fresh setup, ask the user to open and log in to the desktop Telegram client first.

On macOS, quit Telegram, enable developer tools, sign the app, and restart it:

```bash
sudo DevToolsSecurity -enable
sudo codesign --force --deep --sign - /Applications/Telegram.app
open -a Telegram
sudo tg keys --method login --timeout 180
```

If Telegram is installed somewhere else, use that `.app` path instead.

On Linux, install GDB and start the same cross-platform login capture:

```bash
sudo apt install gdb
sudo tg keys --method login --timeout 180
```

While `login` is waiting, log out of the account in Telegram and log back in once. Then verify the direct reader:

```bash
tg sessions --top 50
tg "联系人" --limit 50
tg "群名" --limit 50 --anonymous
```

After the captured account material validates against the local databases, later `tg keys` derives keys for new database salts without another attach.

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

Inspect a complete current group roster or find locally inactive members:

```bash
tg members "产品讨论群" --anonymous
tg members "产品讨论群" --friends --anonymous
tg members "产品讨论群" --inactive-since 1y --anonymous
```

Use `members --friends --anonymous` for existing friends in a group. It intersects the cached roster with conservative local relationship flags, without reading message history. Results are not exhaustive or a live reciprocal-friendship check. Do not combine `--friends` with `--inactive-since`.

Treat inactivity output as a review list, not proof of membership age or permission to remove anyone. If the local sender history starts after the cutoff, surface the command's coverage warning.

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
```

Use `tg query --help` when the user asks what `query` can return or filter on.

Query safety rules:

- `search` and `query` default to the recent 365-day window; `export` always writes the complete chat archive.
- With `--all-time`, `query` requires at least `--contains`, `--raw-contains`, `--has`, or `--since`.
- Empty `--contains` / `--not` values are rejected.
- Use `--session`, `--since`, and a reasonable `--limit` when results could be large.
- Table output escapes terminal control characters; use `--format json` for machine parsing.

Diagnose:

```bash
tg doctor
tg doctor "张三"
```

Export:

```bash
tg export "张三"
tg export "张三" --output exported/zhangsan
tg export "产品讨论群" --output exported/group
```

Each export writes `chat.txt`, `chat.csv`, `chat.json`, and locally available media under `media/`.

Media is exported only as part of the complete chat archive. Locally cached images, videos, stickers, files, and voices go under `media/`; voices prefer WAV and fall back to native `.voice` when decoding is unavailable.

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

- `Telegram is not running`: open and log in to the desktop Telegram client, then run `sudo tg keys`.
- macOS `task_for_pid failed`: quit Telegram, sign it as above, reopen it, then run `sudo tg keys --method login --timeout 180` and log out/in once while it waits.
- Linux attach failure: install GDB, verify ptrace permissions, then run `sudo tg keys --method login --timeout 180`.
- No chats or messages found: run `sudo tg keys`, then `tg doctor` and `tg sessions --top 50`.
- Wrong chat matched: use `tg sessions --top 100` and rerun with the exact `tgid_...` or `...@chatroom`.
- Missing media: open or download it in Telegram first, then retry `tg export CHAT`.
- Voice export prefers WAV and falls back to native `.voice`; install the Homebrew decoder for playable WAV output.
- Unknown issue: run `tg doctor` or `tg doctor "联系人或群名"` and follow the result.
