# tgreader User Workflow Design

## Product Frame

tgreader should feel like a chat-history tool, not a database recovery toolkit. The main path is:

```bash
tgreader "张三"
tgreader "产品讨论群" --since today
tgreader search "项目"
tgreader export "张三" --format json
```

Lower-level steps such as key extraction and database decryption remain available, but they should move into maintenance and diagnostic flows.

## Command Taxonomy

### Primary Tasks

- `tgreader "<chat>"`: read a chat. This is the default route for unknown first arguments.
- `tgreader search "<query>"`: search globally across chats.
- `tgreader export "<chat>"`: export a chat.
- `tgreader image "<chat>"`: inspect or export cached images from a chat.

### Discovery

- `tgreader sessions`: list recent/high-volume chats.
- Future: `tgreader sessions --find "<name>"` to narrow session discovery without reading messages.

### Maintenance

- `tgreader refresh`: refresh decrypted cache with existing keys, then refresh keys and retry if message/contact databases fail.
- `tgreader refresh --keys`: force key extraction first, then refresh decrypted cache.
- `tgreader keys`: extract keys directly.
- `tgreader decrypt`: decrypt directly.

### Diagnostics

- `tgreader doctor`: report whether Telegram is running, scanner is present, keys exist, decrypted cache exists, and contact/message databases are readable.
- `tgreader doctor "<chat>"`: additionally resolve the chat query and check whether the message table exists and has rows.

## Search Semantics

- `tgreader search "项目"` is global search.
- `tgreader "张三" --search "项目"` is within-chat search.
- Global search should support `--since`, matching the time filter vocabulary used by `messages`.

## Error and Recovery Principles

- "No messages found" should not be the only feedback when cache refresh failed.
- Retry is useful only when bounded and tied to a plausible recovery cause.
- Diagnostic commands should be read-only unless explicitly named as maintenance commands.
- Progress and warnings go to stderr; requested data and diagnostics go to stdout.

## First Implementation Slice

1. Keep shorthand chat reads: `tgreader "张三"`.
2. Add `doctor [chat]` for concrete state inspection.
3. Add `refresh [--keys]` for one-command cache maintenance.
4. Add `search --since` for symmetry with chat reads.
