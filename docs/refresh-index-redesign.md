# Refresh and Index Redesign

This document records the technical validation and target design for removing
`tg refresh` as a foreground bottleneck. The goal is not to make indexing free.
The goal is to stop treating a large derived index as a correctness prerequisite
for normal commands.

## Verified Facts

- `.tg_index.db` is an accelerator. Current user-facing commands can fall back to
  decrypted source databases for correctness.
- `tg refresh` currently couples full decrypt refresh with local message index
  maintenance. A slow index phase appears to users as a slow refresh.
- Full decrypt refresh can be sub-second when most decrypted files are reused.
  The slow path is dominated by local index maintenance.
- A recent local sample had about 1.3M rows in `.tg_index.db`. Old schema stored
  both raw and legacy body columns, duplicating about 1.9GB of text.
- Contact database mtime is not a stable invalidation signal. Decrypt can rewrite
  the output mtime even when the display-relevant contact content is unchanged.
- The upstream FTS database is usable as a candidate text index, but not as a
  complete post-processed search replacement. FTS hits must be verified against
  the physical message table before using packed metadata or exporting.
- The session database is a cheap change detector. It can identify active or
  changed sessions in milliseconds, but it is not proof that every latest message
  row is already present in numbered message shards.
- Resource and voice databases already contain structured media rows. They should
  be used for media discovery where possible instead of deriving everything from
  a global decoded-message index.

## First Principles

The source databases are the canonical state. tg-owned indexes and caches are
derived state.

Derived state must not be required for correctness:

- If a cache is missing, stale, locked, or incompatible, commands should use the
  canonical source path or a bounded candidate scan.
- A cache can make a command faster, but it must not be the only way to answer.
- Cache invalidation should affect performance and warnings, not whether normal
  commands can run.

The hard problem is maintaining a post-processed materialized view without an
upstream change log. File-level mtime tells us that a shard changed, not which
message rows changed. Correctness must come from row-level verification or direct
source reads, not from trusting a shard-level cache blindly.

## Target Architecture

1. `refresh` updates decrypted source snapshots only.
   - Default scope should be the minimal scope needed by the command.
   - Full maintenance remains explicit.

2. Query and export commands choose a backend with an explicit plan.
   - Prefer an answerable local accelerator when its capability and coverage are
     sufficient.
   - Prefer upstream FTS for text candidate discovery.
   - Fall back to direct shard scan when accelerator state is missing or stale.

3. Post-processed search uses candidate verification.
   - Use upstream FTS, local media/resource tables, or typed filters to narrow
     candidates.
   - Verify candidates by reading physical message rows and decoding them.
   - Cache decoded rows by row identity and decode fingerprint.

4. Local caches are row-oriented.
   - Message rows: `(source_db, table_name, local_id)` when available.
   - Voice rows: `(chat_id, create_time, local_id, data_index)`.
   - Resource rows: resource primary key plus message-local join fields.
   - Fallback keys must include source and table labels to avoid cross-table
     collisions.

5. Display names are late-bound.
   - Contact changes should not force rewriting the raw row cache.
   - Any cache that embeds display names must include an explicit contact
     fingerprint and be treated as derived presentation data.

## Small Modules

### `row_identity`

Owns source fingerprints and contact fingerprints.

Invariants:

- Source fingerprints are `(mtime_ns, size)`, not absolute paths.
- Contact fingerprints hash display-relevant fields with length delimiters.
- Missing file metadata causes the caller to skip or conservatively refresh.

Current validation:

- Order-independent contact fingerprint test.
- Display-field change test.
- Source shrink detection test.

### `index_policy`

Owns pure refresh policy decisions.

Invariants:

- Unchanged source fingerprint produces no action.
- Missing prior state uses full-window refresh.
- Source shrink uses full-window refresh.
- Source growth or mtime-only change can use a local-id cursor mode when the
  caller accepts append-only cache semantics.
- Overlap windows never go before the configured minimum coverage.

Current validation:

- Unit tests cover unchanged, missing, shrinking, growing, mtime-only changed,
  and overlap-boundary behavior.

### `message_index` Adapter

Remains responsible for SQLite schema and mutation mechanics only.

Near-term adapter work:

- Keep schema migration and WAL checkpointing bounded.
- Store table cursor state separately from message rows.
- Treat local-id cursor refresh as an accelerator maintenance path, not a proof
  that every in-place source update was observed.

Current validation:

- Existing index build tests.
- Contact mtime changes no longer invalidate the index.
- Local-id cursor append path indexes new rows without duplicating old rows.

### `query_plan`

Planned module. It should choose `HotIndex`, upstream FTS candidate verification,
direct shard scan, or refusal for unbounded requests.

Required invariants:

- Planner does not execute SQL.
- Fallback reason is explicit.
- Anonymous/display-sensitive queries do not accidentally rely on stale
  presentation caches.
- No usable index is a fallback condition, not a fatal error.

### `decoded_body_cache`

Planned module. It should wrap decoding and cache keys.

Required invariants:

- Cache keys include raw body, type, marker, packed metadata, local id when it
  affects output, decoder version, and any display fingerprint when display text
  is embedded.
- Decode failures are row-local best-effort failures, not query-wide failures.

## Rollout Plan

1. Keep `.tg_index.db` optional.
2. Decouple `tg refresh` from automatic index maintenance.
3. Add `query_plan` so fallback behavior is reviewable and testable.
4. Route plain text search through upstream FTS candidates plus physical row
   verification.
5. Route media list/export through resource, voice, and filesystem indexes where
   those are authoritative enough.
6. Add decoded-row cache only for post-processed filters that cannot be answered
   by upstream indexes.
7. Add phase timing so future regressions show whether time was spent in decrypt,
   source probing, candidate search, decoding, SQLite writes, or checkpointing.

## Non-Goals

- Eliminating the cost of the first full historical scan.
- Claiming append-only cursor maintenance is a complete correctness model.
- Making a tg-owned global decoded index mandatory for normal commands.
