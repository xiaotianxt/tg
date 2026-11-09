## `tg messages --offset` pages back through history

`tg messages` shows the latest messages by default, but paging with `--offset`
used to jump to the start of the chat: `--limit 50 --offset 50` returned
messages 51–100 counted from the oldest message. `--offset` now counts back
from the newest message, so each page is the one just before the last. This
matches how MCP `read_messages` already pages with `order=newest`.

```bash
tg messages "张三" --limit 20               # latest 20
tg messages "张三" --limit 20 --offset 20   # the 20 before those
tg messages "张三" --head --limit 20 --offset 20   # messages 21–40 from the start
```

With an offset, the header now reads `Showing 20 messages before the latest 20`
instead of claiming to show the latest messages.

This changes paging for scripts: a script that paged with `--offset` and no
`--head` now gets older messages counted back from the newest. Add `--head` to
keep the old pages. `--tail` is still accepted; it is now the default and does
nothing.

### Internal

Message direction is one `newest`/`oldest` type shared by `messages`,
`query --order`, and the MCP tools, and the default one-year search window is
decided in one place. Neither changes output.
