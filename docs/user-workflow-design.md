# tg User Workflow Design

## Product Frame

tg should feel like a chat-history tool, not a database recovery toolkit. The main path is:

```bash
tg "张三"
tg "产品讨论群" --since today
tg search "项目"
tg export "张三"
```

Lower-level steps such as key extraction and database decryption remain available, but they should move into maintenance and diagnostic flows.

## Command Taxonomy

### Primary Tasks

- `tg "<chat>"`: read a chat. This is the default route for unknown first arguments.
- `tg search "<query>"`: search globally across chats.
- `tg export "<chat>"`: export a chat.
- `tg image "<chat>"`: inspect or export cached images from a chat.

### Discovery

- `tg sessions`: list recently active chats and unread counts.
- `tg sessions "<name>"`: narrow session discovery without reading messages.
- `tg unread`: list only chats with unread messages using the lightweight session cache.

### Maintenance

- `tg refresh`: refresh decrypted cache with existing keys, then refresh keys and retry if message/contact databases fail.
- `tg refresh --keys`: force key extraction first, then refresh decrypted cache.
- `tg keys`: extract keys directly.
- `tg decrypt`: decrypt directly.

### Diagnostics

- `tg doctor`: report whether Telegram is running, scanner is present, keys exist, decrypted cache exists, and contact/message databases are readable.
- `tg doctor "<chat>"`: additionally resolve the chat query and check whether the message table exists and has rows.

## Search Semantics

- `tg search "项目"` is global search.
- `tg "张三" --search "项目"` is within-chat search.
- Global search should support `--since`, matching the time filter vocabulary used by `messages`.

## Error and Recovery Principles

- "No messages found" should not be the only feedback when cache refresh failed.
- Retry is useful only when bounded and tied to a plausible recovery cause.
- Diagnostic commands should be read-only unless explicitly named as maintenance commands.
- Progress and warnings go to stderr; requested data and diagnostics go to stdout.

## First Implementation Slice

1. Keep shorthand chat reads: `tg "张三"`.
2. Add `doctor [chat]` for concrete state inspection.
3. Add `refresh [--keys]` for one-command cache maintenance.
4. Add `search --since` for symmetry with chat reads.
