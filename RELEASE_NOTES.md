## Faster reads

tg reads the same local databases the same way and prints byte-identical
output, but does much less work to get there. No caches, plaintext copies or
derived indexes were added; every read is still read-only and decrypted in
place.

Measured on one local account (interleaved runs against 2.4.4, wall time):

| command | 2.4.4 | 2.5.0 | speedup |
|---|---|---|---|
| `query --contains` across all chats | 5.3 s | 2.3 s | 2.4x |
| `query --has file/voice/video` across all chats | 4.1 s | 1.8 s | 2.3x |
| `query --order oldest` | 590 ms | 300 ms | 1.9x |
| `messages --all-time` (large group) | 510 ms | 280 ms | 1.8x |
| `search` | 200 ms | 80 ms | 2.5x |
| `members` | 130 ms | 80 ms | 1.6x |
| `export` (group with 469 images) | 5.1 s | 3.3 s | 1.5x |
| `sessions` / `unread` | 50 ms | 30 ms | 1.7x |

Global `query` peak memory also drops from about 860 MiB to 60-150 MiB.

- `query` filters rows as it reads them, decodes message bodies only for rows
  it will print, and reads whole message tables in storage order instead of
  through the sort index when no limit can stop the scan early.
- Message decoding reuses one decompression context per thread, sizes its
  output up front, and parses XML in single passes.
- `sessions`, `unread` and `members --friends` keep only the contacts they
  display.
- `members` aggregates speaking activity in one table scan.
- `search` scans its full-text tables in parallel and loads contacts at the
  same time. It now honors `--jobs`; the default is one worker per table. It
  uses slightly more CPU in exchange for lower wall time. Each table is read
  from its own consistent snapshot.
- `export` converts packed images in batches of 16 per converter process,
  which uses more peak memory (about 220 MiB) but much less CPU. It reads cached
  media file sizes only when two candidates tie.
