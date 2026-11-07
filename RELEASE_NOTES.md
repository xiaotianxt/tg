## Leaner MCP reads, and images over MCP

The MCP server now sends much less text per message, and can show cached
images to the model. Everything stays read-only and local; nothing new is
written to disk.

### `read_messages` returns a transcript

`read_messages` now returns a plain-text transcript instead of JSON: one
`== YYYY-MM-DD ==` line per local day, then one `HH:MM sender: text` line per
message, with indented continuation lines for multi-line messages. A trimmed
result ends with a `[truncated]` line naming the `offset` to continue from.

On one busy group (2,211 messages in 24 hours) the result went from 277k to
88k characters, with every message kept. Before, about 80% of the result was
JSON field names and the same timestamp written twice, and the whole result
was sent a second time as `structuredContent`. The transcript is sent once, as
text only.

This changes the result format: a client that parsed `read_messages` JSON must
read the text lines instead. The other tools still return JSON.

### New `get_media` tool

Message tools already show images as `[img:<id>]`. `get_media` takes up to 10
of those ids for one chat and returns them as MCP images:

- `size: "preview"` (default) returns the client's standard copy, falling back
  to the thumbnail; `size: "original"` returns the full-resolution copy when it
  was downloaded.
- A video id (the name in `[视频 WxH <id>]`) returns the cached cover.
- An id that was never downloaded is reported as text; the other ids in the
  call still return.

Remote clients get images only after the owner sets
`"allow_media": true` under `remote` in `~/.tg/mcp/config.json`. Session
allow/deny lists apply, and the lookback window applies per cache month.
Voice is still sent as text: the transcript already includes the client's own
voice-to-text when it exists.
