# MCP Server Design

Status: implemented in `src/mcp/` (phases 1–3 and 5). Differences from the
text below:

- Tools return `structuredContent` plus the same JSON as text, without an
  `outputSchema`, so a schema mismatch can never make a client reject a result.
- The HTTP transport is stateless: no `Mcp-Session-Id` is issued, so a server
  restart never strands a client session.
- Session names resolve to the best fuzzy match instead of returning a
  candidate list; every result echoes the resolved `session.id` and `name`.
- The owner passphrase hash lives in `~/.tg/mcp/owner.json` (0600), not in the
  Keychain. `serve --expires` and `install-service` are not implemented.

## Decision

Add a `tg mcp` subcommand that exposes tg's read-only commands as Model Context
Protocol (MCP) tools. It runs in one of two modes, both inside the same binary:

- `tg mcp stdio`: a local server for clients that can spawn a process (Claude
  Desktop, Claude Code, Codex, Cursor). No network and no auth.
- `tg mcp serve`: a Streamable HTTP server bound to the machine's Tailscale
  address, exposed to remote clients such as ChatGPT on the web through a reverse
  proxy on a small VPS that reaches it over the tailnet, and protected by a small
  single-user OAuth 2.1 authorization server built into tg.

The chat databases never leave the machine. The server opens them per request
through `SourceStore`, exactly as the CLI does. Only the tool results a client
asks for cross the network.

## Goals

- Typed MCP tools for `sessions`, `unread`, `messages`, `search`, `query`,
  `members`, and `doctor`, with results as structured JSON.
- One tool registry shared by stdio and HTTP.
- Remote use from ChatGPT on the web without copying any database to a server.
- An owner secret that is required before any remote client can read anything,
  plus revocation that takes effect immediately.
- Owner-controlled limits on what remote clients can see: session allow/deny
  lists, a maximum lookback window, and result size caps.
- No new async runtime. The CLI is synchronous and stays that way.

## Non-goals

- Write tools. tg never sends messages or modifies the source databases, and
  MCP doesn't change that.
- `keys` over MCP. Key capture reads process memory with LLDB/GDB and must stay
  a deliberate manual step.
- `export` over MCP in v1. It writes media files to disk; a later version can add
  a variant restricted to one output directory.
- Multi-user or hosted deployment. There is exactly one owner: the person whose
  machine holds the data.
- Running when the machine is asleep or off. The data lives on that machine.

## What constrains the remote design

These facts come from OpenAI's and the MCP project's documentation (checked
2026-11) and decide most of the HTTP and auth design:

1. **ChatGPT only offers "OAuth", "No authentication", or a mix of the two**
   when you add a custom MCP server. There is no static API key or custom header
   field, and OpenAI's plugin auth docs say ChatGPT does not present custom API
   keys. "Deploy with a secret" therefore has to mean an OAuth flow whose
   consent step checks the secret.
2. ChatGPT uses the authorization-code flow with PKCE `S256`, sends the RFC 8707
   `resource` parameter, and then sends `Authorization: Bearer <token>`. Client
   registration can use a Client ID Metadata Document (CIMD, preferred), Dynamic
   Client Registration (DCR, deprecated in the 2026-07-28 MCP spec but still
   supported), or a predefined client ID and secret.
3. ChatGPT's redirect URI is `https://chatgpt.com/connector_platform_oauth_redirect`
   when the authorization server returns the RFC 9207 `iss` parameter, and a
   per-connector `https://chatgpt.com/connector/oauth/{callback_id}` otherwise.
4. The endpoint must be public HTTPS with Streamable HTTP, typically at `/mcp`.
5. ChatGPT respects the `readOnlyHint` annotation. Tools without it are treated
   as write actions and need a confirmation each time.
6. MCP servers must publish RFC 9728 protected resource metadata and must accept
   only tokens issued for their own resource URL.

Sources: [OpenAI custom MCP server guide](https://developers.openai.com/api/docs/guides/custom-mcp-server),
[OpenAI plugin auth](https://developers.openai.com/plugins/build/auth),
[OpenAI connect to ChatGPT](https://developers.openai.com/plugins/deploy/connect-chatgpt),
[MCP authorization spec](https://modelcontextprotocol.io/specification/draft/basic/authorization).

## Architecture

```text
ChatGPT web ──HTTPS──> mcp.example.com (VPS reverse proxy, TLS)
                                            │  WireGuard via Tailscale,
                                            │  ACL: VPS tag -> this machine, tcp:8765 only
                                            ▼  http://<tailnet IP>:8765
                                     tg mcp serve (on the machine with the data)
                                     ├─ /.well-known/oauth-protected-resource
                                     ├─ /.well-known/oauth-authorization-server
                                     ├─ /oauth/register   (DCR)
                                     ├─ /oauth/authorize  (owner passphrase page)
                                     ├─ /oauth/token
                                     └─ /mcp              (JSON-RPC, bearer token)
                                            │
                                            ▼
                                     tool registry ──> db.rs / query.rs / doctor.rs
                                            │
                                            ▼
                                     SourceStore (read-only SQLCipher)

Claude Desktop ──spawn──> tg mcp stdio ──> same tool registry
```

Module layout:

| File | Responsibility |
|---|---|
| `src/mcp/mod.rs` | `tg mcp` subcommand wiring, config loading |
| `src/mcp/protocol.rs` | JSON-RPC 2.0 framing, `initialize`, `tools/list`, `tools/call`, `ping`, version negotiation |
| `src/mcp/tools.rs` | Tool definitions, input validation, calls into domain modules, output caps |
| `src/mcp/stdio.rs` | Newline-delimited JSON over stdin/stdout |
| `src/mcp/http.rs` | Streamable HTTP transport, Origin/Host checks, routing |
| `src/mcp/oauth.rs` | Authorization server, token store, passphrase verification |
| `src/mcp/policy.rs` | Session allow/deny lists, lookback window, audit log |

## Prerequisite: separate data from rendering

Today most domain functions print as they go. `db::read_messages` and
`db::search_messages` return a count, `list_members` and `doctor::run` return
`()`, and only `query` has a JSON writer. MCP needs the rows themselves.

Before any MCP code, change each function to return a typed, `Serialize` result
and move printing into `main.rs` (or a `render` module):

```rust
pub(crate) struct SessionSummary {
    pub id: String,            // the stable session username
    pub display_name: String,
    pub kind: SessionKind,     // direct, group, official
    pub unread_count: i64,
    pub last_active: i64,      // unix seconds
}

pub(crate) struct MessageRecord {
    pub session_id: String,
    pub local_id: i64,
    pub time: i64,
    pub sender_id: Option<String>,
    pub sender_name: String,
    pub kind: MessageKind,     // text, image, voice, file, video, sticker, link, system, other
    pub text: String,          // parsed body, never the raw XML
}

pub(crate) struct MessagePage {
    pub session: SessionSummary,
    pub messages: Vec<MessageRecord>,
    pub next_offset: Option<usize>,
}
```

Equivalent types cover search hits, members, and doctor checks. The CLI renders
these types and must produce byte-identical output to today; existing CLI tests
are the regression check. This step is worth doing even without MCP, because it
gives every command a consistent `--format json`.

## Session identity

The CLI accepts a display name and resolves it with fuzzy scoring
(`resolve_username_for_messages`). Remote models will guess names, so silent
fuzzy resolution could read the wrong person's chat.

MCP tools take a `session` string with these rules:

1. An exact session ID (`tgid_…` or a group ID) is used as is.
2. Otherwise the name is resolved. If exactly one candidate is clearly best, it
   is used and the result echoes the resolved `session.id` and `display_name`.
3. If several candidates score closely, the tool returns an `isError` result
   listing up to 10 candidates with IDs, so the model can retry with an exact ID.

`list_sessions` always returns IDs, so the usual flow is list, then read by ID.

## Tools

All tools carry `annotations: { readOnlyHint: true, destructiveHint: false,
openWorldHint: false }`, an `inputSchema`, and an `outputSchema`. Results are
returned as `structuredContent` plus a compact text rendering in `content`, for
clients that ignore structured output.

| Tool | Maps to | Inputs | Notes |
|---|---|---|---|
| `list_sessions` | `db::list_sessions` | `query?`, `limit` (default 30, max 200) | Reads only contact and session DBs |
| `list_unread` | `db::list_unread_sessions` | `query?`, `limit` | Same light scope |
| `read_messages` | `db::read_messages` | `session`, `limit` (default 200, max 2000), `offset` (counted from the paging end in either order), `since?`, `until?`, `order` (`newest`/`oldest`), `contains?` | Returns `next_offset` while more remain |
| `search_messages` | `db::search_messages` | `query`, `since?` (default recent window), `limit` (default 20, max 100) | Upstream FTS; errors point to `query_messages` if the FTS DB is missing |
| `query_messages` | `query::run` | `session?`, `contains[]`, `not_contains[]`, `match` (`all`/`any`), `has[]` (media types), `since?`, `until?`, `order`, `limit` (max 2000), `offset` | No raw-body fields over MCP |
| `list_members` | `db::list_members` / `list_member_friends` | `session`, `inactive_since?`, `friends_only` | |
| `diagnose` | `doctor::run` | `session?` | Paths are redacted in remote mode |

Common rules:

- Times accept ISO 8601 or the CLI's relative forms (`1h`, `today`) and are
  returned as unix seconds plus a local ISO string.
- `name_mode` defaults to `PersonalRemark` locally and to `Anonymous` in remote
  mode, so your private contact remarks are not sent to a remote model unless
  the config turns them back on.
- Each text field is capped (default 2,000 characters) and each tool result is
  capped (default 256 KB of JSON). A capped result sets `truncated: true` and
  returns `next_offset`.
- No raw XML, no `packed_info_data`, no media bytes, and no source file paths.
  Media appears as `kind` plus a short label such as a file name.

Not included: a raw SQL tool, media download, and anything that writes.

## Protocol details

- JSON-RPC 2.0. Methods: `initialize`, `notifications/initialized`, `ping`,
  `tools/list`, `tools/call`. No resources, prompts, sampling, or subscriptions
  in v1.
- Version negotiation: support `2025-06-18`, `2025-11-25`, and `2026-07-28`.
  Echo the client's version if supported, otherwise answer with the newest one.
  Re-check the current spec revision before implementation.
- `serverInfo.name` is `tg`; tool descriptions use only neutral wording.
- Handwritten on `serde_json`, which is already a dependency. The official
  `rmcp` crate would bring in tokio and an HTTP framework, which is heavy for a
  synchronous tool that serves one person.

### stdio transport

One JSON message per line on stdin and stdout; logs go to stderr only. Requests
are handled one at a time. Client config:

```json
{ "mcpServers": { "tg": { "command": "tg", "args": ["mcp", "stdio"] } } }
```

stdio mode trusts its parent process the same way the CLI trusts its shell, so
it needs no auth, and policy limits apply only when `--remote-policy` is given.

### Streamable HTTP transport

- One endpoint, `/mcp`. `POST` carries one JSON-RPC message and gets a single
  `application/json` response. tg never streams partial results, so it doesn't
  open SSE streams; `GET /mcp` returns `405`, which the spec allows.
- Assign `Mcp-Session-Id` at `initialize` and require it afterward. Validate the
  `MCP-Protocol-Version` header.
- `listen` must be a loopback or Tailscale address (`127.0.0.0/8`,
  `100.64.0.0/10`, `fd7a:115c:a1e0::/48`); `serve` refuses anything else,
  including `0.0.0.0`. Binding the tailnet address keeps the port off the Mac's
  LAN and Wi-Fi interfaces.
- Check `Host` against the configured public hostname (the proxy passes it
  through unchanged), and reject unexpected `Origin` values, to block DNS rebinding from
  a local browser.
- Use the `X-Forwarded-For` value the proxy sets for the rate limiter and audit
  log, and trust it only when the TCP peer is the configured `trusted_proxy`
  tailnet address.
- Every `/mcp` request without a valid token returns `401` with
  `WWW-Authenticate: Bearer resource_metadata="https://<host>/.well-known/oauth-protected-resource"`.
- **The HTTP mode always requires auth.** A "no auth on private addresses" mode
  would be unsafe here, because the gateway turns every public request into a
  request from a trusted tailnet peer. Local unauthenticated use goes through stdio instead.
- HTTP library: `tiny_http` (synchronous, small, no TLS needed because the proxy
  terminates TLS and the proxy-to-machine hop is WireGuard). Requests are handled on one worker thread with a bounded
  queue; tg's own `jobs` setting already parallelizes the heavy reads.

## Auth: a single-user authorization server inside tg

The "secret" is an **owner passphrase** set with `tg mcp init`. ChatGPT never
sees it. ChatGPT gets an OAuth token only after you type the passphrase into
tg's consent page in your own browser during the connector linking flow.

### Why built in

| Option | Verdict |
|---|---|
| No auth with a secret URL path | Rejected. The URL is stored in the connector config and can end up in logs; anyone holding it reads everything, and it can't be revoked without changing the URL |
| External IdP (Auth0, WorkOS, Cloudflare Access for SaaS) | Works and is the fallback, but adds an account, a token-validation dependency (JWKS), and configuration in two places for one user |
| Built-in AS | Chosen. About 500 lines on existing crates (`sha2`, `hmac`, `pbkdf2`, `zeroize`), with opaque tokens that tg alone issues and checks |

### Endpoints

- `GET /.well-known/oauth-protected-resource`: `resource` is
  `https://<host>/mcp` and `authorization_servers` is `["https://<host>"]`.
- `GET /.well-known/oauth-authorization-server`: issuer, endpoints,
  `code_challenge_methods_supported: ["S256"]`,
  `authorization_response_iss_parameter_supported: true`,
  `grant_types_supported: ["authorization_code", "refresh_token"]`,
  `token_endpoint_auth_methods_supported: ["none", "client_secret_post"]`,
  `scopes_supported: ["tg.read"]`, and `registration_endpoint`.
- `POST /oauth/register` (DCR): accepts a registration only when every
  `redirect_uri` matches the redirect allowlist; otherwise returns
  `invalid_redirect_uri`. Stores at most 10 clients.
- `GET /oauth/authorize`: validates `client_id`, exact `redirect_uri`, `S256`
  challenge, `resource` equal to the `/mcp` URL, and `state`. Renders a minimal
  page showing the requesting redirect host and a passphrase field.
- `POST /oauth/authorize`: verifies the passphrase, then redirects with `code`,
  `state`, and `iss`. Codes are single use and expire after 60 seconds.
- `POST /oauth/token`: exchanges code plus `code_verifier`, or rotates a refresh
  token.

### Client registration

v1 implements **DCR** plus the **predefined client** path, and skips CIMD. CIMD
requires an outbound HTTPS fetch of `https://chatgpt.com/oauth/client.json` and
SSRF-safe handling of it, which means another HTTP client and TLS stack. DCR is
deprecated in the newest spec but ChatGPT still supports it, and the redirect
allowlist gives the same protection for a single-user server. Revisit CIMD when
ChatGPT stops using DCR.

`tg mcp init` can also print a predefined `client_id`/`client_secret` for
ChatGPT's advanced OAuth settings, if that UI offers static client credentials
(the docs say static credentials are used when provided; confirm in the spike).

Redirect allowlist, defaulting to:

```text
https://chatgpt.com/connector_platform_oauth_redirect
https://chatgpt.com/connector/oauth/*        (single path segment)
```

The config can add other clients' redirect URIs, such as Claude's remote
connector callback.

### Tokens

- Access tokens: 32 random bytes, base64url, 1 hour lifetime, audience bound to
  the `/mcp` resource URL.
- Refresh tokens: 32 random bytes, 30 day lifetime, rotated on every use.
  Reusing an already-rotated refresh token revokes the whole grant, which is
  the standard sign of a stolen token.
- Storage: only SHA-256 hashes, in `~/.tg/mcp/oauth.json` (file `0600`,
  directory `0700`). Plaintext tokens exist only in responses.
- `tg mcp revoke [--client <id> | --all]` deletes grants; the running server
  re-reads the store on each request, so revocation takes effect immediately.

### Passphrase

- `tg mcp init` asks for a passphrase of at least 16 characters and stores a
  PBKDF2-HMAC-SHA256 hash (600,000 iterations, random salt) in
  `~/.tg/mcp/owner.json`. Stored in the macOS Keychain instead when available.
- Failed attempts: 5 per 15 minutes globally (there is one owner, so a global
  limit is correct), then the authorize endpoint answers `429` until the window
  passes.
- The consent page sets `Cache-Control: no-store`, `X-Frame-Options: DENY`, and a
  CSRF token tied to the authorization request.

### Defense in depth (optional)

- OpenAI presents an mTLS client certificate (`mtls.prod.connectors.openai.com`)
  and publishes egress IP ranges. The reverse proxy can restrict `/mcp` and
  `/oauth/token` to those ranges (Caddy `remote_ip` matcher). Leave
  `/oauth/authorize` open, because your own browser loads it. This identifies
  ChatGPT, not you, so it complements the passphrase and does not replace it.
- `--expires 8h` on `tg mcp serve` stops the server after a fixed time, for
  people who only want it on while they use it.

## Exposure: VPS reverse proxy over Tailscale

The server must be reachable over public HTTPS while the data stays on the
machine that holds it. The chosen route reuses an existing VPS that already runs
Caddy for other sites and is already on the same tailnet:

1. DNS for `mcp.example.com` points at the VPS. Caddy gets the certificate and
   terminates TLS, as it does for its other sites.
2. Caddy reverse-proxies `mcp.example.com` to `http://<tailnet IP>:8765`. The
   hop from the VPS to the machine is WireGuard, so it is encrypted even though
   tg speaks plain HTTP.
3. `tg mcp serve` listens only on the machine's tailnet address.
4. The tailnet policy allows the VPS's tag to reach that machine on `tcp:8765`
   and nothing else.

Why this route:

- No new account, daemon, or tunnel agent on the Mac; it reuses the proxy, TLS,
  and DNS setup already in place.
- The VPS never holds keys, databases, or tokens. It sees TLS-terminated traffic,
  which includes tool results, so compromising it exposes what is queried while
  it is compromised. That is the same trust as any TLS-terminating edge
  (Cloudflare included), and the policy limits above bound it.
- Access is narrow at two layers: the tailnet ACL admits one port from one tag,
  and tg admits only bearer tokens it issued.

Caddy site sketch:

```caddy
mcp.example.com {
	# Caddy sets X-Forwarded-For itself; tg trusts it only from this peer.
	reverse_proxy <tailnet IP>:8765
	header {
		Strict-Transport-Security "max-age=31536000"
		-Server
	}
}
```

Tailnet policy sketch (add to `grants`):

```hujson
{
	"src": ["tag:<vps tag>"],
	"dst": ["<machine host alias>"],
	"ip":  ["tcp:8765"],
}
```

Things to verify during setup, because they are host-specific:

- If Caddy runs in a Docker container, the container must be able to route to
  the tailnet. Docker's bridge NAT usually forwards to `tailscale0`, but a host
  firewall with a default-deny forward policy (UFW) blocks it until a forward
  rule allows the Caddy container's address to the machine's tailnet IP on
  `tcp:8765`. The alternative is a small host-level forwarder, but a forward
  rule is simpler.
- If the VPS is a Tailscale peer relay, granting it reach into the tailnet is a
  real widening of what that tag can do. Keep the grant to the single port.
- The Mac must be awake. Use `caffeinate` or Energy Saver settings while
  `serve` runs, or `--expires`.

Alternatives, if no VPS is available:

| Option | Stable URL | Notes |
|---|---|---|
| Cloudflare named tunnel | Yes | `cloudflared` on the Mac, ingress to the loopback port |
| Tailscale Funnel | Yes, `*.ts.net` | Simplest; fewer edge controls |
| OpenAI Secure MCP Tunnel | Yes | No public endpoint; check plan availability |
| ngrok / quick tunnels | Usually not | A changing URL breaks the OAuth resource binding |

In every case the public hostname is part of the OAuth `resource` and `issuer`,
so tg's config must name it (`public_url`) and a hostname change forces
re-linking.

## Policy and privacy

Remote mode sends chat content to a third-party model provider. That is the main
risk of this feature, and the design limits it as follows.

`~/.tg/mcp/config.json`:

```json
{
  "public_url": "https://mcp.example.com",
  "listen": "100.x.y.z:8765",
  "trusted_proxy": "100.a.b.c",
  "remote": {
    "allow_sessions": [],
    "deny_sessions": ["tgid_..."],
    "max_lookback_days": 90,
    "show_contact_remarks": false,
    "max_result_bytes": 262144,
    "max_text_chars": 2000
  },
  "extra_redirect_uris": []
}
```

- `allow_sessions` empty means all sessions except `deny_sessions`. Denied
  sessions are invisible: they are removed from lists and searches, and reads
  return "not found", so the model can't tell that a denied session exists.
- `max_lookback_days` clamps every `since` and rejects reads older than the
  window.
- Audit log in `~/.tg/mcp/audit.log` (`0600`): time, client ID, tool name,
  argument *shape* (which fields were set, not their values), row count, and
  duration. It never records message text or search terms.
  `tg mcp audit --since 1d` prints it.
- Remote mode never returns local paths, key file locations, or database names
  in errors; `diagnose` reports pass/fail per check only.

User-facing caveats to document in the README:

- In ChatGPT, results from tool calls go to OpenAI under your ChatGPT data
  settings. Business and Enterprise workspaces may have different retention.
- Developer mode marks the connector as unverified, and some ChatGPT features
  may behave differently while it is on. Check current OpenAI documentation.

## CLI surface

```text
tg mcp stdio [--remote-policy]
tg mcp init                     # passphrase, public_url, writes ~/.tg/mcp/
tg mcp serve [--expires 8h]     # HTTP on the tailnet address, requires init
tg mcp status                   # config, grants, last requests
tg mcp revoke [--client ID | --all]
tg mcp audit [--since 1d]
```

`serve` refuses to start if `init` has not run, if `public_url` is not HTTPS, if
`listen` is not a loopback or tailnet address, or if the state files have
permissions wider than `0600`/`0700`.

A launchd plist (`tg mcp install-service`) is out of scope for v1; the README
shows a manual plist instead.

## Implementation phases

Each phase lands on its own and is usable.

1. **Structured results.** Return `Serialize` types from `db.rs`, `query.rs`,
   and `doctor.rs`; the CLI renders them. Check: existing CLI tests pass with
   unchanged output, plus unit tests for the new types.
2. **stdio MCP.** Protocol, tool registry, session identity rules, caps. Check:
   protocol tests with scripted JSON-RPC exchanges over a fixture database, and
   a manual run in MCP Inspector and Claude Desktop.
3. **HTTP transport and OAuth.** `tiny_http`, the endpoints above, token store,
   passphrase, rate limits. Check: tests for PKCE, redirect allowlist, code
   reuse, refresh rotation and reuse detection, audience checks, and `401`
   discovery; then MCP Inspector against the loopback port.
4. **Gateway and ChatGPT spike.** Add the tailnet grant and the Caddy site,
   confirm the proxy reaches the port (and only that port), then link the
   connector on a real account. Confirm the redirect URI, DCR vs. predefined client, whether
   `structuredContent` is shown, and the size limits. Update this document with
   what was found.
5. **Policy and audit.** Allow/deny lists, lookback clamp, anonymous default,
   audit log, `status`/`revoke`/`audit`.

## New dependencies

| Crate | Why | Alternatives rejected |
|---|---|---|
| `tiny_http` | Synchronous HTTP server on a private address | `axum`/`hyper` need tokio |
| `getrandom` | Token and salt generation | Already a transitive dependency; make it direct |
| `base64` | base64url for tokens and PKCE | Hand-rolled encoding |

No TLS crate, no HTTP client, no JWT library.

## Open questions

- Does ChatGPT's connector UI accept a predefined client ID and secret, or only
  DCR/CIMD? Resolved in the phase 4 spike.
- Does ChatGPT render `structuredContent`, or only the text `content`? This
  decides how much effort goes into the text rendering.
- Should `read_messages` also accept a `local_id` cursor instead of an offset?
  Offsets shift when new messages arrive between pages.
- Is a session allowlist (deny by default) a better default for remote mode than
  a denylist?
