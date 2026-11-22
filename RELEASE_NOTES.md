## tg is now written in Go

3.0 replaces the Rust implementation with Go. The binary name, commands,
flags, `~/.tg` files, MCP tools and endpoints stay the same; upgrade with
`brew upgrade tg` and restart `tg mcp serve` if you run it.

## Fixes that change output

- Voice messages show their real length. Current messages used to read as
  `1s` and older ones showed milliseconds labelled as seconds.
- A private message that starts with a link such as `https://…` keeps its
  text; it was split into sender `https` and text `//…`.
- `query` and `export` name the sender the same way `messages` does: your own
  messages show as `我`, the other side of a private chat by name, and system
  messages as `系统`.
- A chat without a contact entry is shown by its username everywhere, never
  by an internal table name.
- Names containing terminal control or text-direction characters are shown
  escaped as `\u{..}`, so a contact name can no longer recolor or reorder a
  terminal line. Emoji display as before.
- `query --until` is exclusive, like `messages` and the MCP tools.
- `query --format json` writes keys in `--fields` order, and an empty result
  prints nothing on stdout (the note goes to stderr).
- `export` saves cached images with their real extension instead of `.dat`,
  and skips (with a warning) encrypted media it cannot decrypt instead of
  copying ciphertext.
- Errors print once as `error: <message>`.

## MCP

- The HTTP endpoint rejects requests whose `Origin` names another site.
- In remote mode, `list_sessions` and `diagnose` hide contact remarks like the
  other tools, and `list_members` honours the lookback window.

## Building from source

Building needs Go, a C compiler and make (plus the OpenSSL development
package on Linux) instead of Rust.
