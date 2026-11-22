# Direct Source Store Design

Status: implemented. Independent CLI/interface, end-to-end, and
security/release reviewers returned `PASS` on 2026-08-02 for the Rust 2.x
implementation; tg 3.0 carries the same contract to Go (`internal/store` and
`internal/sqlcipher`, see [`go-architecture.md`](go-architecture.md)). Module
names below are the Go packages.

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
- Preserve the result semantics of `sessions`, `unread`, `members`, `messages`,
  `search`, `query`, `export`, and `doctor`.
- Keep the existing FTS5 tokenizer compatibility layer and upstream indexes.
- Stop creating or reading `~/.tg/decrypted` and `*.tg-pages` files.
- Keep macOS arm64, Linux x86_64, and Linux arm64 release artifacts independent
  of external SQLite, SQLCipher, OpenSSL, and libcrypto installations.
- Fail closed instead of returning partial or known-stale results.

## Non-goals

- A public Go library interface.
- A generic database repository, VFS, connection pool, or storage abstraction.
- A cross-database atomic snapshot. SQLite can provide a snapshot per database,
  but not one transaction spanning independent files.
- Filesystem-level immutability of the source directory. SQLite may coordinate
  readers through the source database's native `-shm` file.
- Automatic deletion of an existing `~/.tg/decrypted` directory.

## Alternatives considered

### High-level `History` facade

An interface such as `History.Sessions`, `History.Messages`, and
`History.Export` would hide the most details from `cmd/tg`, but it would either
move the existing domain implementations into one god module or merely forward
to them. It is broader than the storage problem and fails the deletion test.

### Scope and callback runner

An interface based on `ReadScope`, `DatabaseRole`, and callbacks would prevent
callers from seeing database names, but those names and schemas are already
domain knowledge in `history`, `query`, and `export`. Scope policy and query
parallelism would be duplicated in the storage layer.

### Custom decrypting VFS

A custom VFS could promise stricter control over filesystem writes, but it would
need to reproduce SQLite pager, WAL-index, locking, recovery, and SQLCipher HMAC
behavior. That implementation is unnecessary because SQLCipher already
implements the required mechanism.

## Module and interface

The seam lives in `internal/store`:

```go
type Store struct {
    // source root, exact relative-path catalog, and decoded keys
}

func Discover(dbDir, keysPath string) (*Store, error)

func (s *Store) Databases() []string
func (s *Store) Open(rel string) (*sqlcipher.Conn, error)
func (s *Store) OpenOptional(rel string) (*sqlcipher.Conn, error)
func (s *Store) Close() error // removes this command's snapshot clones
```

There is no interface type because there is one implementation.
`internal/sqlcipher.Conn` is the minimal cgo binding (prepare, bind, step,
column reads); SQLite open flags and `query_only` enforce the safety property.

### Interface contract

`Discover`:

- uses `db_dir` when supplied, otherwise performs the current local source
  auto-detection shared with `tg keys`;
- enumerates `.db` files once and freezes that catalog for the command;
- stores canonical relative names in lexical order;
- reads `all_keys.json` once, requires a regular key file with no group/other
  permission bits, accepts only exact relative-path key matches, and rejects
  malformed 32-byte keys;
- keeps decoded keys in fixed-size arrays and never includes them in logs or
  errors;
- does not open every database, derive keys, run PBKDF2, or write any file.

`Databases` exposes only catalogued relative names such as
`message/message_0.db`. It never exposes an absolute source path or key. Callers
retain their existing schema-aware selection rules. A caller that needs to use a
name across scoped worker threads copies that short relative name.

`Open` accepts a name only when it exists in the frozen catalog. It returns a
new, independently keyed read-only connection whose snapshot is pinned before
returning. The snapshot lasts until that active read transaction ends. Current
reader modules must not issue `BEGIN`, `COMMIT`, `END`, or `ROLLBACK`; the
migration review and a source check enforce this contract. The returned
connection is never shared concurrently; the existing scoped workers open one
connection per database. Search is the one reader that opens several
connections to the same database: it scans each FTS content table on its own
connection, so each table is read from one snapshot but different tables may
come from slightly different instants, as with message shards.

The package returns contextual errors. No caller needs to branch on storage error variants after refresh and
fallback behavior is removed. Errors identify only the relative database and
operation; they never contain key material.

## Connection sequence

`Store.Open` performs this sequence in one narrow implementation:

1. Look up the exact catalog entry; arbitrary paths and `..` are not joined.
2. Read the 16-byte page salt from that source database rather than trusting a
   duplicated JSON value.
3. Open the real path with `SQLITE_OPEN_READONLY | SQLITE_OPEN_NOMUTEX`.
   Do not pass a URI and never set `immutable=1`.
4. Build a 99-byte SQLCipher keyspec:
   `x'` + 64 key hex bytes + 32 salt hex bytes + `'`.
5. Call the vendored `sqlite3_key` through cgo, then clear the keyspec
   buffer. Passing the raw 48 bytes directly is forbidden because SQLCipher
   would interpret them as a passphrase and run PBKDF2 again.
6. Set `cipher_compatibility = 4`, `query_only = ON`, and
   `temp_store = MEMORY` before reading database pages.
7. Execute `BEGIN` and read `sqlite_schema`. This both validates the key/cipher
   format and pins that database's SQLite snapshot for the active transaction.
   On macOS the first open of a database per command runs steps 3–7 on the
   live file, clones it while pinned, and then runs them on the clone (see the
   WAL contract below).
8. Return the connection with that read transaction active. Reader packages
   may prepare and run queries but must not execute transaction-control SQL.
   Closing the connection ends the transaction.

Read-only mode comes from the open flag and `query_only`; a test proves write
SQL is rejected. Keyspecs and derived keys are cleared after use. This is
best-effort process-memory hygiene: Go's garbage collector may leave copies,
and it does not claim to erase core dumps or SQLCipher's internal key schedule
before the connection closes.

Do not add retries or a busy timeout without a reproduced failure. A SQLite
busy, recovery, I/O, missing-key, or invalid-key/cipher error is returned with
an actionable instruction to run `tg keys` where appropriate.

## SQLCipher dependency

`internal/sqlcipher` compiles the vendored SQLCipher 4.14.0 amalgamation
(SQLite 3.51.3) with cgo, with FTS5 enabled and extension loading omitted. The
crypto provider is the system CommonCrypto on macOS and OpenSSL's libcrypto,
linked statically, on Linux, so no artifact depends on an external
sqlite/sqlcipher/crypto/ssl shared library or on the runner's `libcrypto.so`
ABI.

This does not make Linux artifacts fully static: they retain the platform libc
dependency. Linux releases target glibc 2.35 or newer, are built and smoked in
native-architecture Ubuntu 22.04 containers, and must not import a newer GLIBC
symbol. They are also smoked on the current Ubuntu runner. The support claim is
explicitly “no external sqlite/sqlcipher/crypto/ssl dependency,” not “runs on
every Linux distribution.”

The release archive must include the project license plus SQLCipher, OpenSSL,
Go, and Go module notices. It must not claim FIPS compliance.

## WAL and filesystem contract

Normal SQLite read-only mode is mandatory because `immutable=1` can ignore the
live WAL and return stale data while the desktop client is running.

Each connection sees a consistent committed snapshot of its database, including
committed WAL frames. Separate contact, session, FTS, media, and message-shard
connections may observe slightly different instants; the current independent
database layout cannot provide a global atomic snapshot.

A read transaction on the live file keeps the desktop client's WAL checkpoints
from completing. Back-to-back bulk queries held one on `message_0.db` almost
continuously for minutes, and the client dropped every message that arrived in
that window. So on macOS the first `open` of a database in a command pins a
live snapshot, clones the main file and then its `-wal` with APFS `clonefile`
into a per-command temporary directory, and releases the live read; that and
every later connection to the database in the command read the clone. The pin
keeps checkpoints from backfilling past the snapshot or restarting the WAL, so
the two files, cloned at two instants, replay to one committed state. A clone
is copy-on-write and stays encrypted; the directory is removed when the
command's `Store` closes. Where `clonefile` is unsupported (Linux, a
non-APFS volume, or a source on another volume than the temporary directory)
reads stay on the live file: a full copy would hold the live read for the
copy's duration and write the whole database on every command.

The direct reader issues no SQL writes to the main database or WAL. SQLite may
read, lock, create, or update the native `-shm` WAL-index as required for correct
coordination. Therefore the promise is:

- no tg-owned plaintext database or persistent page cache; the only copies are
  the encrypted per-command clones above;
- no writes to source main database or WAL content;
- normal SQLite SHM coordination is allowed;
- if a live WAL snapshot cannot be established, fail instead of reading only
  the base database.

Strict zero-byte changes in the source directory would require a custom VFS:
the pin that precedes each clone still coordinates through the live `-shm`. It
is not part of this design.

## Ownership of behavior

`internal/store` owns only source discovery, cataloguing, key decoding,
SQLCipher initialization, read-only enforcement, and per-database snapshots.

Other packages continue to own:

- `history`, `resolve`, `contacts`, `msgdb`: session resolution,
  session/message reads, current group-member activity, FTS table queries,
  global merge;
- `query`: parameterized filters, limits, post-filtering, fields, sorting and
  output formats;
- `export`: archive formats, media lookup, decoding and requested output;
- `sqlcipher`: compatible tokenizer registration;
- `par`: bounded worker scheduling;
- `keys` and `capture`: account-material capture, PBKDF2 key derivation,
  verification, and the small `all_keys.json` file.

Two low-level mechanisms are deliberately shared instead of duplicated:

- `paths` finds `db_storage` below an explicit home directory; normal reads
  pass `$HOME`, while the privileged key-capture path passes the invoking user's
  resolved home;
- `keys` owns the key-file format, source `.db` file records, key-file
  loading/coverage inspection, deterministic file collection, page-1 reading,
  and first-page key verification. `capture`, `doctor`, and `store.Discover`
  reuse those functions.

`store` still owns the frozen per-command catalog and all keyed connection
behavior. These shared helpers are mechanisms needed by the independently
existing `keys` command, not a second storage adapter.

FTS tokenizer registration remains in the search implementation. The storage
module must not infer query policy from a filename.

`doctor` is intentionally not an ordinary `store` caller when setup is
broken. It reports each prerequisite independently through the shared `paths`
source lookup and `keys` catalog/key-coverage helpers, then exercises
`store.Discover`/`Open` only when those prerequisites are available. This
preserves multi-fault diagnostics without adding an inspection interface or
parsing `store` error strings.

## Completeness and optional data

- `sessions` and `unread` require `session/session.db`; contact data is optional
  and missing contacts continue to render raw identifiers.
- `messages`, `export` and `query` require every catalogued message shard they
  read to open and query successfully. A present shard failure aborts the
  command instead of returning partial results. Reading one session opens the
  numbered chat shards and the official-account shards
  (`message/biz_message_N.db`), since its table lives in one of them; an
  unscoped `query` reads chat shards only unless `--official` adds the
  official-account shards.
- `members` requires a complete current-member relation from
  `contact/contact.db` and every numbered message shard. It fails instead of
  presenting historical senders as a complete current roster.
- `members --friends` uses only contact metadata and optional session-based name
  resolution, never message shards. It intersects the cached roster with explicit
  local relationship evidence, conservatively excluding single-bit, deleted,
  verified/service, and stranger records. Missing relationship columns fail
  explicitly; names and previous chats are never treated as friend evidence.
  Results are neither exhaustive nor a live reciprocal relationship check.
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
- delete `--decrypted-dir` and the default decrypted directory;
- add `--db-dir` and `--keys` source overrides to data-reading commands while
  preserving automatic source discovery and the default key path;
- add matching `--db-dir` and `--output` options to `tg keys`, so every custom
  reader configuration has a complete key-generation and repair path;
- retain `--jobs` as query-layer parallelism;
- retain `keys`, including macOS LLDB and Linux GDB login capture;
- rewrite `doctor` around source discovery, key coverage, SQLCipher, FTS, and
  WAL readability.

After all commands migrated and passed differential tests, the 2.x release
deleted the decrypted cache, the immutable plaintext open, refresh retry and
warning branches, decrypted-cache tests, and stale documentation for
`.tg_index.db`, refresh/decrypt scopes, and retired media commands.

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
4. Implement only the small `store` interface.
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
| Provider | one vendored SQLCipher amalgamation; SQLCipher version nonempty; FTS5 enabled |
| Release | archive contains project, SQLCipher, OpenSSL, Go, and Go module license notices |

`make check` (gofmt, `go vet`, unit tests, and the `e2e/` contract suite
against the built binary), release builds, CLI help, and Homebrew smoke tests
remain mandatory.

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
- SQLCipher Community license:
  <https://www.zetetic.net/sqlcipher/license/>
