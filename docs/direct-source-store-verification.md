# Direct source store verification

Measured on 2026-08-02 against the current local dataset. Timings are five
release-build runs; p95 is the nearest-rank maximum of five. The first run was
kept rather than discarded.

| Command | Direct p50 | Direct p95 | Warm plaintext p50 |
| --- | ---: | ---: | ---: |
| `sessions --top 5` | 0.04 s | 0.13 s | 0.02 s |
| `messages CHAT --since 1h --limit 20` | 0.50 s | 1.46 s | 0.29 s |
| `search TERM --since 2d --limit 20` | 0.15 s | 0.71 s | 0.08 s |
| `query --contains TERM --since 2d --limit 20 --format json` | 0.83 s | 1.65 s | 0.22 s |
| 10-message chat export with media | 0.99 s | 1.90 s | 0.42 s |

The former refresh-plus-command path was approximately 3–4 seconds when it
needed to catch up. The direct path has no fixed refresh phase. Warm plaintext
is faster for page-heavy scans, as expected; tg does not recreate that cache.

The query run used 65,323,008 bytes maximum RSS, 43,123,968 bytes peak memory
footprint, and zero physical block-input operations with the OS page cache warm.
Automatic source reads are capped at two worker connections; commands retain at
most one contact connection, so the observed peak is three SQLite connections.

The release binary was 8,470,576 bytes and the compressed macOS archive stream,
including license notices, was 4,014,080 bytes.

## Correctness and disk signals

- Normalized old/new hashes matched for `sessions`, `unread`, bounded and
  all-time `messages`, `search`, and `query`.
- A real 10-message export matched the old binary for all six files by SHA-256;
  the temporary export directory was removed.
- Eight concurrent readers succeeded while the desktop client writer remained
  active.
- Source main DB and WAL content hashes, sizes, and mtimes were unchanged after
  all read commands. `~/.tg` file inventory, sizes, and mtimes were unchanged.
- Unit fixtures cover correct, wrong and missing keys; plaintext rejection;
  read-only writes; live WAL visibility; stable snapshots; missing/unwritable
  SHM; eight readers; encrypted FTS5 with the compatible ideograph tokenizer;
  and fail-closed malformed rows.
- `otool -L` reports no sqlite, SQLCipher, crypto, or ssl dynamic library.
- The release workflow builds both Linux architectures in Ubuntu 22.04,
  rejects GLIBC symbols newer than 2.35, rejects database/crypto dynamic
  dependencies, and archives the project and third-party license notices.
