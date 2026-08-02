# Direct Source Store Design

Status: approved for implementation. Independent CLI/interface, end-to-end,
and Rust/security/release reviewers returned `PASS` on 2026-08-02.

## Decision

Replace the persistent decrypted database cache with one small, deep module that
opens the desktop client's encrypted SQLite databases directly through bundled
SQLCipher.

The shipped implementation will have no plaintext database fallback, persistent
page cache, refresh step, connection pool, storage trait, or alternate adapter.
SQLite remains responsible for indexed access, page decryption, locking, and WAL
snapshots.

## Goals

- Read committed data from the encrypted main database and its live WAL.
- Preserve the result semantics of `sessions`, `unread`, `messages`, `search`,
  `query`, `export`, and `doctor`.
- Keep the existing FTS5 tokenizer compatibility layer and upstream indexes.
- Stop creating or reading `~/.tg/decrypted` and `*.tg-pages` files.
- Keep macOS arm64, Linux x86_64, and Linux arm64 release artifacts independent
  of external SQLite, SQLCipher, OpenSSL, and libcrypto installations.
- Fail closed instead of returning partial or known-stale results.

## Non-goals

- A public Rust library interface.
- A generic database repository, VFS, connection pool, or storage abstraction.
- A cross-database atomic snapshot. SQLite can provide a snapshot per database,
  but not one transaction spanning independent files.
- Filesystem-level immutability of the source directory. SQLite may coordinate
  readers through the source database's native `-shm` file.
- Automatic deletion of an existing `~/.tg/decrypted` directory.

## Alternatives considered

### High-level `History` facade

An interface such as `History::sessions`, `History::messages`, and
`History::export` would hide the most details from `main.rs`, but it would either
move the existing domain implementations into one god module or merely forward
to them. It is broader than the storage problem and fails the deletion test.

### Scope and callback runner

An interface based on `ReadScope`, `DatabaseRole`, and callbacks would prevent
callers from seeing database names, but those names and schemas are already
domain knowledge in `db.rs`, `query.rs`, and `export.rs`. Scope policy and query
parallelism would be duplicated in the storage layer.

### Custom decrypting VFS

A custom VFS could promise stricter control over filesystem writes, but it would
need to reproduce SQLite pager, WAL-index, locking, recovery, and SQLCipher HMAC
behavior. That unsafe implementation is unnecessary because SQLCipher already
implements the required mechanism.

## Module and interface

The seam lives in `src/source_store.rs`:

```rust
pub(crate) struct SourceStore {
    // source root, exact relative-path catalog, and zeroizing keys
}

impl SourceStore {
    pub(crate) fn discover(
        db_dir: Option<&Path>,
        keys_path: &Path,
    ) -> Result<Self, String>;

    pub(crate) fn databases(&self) -> impl Iterator<Item = &str>;

    pub(crate) fn open(&self, relative_path: &str)
        -> Result<rusqlite::Connection, String>;
}
```

This is the whole external interface. There is no trait because there is one
implementation. There is no connection wrapper because SQLite open flags and
`query_only` enforce the safety property, while a wrapper would only forward the
large `rusqlite::Connection` interface.

### Interface contract

`discover`:

- uses `db_dir` when supplied, otherwise performs the current local source
  auto-detection shared with `tg keys`;
- enumerates `.db` files once and freezes that catalog for the command;
- stores canonical relative names in lexical order;
- reads `all_keys.json` once, requires a regular key file with no group/other
  permission bits, accepts only exact relative-path key matches, and rejects
  malformed 32-byte keys;
- keeps decoded keys in zeroizing fixed-size buffers and never includes them in
  logs or errors;
- does not open every database, derive keys, run PBKDF2, or write any file.

`databases` exposes only catalogued relative names such as
`message/message_0.db`. It never exposes an absolute source path or key. Callers
retain their existing schema-aware selection rules. A caller that needs to use a
name across scoped worker threads copies that short relative name.

`open` accepts a name only when it exists in the frozen catalog. It returns a
new, independently keyed read-only connection whose snapshot is pinned before
returning. The snapshot lasts until that active read transaction ends. Current
reader modules must not issue `BEGIN`, `COMMIT`, `END`, or `ROLLBACK`; the
migration review and a source check enforce this contract. The returned
connection is never shared concurrently; the existing scoped workers open one
connection per database.

The module returns contextual `String` errors, matching the rest of this
crate. No caller needs to branch on storage error variants after refresh and
fallback behavior is removed. Errors identify only the relative database and
operation; they never contain key material.

## Connection sequence

`SourceStore::open` performs this sequence in one narrow implementation:

1. Look up the exact catalog entry; arbitrary paths and `..` are not joined.
2. Read the 16-byte page salt from that source database rather than trusting a
   duplicated JSON value.
3. Open the real path with `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX`.
   Do not use a URI and never set `immutable=1`.
4. Build a zeroizing 99-byte SQLCipher keyspec:
   `x'` + 64 key hex bytes + 32 salt hex bytes + `'`.
5. Call the bundled `rusqlite::ffi::sqlite3_key` in the module's only new
   `unsafe` block. Passing the raw 48 bytes directly is forbidden because
   SQLCipher would interpret them as a passphrase and run PBKDF2 again.
6. Set `cipher_compatibility = 4`, `query_only = ON`, and
   `temp_store = MEMORY` before reading database pages.
7. Verify `Connection::is_readonly(MAIN_DB)`.
8. Execute `BEGIN` and read `sqlite_schema`. This both validates the key/cipher
   format and pins that database's SQLite snapshot for the active transaction.
9. Return the connection with that read transaction active. Existing reader
   modules may prepare and run queries but must not execute transaction-control
   SQL. Dropping the connection ends the transaction and erases the Rust-owned
   key buffers on their normal lifetimes.

The `unsafe` block must have a local `SAFETY` proof covering the live SQLite
handle, keyspec pointer and length, return-code handling, and buffer lifetime.
The implementation uses the generated `ffi::sqlite3_key` declaration and does
not redeclare the C symbol.

Key-bearing JSON input, decoded keys, temporary JSON strings, and keyspecs are
zeroized on their normal Rust lifetimes. Internal key types do not implement
`Debug`, `Display`, or `Serialize`. This is best-effort process-memory hygiene;
it does not claim to erase allocator copies, core dumps, or SQLCipher's internal
key schedule before the connection closes.

Do not add retries or a busy timeout without a reproduced failure. A SQLite
busy, recovery, I/O, missing-key, or invalid-key/cipher error is returned with
an actionable instruction to run `tg keys` where appropriate.

## SQLCipher dependency

Implementation changes the already-upgraded dependency to:

```toml
rusqlite = {
    version = "0.40.1",
    default-features = false,
    features = ["bundled-sqlcipher-vendored-openssl"],
}
```

`libsqlite3-sys 0.38.1` bundles SQLCipher 4.14.0 and enables FTS5. Vendored
OpenSSL gives all three release targets one SQLite/SQLCipher/crypto provider and
prevents Linux artifacts from depending on the runner's `libcrypto.so` ABI.
The accepted costs are a slower build and a larger single executable.

This does not make Linux artifacts fully static: they retain the platform libc
dependency. Linux releases target glibc 2.35 or newer, are built and smoked in
native-architecture Ubuntu 22.04 containers, and must not import a newer GLIBC
symbol. They are also smoked on the current Ubuntu runner. The support claim is
explicitly “no external sqlite/sqlcipher/crypto/ssl dependency,” not “runs on
every Linux distribution.”

The release archive must include the project license plus SQLCipher and OpenSSL
notices. It must not claim FIPS compliance.

## WAL and filesystem contract

Normal SQLite read-only mode is mandatory because `immutable=1` can ignore the
live WAL and return stale data while the desktop client is running.

Each connection sees a consistent committed snapshot of its database, including
committed WAL frames. Separate contact, session, FTS, media, and message-shard
connections may observe slightly different instants; the current independent
database layout cannot provide a global atomic snapshot.

The direct reader issues no SQL writes to the main database or WAL. SQLite may
read, lock, create, or update the native `-shm` WAL-index as required for correct
coordination. Therefore the promise is:

- no tg-owned plaintext database or persistent page cache;
- no writes to source main database or WAL content;
- normal SQLite SHM coordination is allowed;
- if a live WAL snapshot cannot be established, fail instead of reading only
  the base database.

Strict zero-byte changes in the source directory would require a snapshot or
custom VFS and conflicts with the no-copy, no-persistent-cache goal. It is not
part of this design.

## Ownership of behavior

`SourceStore` owns only source discovery, cataloguing, key handling, SQLCipher
initialization, read-only enforcement, and per-database snapshots.

Existing modules continue to own:

- `db.rs`: session resolution, session/message reads, FTS table queries, global
  merge and display behavior;
- `query.rs`: parameterized filters, limits, post-filtering, fields, sorting and
  output formats;
- `export.rs`: archive formats, media lookup, decoding and requested output;
- `fts.rs`: compatible tokenizer registration and ideograph behavior;
- `parallel.rs`: bounded worker scheduling;
- `database_keys.rs` and `scanner.rs`: account-material capture, PBKDF2 key
  derivation, verification, and the small `all_keys.json` file.

Two low-level mechanisms are deliberately shared instead of duplicated:

- `paths.rs` finds `db_storage` below an explicit home directory; normal reads
  pass `$HOME`, while the privileged key-capture path passes the invoking user's
  resolved home;
- `database_keys.rs` owns the serialized key types, source `.db` file record,
  key-file loading/coverage inspection, deterministic file collection, page-1
  reading, and first-page key verification. `scanner`, `doctor`, and
  `SourceStore::discover` reuse those functions.

`SourceStore` still owns the frozen per-command catalog and all keyed connection
behavior. These shared helpers are mechanisms needed by the independently
existing `keys` command, not a second storage adapter.

FTS tokenizer registration remains in the search implementation. The storage
module must not infer query policy from a filename.

`doctor` is intentionally not an ordinary `SourceStore` caller when setup is
broken. It reports each prerequisite independently through the shared
`paths.rs` source lookup and `database_keys.rs` catalog/key-coverage helpers,
then exercises `SourceStore::discover/open` only when those prerequisites are
available. This preserves multi-fault diagnostics without adding an inspection
interface or parsing `SourceStore` error strings.

## Completeness and optional data

- `sessions` and `unread` require `session/session.db`; contact data is optional
  and missing contacts continue to render raw identifiers.
- `messages` and `query` require every catalogued numbered message shard to open
  and query successfully. A present shard failure aborts the command instead of
  returning partial results.
- `search` requires `message/message_fts.db`; it does not fall back to an
  unbounded shard scan. Contact data remains optional.
- `export` requires the same complete message set. An absent optional media or
  voice database is treated as uncached media; a present database that cannot
  be opened or queried aborts the export.
- A schema legitimately lacking the target session table is skipped as today.
  Row, statement, key, cipher, and I/O errors are not skipped.

“Optional” means the database path is absent from the frozen source catalog. If
an optional path is present but its key, open, or query fails, the command fails;
it does not silently discard a damaged or unreadable data source.

Commands do not automatically invoke debugger capture or key derivation. A new
database path without an exact key fails with `run tg keys`; cached account
material makes that explicit command cheap and updates only the small key file.
When source/key overrides are active, the error prints the corresponding
`tg keys --db-dir <path> --output <path>` command rather than the defaults.

## CLI and deletion plan

The direct-reader release is a breaking storage cleanup:

- delete `decrypt` and `refresh` commands;
- delete `--decrypted-dir` and `paths::default_decrypted_dir`;
- add `--db-dir` and `--keys` source overrides to data-reading commands while
  preserving automatic source discovery and the default key path;
- add matching `--db-dir` and `--output` options to `tg keys`, so every custom
  reader configuration has a complete key-generation and repair path;
- retain `--jobs` as query-layer parallelism;
- retain `keys`, including macOS LLDB and Linux GDB login capture;
- rewrite `doctor` around source discovery, key coverage, SQLCipher, FTS, and
  WAL readability.

After all commands migrate and pass differential tests, delete:

- `src/cache.rs`;
- all of `src/decrypt.rs`, after moving its small source-file/page-1/key
  verification mechanisms into `database_keys.rs`;
- `src/sqlite.rs::open_immutable`;
- `find_decrypted_*`, refresh retry/warning branches, and decrypted-cache tests;
- stale documentation for `.tg_index.db`, refresh/decrypt scopes, and retired
  media commands.

Do not keep the old read path behind a feature flag or compatibility fallback.

The program does not recursively delete an existing user directory. Once the
new release is verified, release notes give one explicit command to inspect and
remove the exact legacy default directory. Custom historical output directories
are never guessed or deleted.

## Implementation sequence and gates

1. Enable bundled SQLCipher and add a generated temporary encrypted fixture.
   Prove correct key, wrong key, plaintext SQLite, and read-only enforcement.
2. Prove an uncheckpointed WAL-only sentinel is visible, a long-lived reader
   keeps its old snapshot, and a new reader sees the next commit.
3. Prove encrypted FTS5 plus the existing compatible tokenizer performs the
   current ideograph search.
4. Implement only the three-method `SourceStore` interface.
5. Migrate one vertical slice at a time: sessions/unread, messages/query,
   search, export, then doctor. Compare normalized old/new output against a
   fixed real source snapshot after every slice.
6. Run the complete local and three-platform verification matrix, including the
   Linux glibc floor container and transaction-control source gate.
7. Delete the plaintext path and update README, completions, data-model,
   release packaging, and Homebrew documentation in the same change.

The old decrypted reader may exist temporarily on the implementation branch as
a differential oracle. It must not be abstracted, selected at runtime, or ship
in the final binary.

If real source, live WAL, or tokenizer compatibility fails, stop and report the
failed gate. Do not write a custom VFS or restore a plaintext fallback without a
new approved design.

## Verification matrix

| Area | Required signal |
| --- | --- |
| Key | correct, missing, malformed, stale/wrong; no secret in errors/logs |
| Read-only | write SQL rejected; main DB and WAL size/content/mtime unchanged |
| Snapshot | connection returns with an active read transaction; reader modules contain no transaction-control SQL; lifetime behavior is tested |
| WAL | WAL-only sentinel visible; snapshot lifetime behavior verified |
| SHM | with missing/unwritable SHM, either the WAL sentinel is read correctly or the open fails explicitly; stale base-only success is forbidden |
| FTS | bundled FTS5 compile option, tokenizer registration, ideograph search |
| Completeness | one unreadable required shard fails; an absent optional path degrades, while a present optional path with key/open/query failure fails |
| Differential | normalized sessions/unread/messages/search/query result rows and export artifacts match |
| Doctor | process/key/source/session-probe conclusions match; direct-source, cipher, FTS, and WAL conclusions have dedicated assertions |
| Concurrency | desktop writer plus 1 and 8 readers; no corruption or partial success |
| Disk | no `~/.tg/decrypted`, `*.tg-pages`, or new tg-owned DB after all reads |
| macOS artifact | `otool -L` has no sqlite/sqlcipher/crypto/ssl dylib |
| Linux artifacts | native x86_64/arm64 Ubuntu 22.04 build and smoke; no GLIBC symbol newer than 2.35; `ldd`/`readelf` has no sqlite/sqlcipher/crypto/ssl dependency |
| Provider | one `libsqlite3-sys`; SQLCipher version nonempty; FTS5 enabled |
| Release | archive contains project, SQLCipher, and OpenSSL license notices |

All current unit tests, rustfmt, Clippy with `-D warnings`, release builds, CLI
help, and Homebrew smoke tests remain mandatory.

## Performance and disk acceptance

Use median and p95 over at least five runs on the current real dataset.
Post-implementation measurement showed that ratios against the removed warm
plaintext cache are not a valid release gate: they penalize the intended
SQLCipher page authentication and can only be recovered by recreating the disk
cache this design forbids. Keep those ratios as a recorded diagnostic. The
release gates are:

- no command has a fixed 3–4 second refresh phase;
- `sessions`, `messages`, `search`, `query`, and a small real export have p50
  below one second and five-run nearest-rank p95 below two seconds on the
  current real dataset;
- record the warm plaintext comparison without hiding or normalizing the
  SQLCipher cost;
- record peak RSS, connection count, bytes read, executable size, and archive
  size rather than hiding SQLCipher cost behind a new cache;
- a read-only command creates no tg-owned persistent file;
- after explicit legacy cleanup, normal tg state is limited to
  `all_keys.json`, `key_material.bin`, and user-requested exports.

If a performance gate fails, profile page reads, query plans, and repeated
connections first. A persistent plaintext or page cache is not an allowed fix.

The implementation measurements and reproducible checks are recorded in
[`direct-source-store-verification.md`](direct-source-store-verification.md).

## Evidence

- SQLCipher raw key and salt syntax:
  <https://www.zetetic.net/sqlcipher/sqlcipher-api/>
- SQLCipher on-demand pager encryption:
  <https://www.zetetic.net/sqlcipher/about/>
- SQLite read-only WAL requirements:
  <https://www.sqlite.org/wal.html>
- SQLite WAL-index and SHM behavior:
  <https://sqlite.org/walformat.html>
- rusqlite feature and build behavior:
  <https://docs.rs/crate/rusqlite/latest/features>
- SQLCipher Community license:
  <https://www.zetetic.net/sqlcipher/license/>
