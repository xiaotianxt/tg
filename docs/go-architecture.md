# Go Architecture

tg 3.0 replaces the Rust implementation with Go behind the same external
contract (binary name, CLI, `~/.tg` files, MCP tools and endpoints), except
for the deliberate changes listed under "Decided behaviour changes". The
black-box suite in `e2e/` is the contract; it ran green against the Rust
binary before the Go code existed.

## Packages

Dependencies point down this list only. A package never imports one listed
above it.

| Package | Owns |
|---|---|
| `cmd/tg` | argv rewrite (unknown first arg → `messages`), cobra commands, flag defaults and conflicts, building `env.Env`, mapping errors to `error: <msg>` + exit 1 |
| `internal/mcp/...` | `rpc` (JSON-RPC classification, stateless), `tools` (tool registry, arguments, policy, truncation), `httpx` (HTTP routing, host/origin/size/auth checks, audit log), `oauth` (grants, consents, codes, tokens as explicit states with pure transitions taking `now`), `config` (config/owner/state files) |
| `internal/doctor` | setup diagnosis report |
| `internal/export` | chat archive writers (txt/csv/json) and the media job pipeline |
| `internal/query` | structured query validation, shard/rowid planning, filtering |
| `internal/render` | all CLI text: sessions, messages, search, members, friends, doctor, query table/NDJSON; terminal escaping in one place |
| `internal/history` | sessions/unread, read messages, FTS search, members/friends, chat probe; returns typed results, never text |
| `internal/resolve` | session resolution (exact id, fuzzy name scoring), sender filter, self detection |
| `internal/contacts` | contact rows, display-name modes, group rosters and nicknames (ext_buffer), friend evidence |
| `internal/msgdb` | message table naming (`Msg_` + md5), schema probe, Name2Id, shard iteration, **the single raw-row reader** used by history, query and export |
| `internal/voice` | native voice payload sniffing and the decoder protocol |
| `internal/media` | media base discovery, export media index, per-id image cache, kvcomm media keys, V2 decrypt, sticker decrypt, ffmpeg conversion |
| `internal/capture` | the `keys` flow as an explicit state machine; `lldb` (macOS), kernel uprobe + ELF hook locator + process identity (Linux) |
| `internal/keys` | all_keys.json read/write (atomic, 0600), page-1 HMAC check, PBKDF2 derivation grouped by salt, key_material.bin |
| `internal/decode` | **pure**: message types, byte-level XML scanning, protobuf wire reader, packed_info, decompression, `Decode(Raw, Ctx)`, media classification |
| `internal/tgtime` | pure time parsing (`Parse(s, now, loc)`), buckets, local formatting |
| `internal/store` | catalog of source databases, key decoding, read-only SQLCipher open, per-command snapshot clones |
| `internal/sqlcipher` | cgo binding to the vendored SQLCipher amalgamation and the FTS tokenizer alias |
| `internal/env`, `internal/paths`, `internal/dict`, `internal/par`, `internal/logx`, `internal/strsearch` | process environment, path discovery, masked upstream names, ordered parallel map, stderr logging, substring search anchored on the needle's rarest byte (XML and CJK needles make `strings.Index` verify a false anchor at most positions) |

## Rules

- **No hidden globals.** `HOME`, the clock and the time zone are read once
  into `env.Env` in `cmd/tg` and passed down. Pure packages take `now` and
  `loc` as arguments. Masked names live only in `internal/dict`.
- **One reader.** Every command that reads messages goes through
  `msgdb` raw rows, then `contacts`/`resolve` for names, then `decode`.
  Sender inference (1:1 partner, the account owner shown as 我) is the same
  for messages, query, export and MCP.
- **Explicit states.** Multi-step flows (keys capture, snapshot clones,
  OAuth grants, export media outcomes, voice export, search strategy) use a
  named type for their states and one function per transition, not
  combinations of booleans and optional values.
- **Errors.** Return errors with context; `cmd/tg` prints `error: <msg>` and
  exits 1. Do not wrap a message in a second `Error:` prefix. Helpers report
  facts; callers choose fallbacks.
- **Text decoding is byte-compatible.** The XML and protobuf readers copy the
  Rust string-scanning behaviour; do not swap in `encoding/xml`, which
  changes output on malformed input.
- **Naming.** Public code uses `tg` / `telegram` / neutral words. Text tg
  writes at runtime (help, errors, warnings, doctor, export headers, MCP
  metadata, the installed skill) names the real app through `internal/dict`
  (`AppDisplayName` or `RenderAppNames`), never the cover name; tests turn it
  back into the cover name before comparing with checked-in files. Message
  content is never rewritten. The native
  voice codec is "native voice"; its four-letter name is assembled from
  pieces in `internal/dict`, never written literally.

## Linux key capture

`keys --method login` on Linux captures the account key material with a kernel
uprobe instead of attaching a debugger to the client. The ELF hook locator
finds the account-material function's virtual address; the program headers
convert it to a file offset, and a uprobe is registered against that offset in
the client executable (`internal/capture/uprobe_linux.go`).

The probe is defined through tracefs `uprobe_events` with memory-dereference
fetch args: the 32-byte buffer is read as four `x64` words from the argument-1
pointer register (`%si` / `%x1`) and the length from the argument-2 register
(`%dx` / `%x2`). The dereference runs in kernel context at the hit, so the
bytes are copied while they are still live; sampling only the pointer register
and reading `/proc/pid/mem` afterwards would race the client freeing the
buffer. Four fixed-width words parse out of `trace_pipe` more reliably than one
array fetch arg, whose text form varies between kernels.

This path needs no new runtime dependency — no bpftrace, no eBPF loader. It
requires root (CAP_SYS_ADMIN) and a mounted tracefs with `CONFIG_UPROBE_EVENTS`
(`/sys/kernel/tracing` or `/sys/kernel/debug/tracing`); a missing mount or
permission gives a diagnostic naming the mount command. Capture runs in a
dedicated trace instance scoped to the client process set (`set_event_pid` +
`event-fork`) and filtered to the material length, and the flow is an explicit
state machine (`captureState`) whose cleanup removes the event, the probe
definition and the instance on every exit — success, timeout, Ctrl-C or error —
so no uprobe is ever left registered. macOS is unchanged (`lldb_darwin.go`).

## Tests

- `e2e/`: black-box contracts against a built binary with encrypted fixture
  databases. This is the regression gate.
- Go unit tests only for pure functions with many edge cases: time parsing,
  decode (message types, XML, protobuf, decompression), media decrypt and key
  vectors, KDF vectors, OAuth redirect validation and PKCE, session scoring,
  query field/argument validation, escaping, ELF instruction decoding,
  uprobe file-offset conversion and trace-line parsing.
- No unit tests that rebuild fixture databases per package, assert call
  order, or compare a parallel path with a serial one.
- Build and test on the mini (`scripts/remote.sh make check`).

## Decided behaviour changes

Each was a Rust inconsistency or bug found while writing the contracts.

- `query --until` is exclusive (`create_time < until`), matching `messages`
  and MCP.
- `query` and `export` use the same sender inference as `messages`
  (1:1 partner, the owner as 我); export rows gain real senders.
- `query --format json` with no rows prints nothing on stdout; the
  "No rows returned." note goes to stderr.
- `export` skips (with a warning) encrypted V2 media it cannot decrypt
  instead of copying ciphertext.
- `all_keys.json` is written atomically (temp file + rename, 0600).
- The MCP HTTP endpoint rejects requests whose `Origin` header names an
  origin other than the configured public URL.
- MCP `list_members` honours the remote lookback policy.
- Errors print once as `error: <msg>` (Rust printed `error: Error: <msg>`).
- Names and ids in CLI text escape terminal controls (C0/C1, DEL) and bidi
  overrides/isolates as `\u{..}`; Rust printed them raw, so a contact name
  could reorder or recolor the terminal line. Emoji joiners and private-use
  glyphs print as they are.
- Argument validation (for example `search --limit 0`) happens before any
  database is opened.
- A chat without a contact row is shown by its username everywhere, never by
  its message table name.
- MCP `list_sessions` and `diagnose` in remote mode hide contact remarks
  when the policy says so, like the other tools.
- A message body is split into sender and text only at the client's group
  format `<id>:\n`. Rust also split at any `word:`, so a private message
  `https://...` showed sender "https" and text "//..."; measured on real
  stores, every genuine prefix carries the newline.
- System messages show the sender `系统` in `query` as they do in
  `messages` (Rust's `query` left it empty).
- `query --format json` writes keys in `--fields` order (Rust's map wrote
  them alphabetically).
- `export` sniffs plain cached images and saves them with their real
  extension instead of `.dat`.
- A message type whose low 32 bits are 2^31 or more reads as `未知(<n>)` with
  the same signed value in every command (Rust's `query` showed `未知(-1)`).
- Voice durations come from the body's millisecond length
  (`voicelength="<ms>"`, or legacy `<sender>:<ms>:<flag>`), rounded to the
  nearest second. Rust took the first digit run of the body, so current
  messages read as `1s` (the endflag attribute) and legacy ones showed
  milliseconds labelled as seconds.
