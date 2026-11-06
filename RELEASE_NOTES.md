## MCP server

Expose tg's read-only commands as MCP tools so local assistants and remote
clients such as ChatGPT can read and search your local chat history.

Patch follow-up: `read_messages` now pages through a whole time range. It
accepts `until`, honors `offset` in both orders, and returns `next_offset`
while messages remain, so clients no longer receive the same newest batch
repeatedly.

Second follow-up: message and search tools no longer cap `limit`; it is only a
page size (default 1000 messages, 100 search hits). The single size guard is
the result cap, now 16 MiB by default, and a result trimmed to it still
returns `next_offset` or `has_more`.

- Add `tg mcp stdio` for local MCP clients. It needs no network or authentication.
- Add `tg mcp init` / `tg mcp serve` for remote clients over Streamable HTTP. tg only listens on loopback or Tailscale addresses and expects your own HTTPS reverse proxy in front.
- Remote access is protected by a built-in single-user OAuth 2.1 server: dynamic client registration with a redirect allowlist, PKCE S256, an owner passphrase on the consent page, short-lived hashed access tokens, and rotating refresh tokens with replay detection.
- Tools: `list_sessions`, `list_unread`, `read_messages`, `search_messages`, `query_messages`, `list_members`, `diagnose`. All are read-only and return structured JSON.
- Remote policy hides your contact remarks, limits results to the last 90 days by default, caps text and result size, and can allow or deny specific sessions.
- Add `tg mcp status`, `tg mcp revoke`, and `tg mcp audit`. The audit log records tool names and argument names only, never content or search terms.
- Read commands now build typed results before rendering. CLI output is unchanged.

All source database access remains local and read-only. Tool results do enter the remote model's context, so enable remote mode only for clients you trust.
