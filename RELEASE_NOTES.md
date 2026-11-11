## Filter by sender, with a stable id for every sender

`messages`, `query`, and the MCP `read_messages` and `query_messages` tools
take a sender filter. Before, finding what one person said meant searching
for their name, which matched messages that mention them, not the ones they
sent.

```bash
tg messages "产品讨论群" --sender me
tg query --session "产品讨论群" --sender "张三" --format json --fields timestamp,sender,sender_id,body
```

The value is `me`, an account id, or a name exactly as tg shows it (remark,
nickname, or group nickname; case is ignored). When several accounts share
the name, tg fails and lists their ids instead of mixing them; inside a
group, current members win. `--anonymous` and remote MCP never match contact
remarks. `search` uses the client's own index and has no sender filter.

Every message now carries its sender's account id, which stays the same
across chats and display names:

- `query --fields sender_id`, and a `sender_id` on each `query_messages`
  result.
- The `read_messages` header lists each sender shown as `name = id`.
- You appear as `我` in every listing. `query` used to show your nickname.

## Quoted messages show what they quote

Replies that quote a file, a link, or another reply showed `[未知(57)]`,
`[未知(6)]`, and similar. They now show the quoted text, file name, or link
title, with the quoted sender.

## Empty early ranges say why

`read_messages` now states when the chat's readable history starts, and
`tg messages --since` notes it when the requested range starts earlier.
Messages before that point are not stored on this device (or, over remote
MCP, are past the lookback limit), so an empty early range means missing
history, not silence.

## Search covers the full history by default

`search`, `query`, `search_messages`, and `query_messages` used to read only
the last 365 days unless given `--since` or `--all-time`. They now read the
whole local history; `--since` and `--until` still narrow it. The window did
not make searches faster, so the only change is that older matches appear.
Remote MCP still applies its lookback limit.

`--all-time` and `all_time` are still accepted and change nothing;
`read_messages` accepts `all_time` too. `tg messages --all-time` keeps its
meaning: it lifts the default 50-message limit.

`query` still refuses to return every message of every chat without a
filter, but `--session` alone is now enough.
