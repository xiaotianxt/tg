## Faster reads, round two

tg still reads the same local databases read-only and prints byte-identical
output for the same data. No caches, plaintext copies or derived indexes were
added. Most of the gain comes from doing independent work at the same time,
so some commands use a little more CPU and memory in exchange for lower wall
time.

Measured on an 8-core Linux machine against a synthetic encrypted workset of
about 2.2 million messages (interleaved runs, median wall time):

| command | 2.5.0 | 2.6.0 | speedup |
|---|---|---|---|
| `query --contains` across all chats | 4.8 s | 2.8 s | 1.7x |
| `query --has` media across all chats | 3.6 s | 2.2 s | 1.7x |
| `query --format json` across all chats | 5.1 s | 3.1 s | 1.7x |
| `query --order oldest` | 1.3 s | 0.9 s | 1.4x |
| `messages` (session id) | 400 ms | 240 ms | 1.6x |
| `messages` (display name) | 520-600 ms | 320 ms | 1.6-1.9x |
| `messages --all-time` (large group) | 1.5 s | 1.2 s | 1.2x |
| `members` | 420 ms | 210 ms | 2.0x |
| `export` (group with media) | 1.06 s | 0.82 s | 1.3x |
| `doctor` (session id) | 430 ms | 220 ms | 1.9x |

`sessions`, `unread` and `search` with ASCII queries are unchanged.

- `query` splits large message tables into row ranges and reads them on up
  to four threads (`--jobs` overrides). Global queries use 5-10% more CPU and
  more peak memory.
- SQLite allocation statistics are turned off, so parallel readers no longer
  wait on one process-wide lock.
- `messages`, `members` and `doctor` given a session id load contacts while
  the message databases are read.
- Display-name lookups score each contact once, with a bounded edit distance,
  in parallel chunks.
- `export` indexes the media cache while messages are read, plans media jobs
  while the text files are written, lists cache directories without a stat
  per candidate, and skips a redundant probe when converting batched HEVC
  images.

## Fixes

- `export` picks the media key that actually decrypts this chat's cached
  images and videos when the client keeps keys for several codes. Before, it
  took whichever key file the directory listed first and could write media as
  undecrypted `.bin` files.
- Forwarded chat records nested inside other forwarded records now expand as
  indented inner records instead of being flattened into the parent (#13,
  thanks @thezzisu).
