use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use crate::{contact, db, dictionary, message, parallel, paths, time};

const INDEX_FILE: &str = ".tg_index.db";
const SCHEMA_VERSION: i64 = 2;
const REFRESH_OVERLAP_SECS: i64 = 7 * 86400;

pub(crate) struct HotIndex {
    pub(crate) path: PathBuf,
    pub(crate) since: i64,
}

#[derive(Clone)]
struct SourceFile {
    key: String,
    path: PathBuf,
    mtime_ns: i64,
    size: i64,
}

#[derive(Clone)]
struct SourceRefresh {
    source: SourceFile,
    since: i64,
}

struct IndexedMessage {
    source_db: String,
    table_name: String,
    session_id: String,
    session_display: String,
    sender_account: String,
    sender_display: String,
    local_id: Option<i64>,
    local_type: i64,
    create_time: i64,
    body: String,
    marker: Option<i64>,
    packed_info: Vec<u8>,
}

impl HotIndex {
    pub(crate) fn covers(&self, since: i64) -> bool {
        since >= self.since
    }
}

pub(crate) fn ensure_recent(decrypted_dir: &Path, jobs: usize) -> Result<HotIndex, String> {
    paths::ensure_private_dir(decrypted_dir).map_err(|e| {
        format!(
            "Cannot create index parent {}: {}",
            decrypted_dir.display(),
            e
        )
    })?;

    let index_path = decrypted_dir.join(INDEX_FILE);
    let mut conn = Connection::open(&index_path)
        .map_err(|e| format!("Cannot open message index {}: {}", index_path.display(), e))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| format!("Set index journal mode: {}", e))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| format!("Set index synchronous mode: {}", e))?;
    ensure_schema(&conn)?;

    let build_since = time::default_recent_since();
    let current_since = meta_i64(&conn, "index_since")?;
    if current_since.is_none_or(|since| since > build_since) {
        rebuild_all(&mut conn, decrypted_dir, build_since, jobs)?;
    } else {
        refresh_changed_sources(&mut conn, decrypted_dir, build_since, jobs)?;
    }

    let since = meta_i64(&conn, "index_since")?.unwrap_or(build_since);
    Ok(HotIndex {
        path: index_path,
        since,
    })
}

pub(crate) fn open_existing_recent(decrypted_dir: &Path) -> Result<Option<HotIndex>, String> {
    let index_path = decrypted_dir.join(INDEX_FILE);
    if !index_path.exists() {
        return Ok(None);
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(&index_path, flags)
        .map_err(|e| format!("Cannot open message index {}: {}", index_path.display(), e))?;
    let _ = conn.busy_timeout(Duration::from_millis(50));

    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|e| format!("Read index schema version: {}", e))?;
    if version != SCHEMA_VERSION {
        return Ok(None);
    }

    let Some(since) = meta_i64(&conn, "index_since")? else {
        return Ok(None);
    };
    if !index_matches_sources(&conn, decrypted_dir)? {
        return Ok(None);
    }

    Ok(Some(HotIndex {
        path: index_path,
        since,
    }))
}

fn index_matches_sources(conn: &Connection, decrypted_dir: &Path) -> Result<bool, String> {
    let (contact_db, message_dbs) = db::find_decrypted_dbs(decrypted_dir);
    let contact_signature = contact_db
        .as_deref()
        .and_then(file_signature)
        .unwrap_or_default();
    let previous_contact_signature = meta_string(conn, "contact_signature")?.unwrap_or_default();
    if contact_signature != previous_contact_signature {
        return Ok(false);
    }

    let sources = source_files(decrypted_dir, message_dbs);
    let previous = previous_sources(conn)?;
    if previous.len() != sources.len() {
        return Ok(false);
    }
    Ok(sources.iter().all(|source| {
        previous
            .get(&source.key)
            .is_some_and(|signature| *signature == (source.mtime_ns, source.size))
    }))
}

fn ensure_schema(conn: &Connection) -> Result<(), String> {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|e| format!("Read index schema version: {}", e))?;
    if version != SCHEMA_VERSION {
        conn.execute_batch(
            "DROP TABLE IF EXISTS messages;
             DROP TABLE IF EXISTS source_files;
             DROP TABLE IF EXISTS meta;",
        )
        .map_err(|e| format!("Reset index schema: {}", e))?;
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS source_files (
            path TEXT PRIMARY KEY,
            mtime_ns INTEGER NOT NULL,
            size INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY,
            source_db TEXT NOT NULL,
            table_name TEXT NOT NULL,
            session_id TEXT NOT NULL,
            session_display TEXT NOT NULL,
            sender_account TEXT NOT NULL,
            sender_display TEXT NOT NULL,
            local_id INTEGER,
            local_type INTEGER NOT NULL,
            create_time INTEGER NOT NULL,
            body TEXT NOT NULL,
            marker INTEGER,
            packed_info BLOB NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_messages_time
            ON messages(create_time DESC);
        CREATE INDEX IF NOT EXISTS idx_messages_session_time
            ON messages(session_id, create_time DESC);
        CREATE INDEX IF NOT EXISTS idx_messages_source
            ON messages(source_db);",
    )
    .map_err(|e| format!("Create index schema: {}", e))?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(|e| format!("Set index schema version: {}", e))?;
    Ok(())
}

fn rebuild_all(
    conn: &mut Connection,
    decrypted_dir: &Path,
    since: i64,
    jobs: usize,
) -> Result<(), String> {
    let tx = conn
        .transaction()
        .map_err(|e| format!("Start index rebuild: {}", e))?;
    tx.execute_batch(
        "DELETE FROM messages;
         DELETE FROM source_files;
         DELETE FROM meta;",
    )
    .map_err(|e| format!("Clear index: {}", e))?;
    set_meta_i64_tx(&tx, "index_since", since)?;
    tx.commit()
        .map_err(|e| format!("Commit index reset: {}", e))?;
    refresh_changed_sources(conn, decrypted_dir, since, jobs)
}

fn refresh_changed_sources(
    conn: &mut Connection,
    decrypted_dir: &Path,
    since: i64,
    jobs: usize,
) -> Result<(), String> {
    let (contact_db, message_dbs) = db::find_decrypted_dbs(decrypted_dir);
    let contact_signature = contact_db
        .as_deref()
        .and_then(file_signature)
        .unwrap_or_default();
    let previous_contact_signature = meta_string(conn, "contact_signature")?.unwrap_or_default();
    let contact_changed = contact_signature != previous_contact_signature;

    let contacts = contact_db
        .as_ref()
        .and_then(|path| contact::load_contacts(path).ok())
        .unwrap_or_default();
    let table_context = table_context(&contacts);
    let sources = source_files(decrypted_dir, message_dbs);
    let previous = previous_sources(conn)?;

    let mut refreshes = Vec::new();
    if contact_changed {
        refreshes.extend(
            sources
                .iter()
                .cloned()
                .map(|source| SourceRefresh { source, since }),
        );
    } else {
        for source in &sources {
            let Some((old_mtime, old_size)) = previous.get(&source.key).copied() else {
                refreshes.push(SourceRefresh {
                    source: source.clone(),
                    since,
                });
                continue;
            };
            if (old_mtime, old_size) != (source.mtime_ns, source.size) {
                let refresh_since = if source.size < old_size {
                    since
                } else {
                    indexed_source_latest(conn, &source.key)?
                        .map(|latest| (latest - REFRESH_OVERLAP_SECS).max(since))
                        .unwrap_or(since)
                };
                refreshes.push(SourceRefresh {
                    source: source.clone(),
                    since: refresh_since,
                });
            }
        }
    }

    let source_keys: std::collections::HashSet<&str> =
        sources.iter().map(|source| source.key.as_str()).collect();
    let removed = previous
        .keys()
        .filter(|key| !source_keys.contains(key.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    if refreshes.is_empty() && removed.is_empty() && !contact_changed {
        return Ok(());
    }

    let job_count = parallel::job_count(jobs, 8);
    let rows_by_source = parallel::map_ordered(refreshes.clone(), job_count, |refresh| {
        collect_source_messages(&refresh.source, refresh.since, &table_context)
    });

    let tx = conn
        .transaction()
        .map_err(|e| format!("Start index update: {}", e))?;

    if contact_changed {
        tx.execute("DELETE FROM messages", [])
            .map_err(|e| format!("Clear messages after contact change: {}", e))?;
        tx.execute("DELETE FROM source_files", [])
            .map_err(|e| format!("Clear source state after contact change: {}", e))?;
    } else {
        for key in &removed {
            tx.execute("DELETE FROM messages WHERE source_db = ?1", params![key])
                .map_err(|e| format!("Delete removed source {}: {}", key, e))?;
            tx.execute("DELETE FROM source_files WHERE path = ?1", params![key])
                .map_err(|e| format!("Delete removed source state {}: {}", key, e))?;
        }
    }

    for (refresh, rows) in refreshes.iter().zip(rows_by_source) {
        tx.execute(
            "DELETE FROM messages WHERE source_db = ?1 AND create_time >= ?2",
            params![refresh.source.key, refresh.since],
        )
        .map_err(|e| format!("Delete stale source window {}: {}", refresh.source.key, e))?;
        insert_messages(&tx, &rows?)?;
        tx.execute(
            "INSERT OR REPLACE INTO source_files (path, mtime_ns, size)
             VALUES (?1, ?2, ?3)",
            params![
                refresh.source.key,
                refresh.source.mtime_ns,
                refresh.source.size
            ],
        )
        .map_err(|e| format!("Record source state {}: {}", refresh.source.key, e))?;
    }

    set_meta_i64_tx(&tx, "index_since", since)?;
    set_meta_string_tx(&tx, "contact_signature", &contact_signature)?;
    tx.commit()
        .map_err(|e| format!("Commit index update: {}", e))?;
    Ok(())
}

fn insert_messages(conn: &Connection, rows: &[IndexedMessage]) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "INSERT INTO messages (
                source_db, table_name, session_id, session_display,
                sender_account, sender_display, local_id, local_type,
                create_time, body, marker, packed_info
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )
        .map_err(|e| format!("Prepare index insert: {}", e))?;

    for row in rows {
        stmt.execute(params![
            row.source_db,
            row.table_name,
            row.session_id,
            row.session_display,
            row.sender_account,
            row.sender_display,
            row.local_id,
            row.local_type,
            row.create_time,
            row.body,
            row.marker,
            row.packed_info,
        ])
        .map_err(|e| format!("Insert indexed message: {}", e))?;
    }
    Ok(())
}

fn collect_source_messages(
    source: &SourceFile,
    since: i64,
    table_context: &TableContext,
) -> Result<Vec<IndexedMessage>, String> {
    let conn = Connection::open(&source.path)
        .map_err(|e| format!("Cannot open {}: {}", source.path.display(), e))?;
    let tables = list_message_tables(&conn)?;
    let name2id = load_name2id(&conn);
    let body_col = db::quote_identifier(&dictionary::msg_body_column());
    let marker_col = db::quote_identifier(&dictionary::msg_compression_marker_column());
    let sender_col = db::quote_identifier(&dictionary::msg_sender_column());
    let packed_col = db::quote_identifier(&dictionary::msg_packed_meta_column());
    let mut messages = Vec::new();

    for table_name in tables {
        let quoted_table = db::quote_identifier(&table_name);
        let local_id_col = if db::table_has_column(&conn, &table_name, "local_id") {
            db::quote_identifier("local_id")
        } else {
            "NULL".to_string()
        };
        let sql = format!(
            "SELECT {local_id_col}, local_type, create_time, {body_col}, {marker_col}, {sender_col}, {packed_col}
             FROM {quoted_table}
             WHERE create_time >= ?1
             ORDER BY create_time ASC",
            local_id_col = local_id_col
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            continue;
        };

        let session_id = table_context
            .table_to_session
            .get(&table_name)
            .cloned()
            .unwrap_or_else(|| table_name.clone());
        let session_display = table_context
            .table_to_display
            .get(&table_name)
            .cloned()
            .unwrap_or_else(|| "(?)".to_string());

        let rows = stmt
            .query_map(params![since], |row| {
                let marker: Option<i64> = row.get::<_, Option<i64>>(4)?;
                let body = read_message_body(row, 3, marker);
                let sender_id: i64 = row.get::<_, Option<i64>>(5)?.unwrap_or(0);
                let sender_account = name2id.get(&sender_id).cloned().unwrap_or_default();
                let sender_display = table_context
                    .sender_display
                    .get(&sender_account)
                    .cloned()
                    .unwrap_or_else(|| sender_account.clone());
                let packed_info: Vec<u8> = row.get::<_, Option<Vec<u8>>>(6)?.unwrap_or_default();
                Ok(IndexedMessage {
                    source_db: source.key.clone(),
                    table_name: table_name.clone(),
                    session_id: session_id.clone(),
                    session_display: session_display.clone(),
                    sender_account,
                    sender_display,
                    local_id: row.get::<_, Option<i64>>(0)?,
                    local_type: row.get::<_, Option<i64>>(1)?.unwrap_or(-1),
                    create_time: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    body,
                    marker,
                    packed_info,
                })
            })
            .map_err(|e| format!("Read indexed messages from {}: {}", source.key, e))?;
        messages.extend(rows.filter_map(|row| row.ok()));
    }

    Ok(messages)
}

struct TableContext {
    table_to_session: HashMap<String, String>,
    table_to_display: HashMap<String, String>,
    sender_display: HashMap<String, String>,
}

fn table_context(contacts: &HashMap<String, contact::Contact>) -> TableContext {
    let mut table_to_session = HashMap::new();
    let mut table_to_display = HashMap::new();
    let mut sender_display = HashMap::new();

    for (username, contact) in contacts {
        let table = db::msg_table_name(username);
        let display = contact.personal_display_name().to_string();
        table_to_session.insert(table.clone(), username.clone());
        table_to_display.insert(table, display.clone());
        sender_display.insert(username.clone(), display);
    }

    TableContext {
        table_to_session,
        table_to_display,
        sender_display,
    }
}

fn list_message_tables(conn: &Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'")
        .map_err(|e| format!("Cannot list message tables: {}", e))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("Cannot read message table names: {}", e))?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

fn load_name2id(conn: &Connection) -> HashMap<i64, String> {
    match conn.prepare("SELECT rowid, user_name FROM Name2Id") {
        Ok(mut stmt) => stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .ok()
            .map(|rows| rows.filter_map(|row| row.ok()).collect())
            .unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

fn read_message_body(row: &rusqlite::Row<'_>, index: usize, marker: Option<i64>) -> String {
    if marker == Some(4) {
        if let Ok(bytes) = row.get::<_, Vec<u8>>(index) {
            return message::try_decompress(&bytes).unwrap_or_default();
        }
    }

    match row.get::<_, Option<String>>(index) {
        Ok(Some(value)) => value,
        _ => match row.get::<_, Option<Vec<u8>>>(index) {
            Ok(Some(bytes)) => String::from_utf8(bytes).unwrap_or_default(),
            _ => String::new(),
        },
    }
}

fn source_files(decrypted_dir: &Path, message_dbs: Vec<PathBuf>) -> Vec<SourceFile> {
    message_dbs
        .into_iter()
        .filter_map(|path| {
            let key = relative_label(decrypted_dir, &path);
            let (mtime_ns, size) = file_stat(&path)?;
            Some(SourceFile {
                key,
                path,
                mtime_ns,
                size,
            })
        })
        .collect()
}

fn previous_sources(conn: &Connection) -> Result<HashMap<String, (i64, i64)>, String> {
    let mut stmt = conn
        .prepare("SELECT path, mtime_ns, size FROM source_files")
        .map_err(|e| format!("Prepare source state query: {}", e))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (row.get::<_, i64>(1)?, row.get::<_, i64>(2)?),
            ))
        })
        .map_err(|e| format!("Read source state: {}", e))?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

fn indexed_source_latest(conn: &Connection, source_key: &str) -> Result<Option<i64>, String> {
    conn.query_row(
        "SELECT MAX(create_time) FROM messages WHERE source_db = ?1",
        params![source_key],
        |row| row.get::<_, Option<i64>>(0),
    )
    .optional()
    .map(|value| value.flatten())
    .map_err(|e| format!("Read indexed source latest {}: {}", source_key, e))
}

fn file_signature(path: &Path) -> Option<String> {
    let (mtime_ns, size) = file_stat(path)?;
    Some(format!("{}:{}", mtime_ns, size))
}

fn file_stat(path: &Path) -> Option<(i64, i64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime_ns = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos()
        .try_into()
        .ok()?;
    Some((mtime_ns, meta.len().try_into().ok()?))
}

fn relative_label(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn meta_string(conn: &Connection, key: &str) -> Result<Option<String>, String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
    .map_err(|e| format!("Read index meta {}: {}", key, e))
}

fn meta_i64(conn: &Connection, key: &str) -> Result<Option<i64>, String> {
    Ok(meta_string(conn, key)?.and_then(|value| value.parse().ok()))
}

fn set_meta_i64_tx(conn: &Connection, key: &str, value: i64) -> Result<(), String> {
    set_meta_string_tx(conn, key, &value.to_string())
}

fn set_meta_string_tx(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        params![key, value],
    )
    .map_err(|e| format!("Write index meta {}: {}", key, e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::tempdir;

    #[test]
    fn builds_recent_index_for_numbered_message_dbs() {
        let dir = tempdir().unwrap();
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();
        let conn = Connection::open(message_dir.join("message_0.db")).unwrap();
        let table = db::msg_table_name("tgid_indexed");
        let body_col = db::quote_identifier(&dictionary::msg_body_column());
        let marker_col = db::quote_identifier(&dictionary::msg_compression_marker_column());
        let sender_col = db::quote_identifier(&dictionary::msg_sender_column());
        let packed_col = db::quote_identifier(&dictionary::msg_packed_meta_column());
        conn.execute(
            &format!(
                "CREATE TABLE {} (
                    local_type INTEGER,
                    create_time INTEGER,
                    {} TEXT,
                    {} INTEGER,
                    {} INTEGER,
                    {} BLOB
                )",
                table, body_col, marker_col, sender_col, packed_col
            ),
            [],
        )
        .unwrap();
        conn.execute("CREATE TABLE Name2Id (user_name TEXT)", [])
            .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO {} (local_type, create_time, {}, {}, {}, {})
                 VALUES (1, ?1, 'indexed needle', NULL, 0, x'')",
                table, body_col, marker_col, sender_col, packed_col
            ),
            params![time::default_recent_since() + 1],
        )
        .unwrap();
        drop(conn);

        let index = ensure_recent(dir.path(), 1).unwrap();
        let indexed = Connection::open(index.path).unwrap();
        let count: i64 = indexed
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();

        assert_eq!(count, 1);
        assert!(open_existing_recent(dir.path()).unwrap().is_some());

        std::fs::copy(
            message_dir.join("message_0.db"),
            message_dir.join("message_1.db"),
        )
        .unwrap();
        assert!(open_existing_recent(dir.path()).unwrap().is_none());
    }

    #[test]
    fn open_existing_recent_returns_none_without_index_file() {
        let dir = tempdir().unwrap();

        let index = open_existing_recent(dir.path()).unwrap();

        assert!(index.is_none());
    }
}
