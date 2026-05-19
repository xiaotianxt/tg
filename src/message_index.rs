use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{
    contact, db, dictionary, index_policy, media, message, parallel, paths, row_identity, time,
};

const INDEX_FILE: &str = ".tg_index.db";
const SCHEMA_VERSION: i64 = 8;
const REFRESH_OVERLAP_SECS: i64 = 7 * 86400;

pub(crate) struct HotIndex {
    pub(crate) path: PathBuf,
    pub(crate) since: i64,
}

#[derive(Clone)]
struct SourceFile {
    key: String,
    path: PathBuf,
    fingerprint: row_identity::SourceFingerprint,
}

#[derive(Clone)]
struct SourceRefresh {
    source: SourceFile,
    since: i64,
    mode: index_policy::RefreshMode,
}

struct CollectedSource {
    messages: Vec<IndexedMessage>,
    table_states: Vec<TableState>,
    replaced_windows: Vec<TableWindow>,
    current_tables: HashSet<String>,
}

struct TableState {
    table_name: String,
    max_local_id: Option<i64>,
}

struct TableWindow {
    table_name: String,
    since: i64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionCatchUpStats {
    pub(crate) changed_sessions: usize,
    pub(crate) refreshed_tables: usize,
}

#[derive(Clone, PartialEq, Eq)]
struct SessionIndexState {
    username: String,
    last_timestamp: i64,
    sort_timestamp: i64,
    last_msg_local_id: Option<i64>,
    last_msg_type: Option<i64>,
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
    media_type: String,
    create_time: i64,
    raw_body: String,
    decoded_body: String,
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
    conn.busy_timeout(Duration::from_secs(30))
        .map_err(|e| format!("Set index busy timeout: {}", e))?;
    ensure_schema(&conn)?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| format!("Set index journal mode: {}", e))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| format!("Set index synchronous mode: {}", e))?;

    let build_since = time::default_recent_since();
    let current_since = meta_i64(&conn, "index_since")?;
    if current_since.is_none_or(|since| since > build_since) {
        rebuild_all(&mut conn, decrypted_dir, build_since, jobs)?;
    } else {
        refresh_changed_sources(&mut conn, decrypted_dir, build_since, jobs)?;
    }
    if let Err(e) = sync_current_session_states(&mut conn, decrypted_dir) {
        log::debug!("Message index session state sync skipped: {}", e);
    }

    let since = meta_i64(&conn, "index_since")?.unwrap_or(build_since);
    checkpoint_index(&conn);
    Ok(HotIndex {
        path: index_path,
        since,
    })
}

pub(crate) fn ensure_recent_sessions(
    decrypted_dir: &Path,
    jobs: usize,
) -> Result<SessionCatchUpStats, String> {
    paths::ensure_private_dir(decrypted_dir).map_err(|e| {
        format!(
            "Cannot create index parent {}: {}",
            decrypted_dir.display(),
            e
        )
    })?;

    let Some(session_db) = db::find_decrypted_session_db(decrypted_dir) else {
        return Ok(SessionCatchUpStats::default());
    };

    let index_path = decrypted_dir.join(INDEX_FILE);
    let mut conn = Connection::open(&index_path)
        .map_err(|e| format!("Cannot open message index {}: {}", index_path.display(), e))?;
    conn.busy_timeout(Duration::from_secs(30))
        .map_err(|e| format!("Set index busy timeout: {}", e))?;
    ensure_schema(&conn)?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| format!("Set index journal mode: {}", e))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| format!("Set index synchronous mode: {}", e))?;

    let build_since = time::default_recent_since();
    let index_since = meta_i64(&conn, "index_since")?.unwrap_or(build_since);
    if meta_i64(&conn, "index_since")?.is_none() {
        set_meta_i64_tx(&conn, "index_since", index_since)?;
    }

    let current_sessions = load_session_index_states(&session_db)?;
    if current_sessions.is_empty() {
        return Ok(SessionCatchUpStats::default());
    }
    let previous_sessions = previous_session_states(&conn)?;
    let changed_sessions = current_sessions
        .into_iter()
        .filter(|state| previous_sessions.get(&state.username) != Some(state))
        .collect::<Vec<_>>();
    if changed_sessions.is_empty() {
        return Ok(SessionCatchUpStats::default());
    }

    let (contact_db, message_dbs) = db::find_decrypted_dbs(decrypted_dir);
    if message_dbs.is_empty() {
        return Ok(SessionCatchUpStats {
            changed_sessions: changed_sessions.len(),
            refreshed_tables: 0,
        });
    }

    let contacts = contact_db
        .as_ref()
        .and_then(|path| contact::load_contacts(path).ok())
        .unwrap_or_default();
    let table_context = table_context(&contacts);
    let source_files = source_files(decrypted_dir, message_dbs);
    let table_jobs = parallel::job_count(jobs, 8);
    let mut refreshes = Vec::new();
    for state in &changed_sessions {
        let table_name = db::msg_table_name(&state.username);
        for source in &source_files {
            refreshes.push((source.clone(), state.username.clone(), table_name.clone()));
        }
    }

    let collected = parallel::map_ordered(refreshes.clone(), table_jobs, |refresh| {
        let (source, username, table_name) = refresh;
        collect_session_table_messages(&source, &username, &table_name, &table_context, index_since)
    });

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("Start session index catch-up: {}", e))?;
    let mut refreshed_tables = 0usize;
    for ((source, _username, table_name), collected) in refreshes.into_iter().zip(collected) {
        let Some(collected) = collected? else {
            continue;
        };
        refreshed_tables += 1;
        tx.execute(
            "DELETE FROM messages
             WHERE source_db = ?1 AND table_name = ?2 AND create_time >= ?3",
            params![source.key, table_name, index_since],
        )
        .map_err(|e| {
            format!(
                "Delete stale session table window {}.{}: {}",
                source.key, table_name, e
            )
        })?;
        insert_messages(&tx, &collected.messages)?;
        insert_table_states(&tx, &source.key, &collected.table_states)?;
    }
    insert_session_states(&tx, &changed_sessions)?;
    tx.commit()
        .map_err(|e| format!("Commit session index catch-up: {}", e))?;
    checkpoint_index(&conn);

    Ok(SessionCatchUpStats {
        changed_sessions: changed_sessions.len(),
        refreshed_tables,
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

    let version = index_schema_version(&conn)?;
    if !is_supported_schema_version(version) {
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

pub(crate) fn open_existing_query_index(decrypted_dir: &Path) -> Result<Option<HotIndex>, String> {
    let index_path = decrypted_dir.join(INDEX_FILE);
    if !index_path.exists() {
        return Ok(None);
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(&index_path, flags)
        .map_err(|e| format!("Cannot open message index {}: {}", index_path.display(), e))?;
    let _ = conn.busy_timeout(Duration::from_millis(50));
    let version = index_schema_version(&conn)?;
    if !is_supported_schema_version(version) {
        return Ok(None);
    }
    let Some(since) = meta_i64(&conn, "index_since")? else {
        return Ok(None);
    };
    Ok(Some(HotIndex {
        path: index_path,
        since,
    }))
}

fn index_matches_sources(conn: &Connection, decrypted_dir: &Path) -> Result<bool, String> {
    let (_, message_dbs) = db::find_decrypted_dbs(decrypted_dir);
    let sources = source_files(decrypted_dir, message_dbs);
    let previous = previous_sources(conn)?;
    if previous.len() != sources.len() {
        return Ok(false);
    }
    Ok(sources.iter().all(|source| {
        previous
            .get(&source.key)
            .is_some_and(|signature| *signature == source.fingerprint)
    }))
}

fn ensure_schema(conn: &Connection) -> Result<(), String> {
    let version = index_schema_version(conn)?;
    if version != SCHEMA_VERSION && version != 7 && version != 6 {
        conn.pragma_update(None, "journal_mode", "DELETE")
            .map_err(|e| format!("Prepare index schema reset: {}", e))?;
        conn.execute_batch(
            "DROP TABLE IF EXISTS messages;
             DROP TABLE IF EXISTS source_files;
             DROP TABLE IF EXISTS table_states;
             DROP TABLE IF EXISTS session_states;
             DROP TABLE IF EXISTS meta;",
        )
        .map_err(|e| format!("Reset index schema: {}", e))?;
        conn.execute_batch("VACUUM;")
            .map_err(|e| format!("Compact reset index schema: {}", e))?;
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
        CREATE TABLE IF NOT EXISTS table_states (
            source_db TEXT NOT NULL,
            table_name TEXT NOT NULL,
            max_local_id INTEGER,
            PRIMARY KEY(source_db, table_name)
        );
        CREATE TABLE IF NOT EXISTS session_states (
            username TEXT PRIMARY KEY,
            last_timestamp INTEGER NOT NULL,
            sort_timestamp INTEGER NOT NULL,
            last_msg_local_id INTEGER,
            last_msg_type INTEGER
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
            media_type TEXT NOT NULL,
            create_time INTEGER NOT NULL,
            raw_body TEXT NOT NULL,
            decoded_body TEXT NOT NULL,
            marker INTEGER,
            packed_info BLOB NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_messages_time
            ON messages(create_time DESC);
        CREATE INDEX IF NOT EXISTS idx_messages_session_time
            ON messages(session_id, create_time DESC);
        CREATE INDEX IF NOT EXISTS idx_messages_media_time
            ON messages(media_type, create_time DESC);
        CREATE INDEX IF NOT EXISTS idx_messages_source
            ON messages(source_db);
        CREATE INDEX IF NOT EXISTS idx_table_states_source
            ON table_states(source_db);",
    )
    .map_err(|e| format!("Create index schema: {}", e))?;
    if version == 6 {
        seed_table_states(conn)?;
    }
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
         DELETE FROM table_states;
         DELETE FROM session_states;
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
    let sources = source_files(decrypted_dir, message_dbs);
    let previous = previous_sources(conn)?;

    let mut refreshes = Vec::new();
    for source in &sources {
        if let Some(mode) = index_policy::source_refresh_mode(
            previous.get(&source.key).copied(),
            source.fingerprint,
        ) {
            let refresh_since = match mode {
                index_policy::RefreshMode::FullWindow => since,
                index_policy::RefreshMode::LocalIdCursor => index_policy::overlap_since(
                    indexed_source_latest(conn, &source.key)?,
                    since,
                    REFRESH_OVERLAP_SECS,
                ),
            };
            refreshes.push(SourceRefresh {
                source: source.clone(),
                since: refresh_since,
                mode,
            });
        }
    }

    let source_keys: std::collections::HashSet<&str> =
        sources.iter().map(|source| source.key.as_str()).collect();
    let removed = previous
        .keys()
        .filter(|key| !source_keys.contains(key.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    if refreshes.is_empty() && removed.is_empty() {
        return Ok(());
    }

    let contacts = contact_db
        .as_ref()
        .and_then(|path| contact::load_contacts(path).ok())
        .unwrap_or_default();
    let contact_signature = contact_signature(&contacts);
    let table_context = table_context(&contacts);
    let previous_tables = previous_table_states(conn)?;

    let job_count = parallel::job_count(jobs, 8);
    let rows_by_source = parallel::map_ordered(refreshes.clone(), job_count, |refresh| {
        collect_source_messages(&refresh, &table_context, &previous_tables)
    });

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("Start index update: {}", e))?;

    for key in &removed {
        tx.execute("DELETE FROM messages WHERE source_db = ?1", params![key])
            .map_err(|e| format!("Delete removed source {}: {}", key, e))?;
        tx.execute("DELETE FROM source_files WHERE path = ?1", params![key])
            .map_err(|e| format!("Delete removed source state {}: {}", key, e))?;
        tx.execute(
            "DELETE FROM table_states WHERE source_db = ?1",
            params![key],
        )
        .map_err(|e| format!("Delete removed table state {}: {}", key, e))?;
    }

    for (refresh, collected) in refreshes.iter().zip(rows_by_source) {
        let collected = collected?;
        delete_removed_tables(&tx, refresh, &previous_tables, &collected.current_tables)?;
        for window in &collected.replaced_windows {
            tx.execute(
                "DELETE FROM messages
                 WHERE source_db = ?1 AND table_name = ?2 AND create_time >= ?3",
                params![refresh.source.key, window.table_name, window.since],
            )
            .map_err(|e| {
                format!(
                    "Delete stale table window {}.{}: {}",
                    refresh.source.key, window.table_name, e
                )
            })?;
        }
        insert_messages(&tx, &collected.messages)?;
        tx.execute(
            "DELETE FROM table_states WHERE source_db = ?1",
            params![refresh.source.key],
        )
        .map_err(|e| format!("Clear table state {}: {}", refresh.source.key, e))?;
        insert_table_states(&tx, &refresh.source.key, &collected.table_states)?;
        tx.execute(
            "INSERT OR REPLACE INTO source_files (path, mtime_ns, size)
             VALUES (?1, ?2, ?3)",
            params![
                refresh.source.key,
                refresh.source.fingerprint.mtime_ns,
                refresh.source.fingerprint.size
            ],
        )
        .map_err(|e| format!("Record source state {}: {}", refresh.source.key, e))?;
    }

    set_meta_i64_tx(&tx, "index_since", since)?;
    set_meta_string_tx(&tx, "contact_signature", &contact_signature)?;
    tx.commit()
        .map_err(|e| format!("Commit index update: {}", e))?;
    checkpoint_index(conn);
    Ok(())
}

fn checkpoint_index(conn: &Connection) {
    let _ = conn.busy_timeout(Duration::from_millis(250));
    if let Err(e) = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
        log::debug!("Message index WAL checkpoint failed: {}", e);
    }
    let _ = conn.busy_timeout(Duration::from_secs(30));
}

fn insert_messages(conn: &Connection, rows: &[IndexedMessage]) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "INSERT INTO messages (
                source_db, table_name, session_id, session_display,
                sender_account, sender_display, local_id, local_type,
                media_type, create_time, raw_body, decoded_body, marker, packed_info
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
            row.media_type,
            row.create_time,
            row.raw_body,
            row.decoded_body,
            row.marker,
            row.packed_info,
        ])
        .map_err(|e| format!("Insert indexed message: {}", e))?;
    }
    Ok(())
}

fn insert_table_states(
    conn: &Connection,
    source_key: &str,
    states: &[TableState],
) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "INSERT OR REPLACE INTO table_states (source_db, table_name, max_local_id)
             VALUES (?1, ?2, ?3)",
        )
        .map_err(|e| format!("Prepare table state insert: {}", e))?;

    for state in states {
        stmt.execute(params![source_key, state.table_name, state.max_local_id])
            .map_err(|e| {
                format!(
                    "Insert table state {}.{}: {}",
                    source_key, state.table_name, e
                )
            })?;
    }
    Ok(())
}

fn seed_table_states(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "DELETE FROM table_states;
         INSERT OR REPLACE INTO table_states (source_db, table_name, max_local_id)
         SELECT source_db, table_name, MAX(local_id)
         FROM messages
         GROUP BY source_db, table_name;",
    )
    .map_err(|e| format!("Seed table cursor state: {}", e))?;
    Ok(())
}

fn previous_table_states(
    conn: &Connection,
) -> Result<HashMap<(String, String), Option<i64>>, String> {
    let mut stmt = conn
        .prepare("SELECT source_db, table_name, max_local_id FROM table_states")
        .map_err(|e| format!("Prepare table state query: {}", e))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                row.get::<_, Option<i64>>(2)?,
            ))
        })
        .map_err(|e| format!("Read table state: {}", e))?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

fn delete_removed_tables(
    conn: &Connection,
    refresh: &SourceRefresh,
    previous_tables: &HashMap<(String, String), Option<i64>>,
    current_tables: &HashSet<String>,
) -> Result<(), String> {
    for (source_db, table_name) in previous_tables.keys() {
        if source_db != &refresh.source.key || current_tables.contains(table_name) {
            continue;
        }
        conn.execute(
            "DELETE FROM messages WHERE source_db = ?1 AND table_name = ?2",
            params![source_db, table_name],
        )
        .map_err(|e| format!("Delete removed table {}.{}: {}", source_db, table_name, e))?;
        conn.execute(
            "DELETE FROM table_states WHERE source_db = ?1 AND table_name = ?2",
            params![source_db, table_name],
        )
        .map_err(|e| {
            format!(
                "Delete removed table state {}.{}: {}",
                source_db, table_name, e
            )
        })?;
    }
    Ok(())
}

fn collect_source_messages(
    refresh: &SourceRefresh,
    table_context: &TableContext,
    previous_tables: &HashMap<(String, String), Option<i64>>,
) -> Result<CollectedSource, String> {
    let source = &refresh.source;
    let conn = Connection::open(&source.path)
        .map_err(|e| format!("Cannot open {}: {}", source.path.display(), e))?;
    let tables = list_message_tables(&conn)?;
    let name2id = load_name2id(&conn);
    let body_col = db::quote_identifier(dictionary::msg_body_column());
    let marker_col = db::quote_identifier(dictionary::msg_compression_marker_column());
    let sender_col = db::quote_identifier(dictionary::msg_sender_column());
    let packed_col = db::quote_identifier(dictionary::msg_packed_meta_column());
    let mut messages = Vec::new();
    let mut table_states = Vec::new();
    let mut replaced_windows = Vec::new();
    let mut current_tables = HashSet::new();

    for table_name in tables {
        current_tables.insert(table_name.clone());
        let quoted_table = db::quote_identifier(&table_name);
        let has_local_id = db::table_has_column(&conn, &table_name, "local_id");
        let current_max_local_id = if has_local_id {
            table_max_local_id(&conn, &table_name)?
        } else {
            None
        };
        table_states.push(TableState {
            table_name: table_name.clone(),
            max_local_id: current_max_local_id,
        });

        let local_id_col = if has_local_id {
            db::quote_identifier("local_id")
        } else {
            "NULL".to_string()
        };
        let previous_max_local_id = previous_tables
            .get(&(source.key.clone(), table_name.clone()))
            .copied()
            .flatten();
        let local_id_cursor = refresh.mode == index_policy::RefreshMode::LocalIdCursor
            && has_local_id
            && previous_max_local_id.is_some();
        if local_id_cursor && current_max_local_id <= previous_max_local_id {
            continue;
        }

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

        let map_row = |row: &rusqlite::Row<'_>| {
            let marker: Option<i64> = row.get::<_, Option<i64>>(4)?;
            let raw_body = read_message_body(row, 3, marker);
            let sender_id: i64 = row.get::<_, Option<i64>>(5)?.unwrap_or(0);
            let sender_account = name2id.get(&sender_id).cloned().unwrap_or_default();
            let sender_display = table_context
                .sender_display
                .get(&sender_account)
                .cloned()
                .unwrap_or_else(|| sender_account.clone());
            let packed_info: Vec<u8> = row.get::<_, Option<Vec<u8>>>(6)?.unwrap_or_default();
            let local_id = row.get::<_, Option<i64>>(0)?;
            let local_type = row.get::<_, Option<i64>>(1)?.unwrap_or(-1);
            let decoded_body = indexed_decoded_body(
                local_type,
                &raw_body,
                marker,
                &packed_info,
                &session_display,
                local_id,
                table_context,
            );
            Ok(IndexedMessage {
                source_db: source.key.clone(),
                table_name: table_name.clone(),
                session_id: session_id.clone(),
                session_display: session_display.clone(),
                sender_account,
                sender_display,
                local_id,
                local_type,
                media_type: media::message_media_type(local_type, &raw_body)
                    .unwrap_or("")
                    .to_string(),
                create_time: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                raw_body,
                decoded_body,
                marker,
                packed_info,
            })
        };

        if let Some(previous_max_local_id) =
            local_id_cursor.then_some(previous_max_local_id).flatten()
        {
            let sql = format!(
                "SELECT {local_id_col}, local_type, create_time, {body_col}, {marker_col}, {sender_col}, {packed_col}
                 FROM {quoted_table}
                 WHERE {local_id_col} > ?1 AND create_time >= ?2
                 ORDER BY {local_id_col} ASC",
                local_id_col = local_id_col
            );
            let Ok(mut stmt) = conn.prepare(&sql) else {
                continue;
            };
            let rows = stmt
                .query_map(params![previous_max_local_id, refresh.since], |row| {
                    map_row(row)
                })
                .map_err(|e| format!("Read indexed messages from {}: {}", source.key, e))?;
            messages.extend(rows.filter_map(|row| row.ok()));
        } else {
            replaced_windows.push(TableWindow {
                table_name: table_name.clone(),
                since: refresh.since,
            });
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
            let rows = stmt
                .query_map(params![refresh.since], |row| map_row(row))
                .map_err(|e| format!("Read indexed messages from {}: {}", source.key, e))?;
            messages.extend(rows.filter_map(|row| row.ok()));
        }
    }

    Ok(CollectedSource {
        messages,
        table_states,
        replaced_windows,
        current_tables,
    })
}

fn collect_session_table_messages(
    source: &SourceFile,
    username: &str,
    table_name: &str,
    table_context: &TableContext,
    since: i64,
) -> Result<Option<CollectedSource>, String> {
    let conn = Connection::open(&source.path)
        .map_err(|e| format!("Cannot open {}: {}", source.path.display(), e))?;
    if !message_table_exists(&conn, table_name) {
        return Ok(None);
    }

    let name2id = load_name2id(&conn);
    let body_col = db::quote_identifier(dictionary::msg_body_column());
    let marker_col = db::quote_identifier(dictionary::msg_compression_marker_column());
    let sender_col = db::quote_identifier(dictionary::msg_sender_column());
    let packed_col = db::quote_identifier(dictionary::msg_packed_meta_column());
    let quoted_table = db::quote_identifier(table_name);
    let has_local_id = db::table_has_column(&conn, table_name, "local_id");
    let local_id_col = if has_local_id {
        db::quote_identifier("local_id")
    } else {
        "NULL".to_string()
    };
    let current_max_local_id = if has_local_id {
        table_max_local_id(&conn, table_name)?
    } else {
        None
    };
    let session_display = table_context
        .table_to_display
        .get(table_name)
        .cloned()
        .unwrap_or_else(|| username.to_string());
    let sql = format!(
        "SELECT {local_id_col}, local_type, create_time, {body_col}, {marker_col}, {sender_col}, {packed_col}
         FROM {quoted_table}
         WHERE create_time >= ?1
         ORDER BY create_time ASC",
        local_id_col = local_id_col
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("Read session table {}.{}: {}", source.key, table_name, e))?;
    let rows = stmt
        .query_map(params![since], |row| {
            let marker: Option<i64> = row.get::<_, Option<i64>>(4)?;
            let raw_body = read_message_body(row, 3, marker);
            let sender_id: i64 = row.get::<_, Option<i64>>(5)?.unwrap_or(0);
            let sender_account = name2id.get(&sender_id).cloned().unwrap_or_default();
            let sender_display = table_context
                .sender_display
                .get(&sender_account)
                .cloned()
                .unwrap_or_else(|| sender_account.clone());
            let packed_info: Vec<u8> = row.get::<_, Option<Vec<u8>>>(6)?.unwrap_or_default();
            let local_id = row.get::<_, Option<i64>>(0)?;
            let local_type = row.get::<_, Option<i64>>(1)?.unwrap_or(-1);
            let decoded_body = indexed_decoded_body(
                local_type,
                &raw_body,
                marker,
                &packed_info,
                &session_display,
                local_id,
                table_context,
            );
            Ok(IndexedMessage {
                source_db: source.key.clone(),
                table_name: table_name.to_string(),
                session_id: username.to_string(),
                session_display: session_display.clone(),
                sender_account,
                sender_display,
                local_id,
                local_type,
                media_type: media::message_media_type(local_type, &raw_body)
                    .unwrap_or("")
                    .to_string(),
                create_time: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                raw_body,
                decoded_body,
                marker,
                packed_info,
            })
        })
        .map_err(|e| format!("Read indexed messages from {}: {}", source.key, e))?;

    Ok(Some(CollectedSource {
        messages: rows.filter_map(|row| row.ok()).collect(),
        table_states: vec![TableState {
            table_name: table_name.to_string(),
            max_local_id: current_max_local_id,
        }],
        replaced_windows: vec![TableWindow {
            table_name: table_name.to_string(),
            since,
        }],
        current_tables: HashSet::new(),
    }))
}

fn table_max_local_id(conn: &Connection, table_name: &str) -> Result<Option<i64>, String> {
    let sql = format!(
        "SELECT MAX({}) FROM {}",
        db::quote_identifier("local_id"),
        db::quote_identifier(table_name)
    );
    conn.query_row(&sql, [], |row| row.get::<_, Option<i64>>(0))
        .map_err(|e| format!("Read table max local_id {}: {}", table_name, e))
}

fn indexed_decoded_body(
    local_type: i64,
    raw_body: &str,
    marker: Option<i64>,
    packed_info: &[u8],
    session_display: &str,
    local_id: Option<i64>,
    table_context: &TableContext,
) -> String {
    let voice_id = if media::local_type_low32(local_type) == 34 {
        local_id.filter(|id| *id > 0)
    } else {
        None
    };
    message::decode_message_with_context(
        local_type as i32,
        raw_body,
        session_display,
        marker,
        packed_info,
        message::DecodeContext {
            time_bucket: time::MessageTimeBucket::Minute(1),
            voice_id,
        },
        |id| {
            table_context
                .sender_display
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.to_string())
        },
    )
    .content
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

fn message_table_exists(conn: &Connection, table_name: &str) -> bool {
    conn.prepare(&format!(
        "SELECT 1 FROM {} LIMIT 1",
        db::quote_identifier(table_name)
    ))
    .is_ok()
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
            let fingerprint = row_identity::SourceFingerprint::from_path(&path)?;
            Some(SourceFile {
                key,
                path,
                fingerprint,
            })
        })
        .collect()
}

fn previous_sources(
    conn: &Connection,
) -> Result<HashMap<String, row_identity::SourceFingerprint>, String> {
    let mut stmt = conn
        .prepare("SELECT path, mtime_ns, size FROM source_files")
        .map_err(|e| format!("Prepare source state query: {}", e))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row_identity::SourceFingerprint {
                    mtime_ns: row.get(1)?,
                    size: row.get(2)?,
                },
            ))
        })
        .map_err(|e| format!("Read source state: {}", e))?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

fn previous_session_states(
    conn: &Connection,
) -> Result<HashMap<String, SessionIndexState>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT username, last_timestamp, sort_timestamp, last_msg_local_id, last_msg_type
             FROM session_states",
        )
        .map_err(|e| format!("Prepare session state query: {}", e))?;
    let rows = stmt
        .query_map([], |row| {
            let state = SessionIndexState {
                username: row.get(0)?,
                last_timestamp: row.get(1)?,
                sort_timestamp: row.get(2)?,
                last_msg_local_id: row.get(3)?,
                last_msg_type: row.get(4)?,
            };
            Ok((state.username.clone(), state))
        })
        .map_err(|e| format!("Read session state: {}", e))?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

fn insert_session_states(conn: &Connection, states: &[SessionIndexState]) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "INSERT OR REPLACE INTO session_states (
                username, last_timestamp, sort_timestamp, last_msg_local_id, last_msg_type
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .map_err(|e| format!("Prepare session state insert: {}", e))?;
    for state in states {
        stmt.execute(params![
            state.username,
            state.last_timestamp,
            state.sort_timestamp,
            state.last_msg_local_id,
            state.last_msg_type
        ])
        .map_err(|e| format!("Insert session state {}: {}", state.username, e))?;
    }
    Ok(())
}

fn sync_current_session_states(conn: &mut Connection, decrypted_dir: &Path) -> Result<(), String> {
    let Some(session_db) = db::find_decrypted_session_db(decrypted_dir) else {
        return Ok(());
    };
    let states = load_session_index_states(&session_db)?;
    let current = states
        .iter()
        .map(|state| (state.username.clone(), state.clone()))
        .collect::<HashMap<_, _>>();
    if previous_session_states(conn)? == current {
        return Ok(());
    }

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("Start session state sync: {}", e))?;
    tx.execute("DELETE FROM session_states", [])
        .map_err(|e| format!("Clear session state: {}", e))?;
    insert_session_states(&tx, &states)?;
    tx.commit()
        .map_err(|e| format!("Commit session state sync: {}", e))?;
    Ok(())
}

fn load_session_index_states(session_db: &Path) -> Result<Vec<SessionIndexState>, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(session_db, flags)
        .map_err(|e| format!("Cannot open session db {}: {}", session_db.display(), e))?;
    let has_last_local_id = db::table_has_column(&conn, "SessionTable", "last_msg_locald_id");
    let has_last_type = db::table_has_column(&conn, "SessionTable", "last_msg_type");
    let last_local_id_col = if has_last_local_id {
        "last_msg_locald_id"
    } else {
        "NULL"
    };
    let last_type_col = if has_last_type {
        "last_msg_type"
    } else {
        "NULL"
    };
    let sql = format!(
        "SELECT username,
                COALESCE(last_timestamp, 0),
                COALESCE(sort_timestamp, 0),
                {last_local_id_col},
                {last_type_col}
         FROM SessionTable
         WHERE username IS NOT NULL
           AND username != ''
           AND (COALESCE(last_timestamp, 0) > 0
                OR COALESCE(sort_timestamp, 0) > 0
                OR {last_local_id_col} IS NOT NULL)"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("Prepare session table scan: {}", e))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SessionIndexState {
                username: row.get(0)?,
                last_timestamp: row.get(1)?,
                sort_timestamp: row.get(2)?,
                last_msg_local_id: row.get(3)?,
                last_msg_type: row.get(4)?,
            })
        })
        .map_err(|e| format!("Read session table: {}", e))?;
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

fn contact_signature(contacts: &HashMap<String, contact::Contact>) -> String {
    let mut entries = contacts
        .values()
        .map(|contact| row_identity::ContactFingerprintEntry {
            account: &contact.username,
            nick_name: &contact.nick_name,
            remark: &contact.remark,
            alias: &contact.alias,
            is_stranger: contact.is_stranger,
        })
        .collect::<Vec<_>>();
    row_identity::contact_fingerprint(&mut entries).to_string()
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

fn index_schema_version(conn: &Connection) -> Result<i64, String> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|e| format!("Read index schema version: {}", e))
}

fn is_supported_schema_version(version: i64) -> bool {
    matches!(version, SCHEMA_VERSION | 7 | 6 | 5 | 4 | 3 | 2)
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

    fn create_session_db(dir: &Path, rows: &[(&str, i64, i64, i64, i64)]) {
        let session_dir = dir.join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        let conn = Connection::open(session_dir.join("session.db")).unwrap();
        conn.execute(
            "CREATE TABLE SessionTable (
                username TEXT PRIMARY KEY,
                last_timestamp INTEGER,
                sort_timestamp INTEGER,
                last_msg_locald_id INTEGER,
                last_msg_type INTEGER
            )",
            [],
        )
        .unwrap();
        for (username, last_timestamp, sort_timestamp, local_id, msg_type) in rows {
            conn.execute(
                "INSERT INTO SessionTable (
                    username, last_timestamp, sort_timestamp, last_msg_locald_id, last_msg_type
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![username, last_timestamp, sort_timestamp, local_id, msg_type],
            )
            .unwrap();
        }
    }

    fn update_session_last_message(dir: &Path, username: &str, local_id: i64, last_timestamp: i64) {
        let conn = Connection::open(dir.join("session/session.db")).unwrap();
        conn.execute(
            "UPDATE SessionTable
             SET last_timestamp = ?2, sort_timestamp = ?2, last_msg_locald_id = ?3
             WHERE username = ?1",
            params![username, last_timestamp, local_id],
        )
        .unwrap();
    }

    fn create_message_db(dir: &Path, rows: &[(&str, i64, i64, &str)]) -> PathBuf {
        let message_dir = dir.join("message");
        std::fs::create_dir_all(&message_dir).unwrap();
        let message_path = message_dir.join("message_0.db");
        let conn = Connection::open(&message_path).unwrap();
        conn.execute("CREATE TABLE Name2Id (user_name TEXT)", [])
            .unwrap();
        let body_col = db::quote_identifier(dictionary::msg_body_column());
        let marker_col = db::quote_identifier(dictionary::msg_compression_marker_column());
        let sender_col = db::quote_identifier(dictionary::msg_sender_column());
        let packed_col = db::quote_identifier(dictionary::msg_packed_meta_column());
        for (username, local_id, create_time, body) in rows {
            let table = db::msg_table_name(username);
            conn.execute(
                &format!(
                    "CREATE TABLE IF NOT EXISTS {} (
                        local_id INTEGER,
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
            conn.execute(
                &format!(
                    "INSERT INTO {} (local_id, local_type, create_time, {}, {}, {}, {})
                     VALUES (?1, 1, ?2, ?3, NULL, 0, x'')",
                    table, body_col, marker_col, sender_col, packed_col
                ),
                params![local_id, create_time, body],
            )
            .unwrap();
        }
        message_path
    }

    fn update_message_body(message_path: &Path, username: &str, local_id: i64, body: &str) {
        let conn = Connection::open(message_path).unwrap();
        let table = db::msg_table_name(username);
        let body_col = db::quote_identifier(dictionary::msg_body_column());
        conn.execute(
            &format!("UPDATE {} SET {} = ?1 WHERE local_id = ?2", table, body_col),
            params![body, local_id],
        )
        .unwrap();
    }

    #[test]
    fn builds_recent_index_for_numbered_message_dbs() {
        let dir = tempdir().unwrap();
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();
        let conn = Connection::open(message_dir.join("message_0.db")).unwrap();
        let table = db::msg_table_name("tgid_indexed");
        let body_col = db::quote_identifier(dictionary::msg_body_column());
        let marker_col = db::quote_identifier(dictionary::msg_compression_marker_column());
        let sender_col = db::quote_identifier(dictionary::msg_sender_column());
        let packed_col = db::quote_identifier(dictionary::msg_packed_meta_column());
        conn.execute(
            &format!(
                "CREATE TABLE {} (
                    local_id INTEGER,
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
                "INSERT INTO {} (local_id, local_type, create_time, {}, {}, {}, {})
                 VALUES (NULL, 1, ?1, 'indexed needle', NULL, 0, x'')",
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
        let media_type: String = indexed
            .query_row("SELECT media_type FROM messages", [], |row| row.get(0))
            .unwrap();

        assert_eq!(count, 1);
        assert_eq!(media_type, "");
        assert!(open_existing_recent(dir.path()).unwrap().is_some());

        std::fs::copy(
            message_dir.join("message_0.db"),
            message_dir.join("message_1.db"),
        )
        .unwrap();
        assert!(open_existing_recent(dir.path()).unwrap().is_none());
    }

    #[test]
    fn contact_mtime_changes_do_not_block_existing_index_open() {
        let dir = tempdir().unwrap();
        let contact_dir = dir.path().join("contact");
        std::fs::create_dir_all(&contact_dir).unwrap();
        let contact_path = contact_dir.join("contact.db");
        let contact_conn = Connection::open(&contact_path).unwrap();
        contact_conn
            .execute(
                "CREATE TABLE contact (
                    username TEXT,
                    nick_name TEXT,
                    remark TEXT,
                    alias TEXT
                )",
                [],
            )
            .unwrap();
        contact_conn
            .execute(
                "INSERT INTO contact (username, nick_name, remark, alias)
                 VALUES ('tgid_indexed', 'Indexed Nick', '', '')",
                [],
            )
            .unwrap();
        drop(contact_conn);

        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();
        let conn = Connection::open(message_dir.join("message_0.db")).unwrap();
        let table = db::msg_table_name("tgid_indexed");
        let body_col = db::quote_identifier(dictionary::msg_body_column());
        let marker_col = db::quote_identifier(dictionary::msg_compression_marker_column());
        let sender_col = db::quote_identifier(dictionary::msg_sender_column());
        let packed_col = db::quote_identifier(dictionary::msg_packed_meta_column());
        conn.execute(
            &format!(
                "CREATE TABLE {} (
                    local_id INTEGER,
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
                "INSERT INTO {} (local_id, local_type, create_time, {}, {}, {}, {})
                 VALUES (1, 1, ?1, 'indexed needle', NULL, 0, x'')",
                table, body_col, marker_col, sender_col, packed_col
            ),
            params![time::default_recent_since() + 1],
        )
        .unwrap();
        drop(conn);

        ensure_recent(dir.path(), 1).unwrap();
        assert!(open_existing_recent(dir.path()).unwrap().is_some());

        let bytes = std::fs::read(&contact_path).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        std::fs::write(&contact_path, bytes).unwrap();
        assert!(open_existing_recent(dir.path()).unwrap().is_some());

        let contact_conn = Connection::open(&contact_path).unwrap();
        contact_conn
            .execute(
                "UPDATE contact SET remark = 'Changed Remark' WHERE username = 'tgid_indexed'",
                [],
            )
            .unwrap();
        drop(contact_conn);
        assert!(open_existing_recent(dir.path()).unwrap().is_some());
    }

    #[test]
    fn local_id_cursor_refresh_appends_new_rows_without_duplication() {
        let dir = tempdir().unwrap();
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();
        let message_path = message_dir.join("message_0.db");
        let conn = Connection::open(&message_path).unwrap();
        let table = db::msg_table_name("tgid_cursor");
        let body_col = db::quote_identifier(dictionary::msg_body_column());
        let marker_col = db::quote_identifier(dictionary::msg_compression_marker_column());
        let sender_col = db::quote_identifier(dictionary::msg_sender_column());
        let packed_col = db::quote_identifier(dictionary::msg_packed_meta_column());
        conn.execute(
            &format!(
                "CREATE TABLE {} (
                    local_id INTEGER,
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
                "INSERT INTO {} (local_id, local_type, create_time, {}, {}, {}, {})
                 VALUES (1, 1, ?1, 'old cursor row', NULL, 0, x'')",
                table, body_col, marker_col, sender_col, packed_col
            ),
            params![time::default_recent_since() + 10],
        )
        .unwrap();
        drop(conn);

        let index = ensure_recent(dir.path(), 1).unwrap();
        let indexed = Connection::open(&index.path).unwrap();
        let initial_count: i64 = indexed
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        let initial_cursor: i64 = indexed
            .query_row("SELECT max_local_id FROM table_states", [], |row| {
                row.get(0)
            })
            .unwrap();
        drop(indexed);

        assert_eq!(initial_count, 1);
        assert_eq!(initial_cursor, 1);

        std::thread::sleep(Duration::from_millis(2));
        let conn = Connection::open(&message_path).unwrap();
        conn.execute(
            &format!(
                "INSERT INTO {} (local_id, local_type, create_time, {}, {}, {}, {})
                 VALUES (2, 1, ?1, 'new cursor row', NULL, 0, x'')",
                table, body_col, marker_col, sender_col, packed_col
            ),
            params![time::default_recent_since() + 20],
        )
        .unwrap();
        drop(conn);

        let index = ensure_recent(dir.path(), 1).unwrap();
        let indexed = Connection::open(index.path).unwrap();
        let (count, old_count, new_count, cursor): (i64, i64, i64, i64) = indexed
            .query_row(
                "SELECT
                    COUNT(*),
                    SUM(CASE WHEN raw_body = 'old cursor row' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN raw_body = 'new cursor row' THEN 1 ELSE 0 END),
                    (SELECT max_local_id FROM table_states)
                 FROM messages",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();

        assert_eq!(count, 2);
        assert_eq!(old_count, 1);
        assert_eq!(new_count, 1);
        assert_eq!(cursor, 2);
    }

    #[test]
    fn contact_change_does_not_promote_cursor_refresh_to_full_window() {
        let dir = tempdir().unwrap();
        let contact_dir = dir.path().join("contact");
        std::fs::create_dir_all(&contact_dir).unwrap();
        let contact_path = contact_dir.join("contact.db");
        let contact_conn = Connection::open(&contact_path).unwrap();
        contact_conn
            .execute(
                "CREATE TABLE contact (
                    username TEXT,
                    nick_name TEXT,
                    remark TEXT,
                    alias TEXT
                )",
                [],
            )
            .unwrap();
        contact_conn
            .execute(
                "INSERT INTO contact (username, nick_name, remark, alias)
                 VALUES ('tgid_contact_cursor', 'Before', '', '')",
                [],
            )
            .unwrap();
        drop(contact_conn);

        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();
        let message_path = message_dir.join("message_0.db");
        let conn = Connection::open(&message_path).unwrap();
        let table = db::msg_table_name("tgid_contact_cursor");
        let body_col = db::quote_identifier(dictionary::msg_body_column());
        let marker_col = db::quote_identifier(dictionary::msg_compression_marker_column());
        let sender_col = db::quote_identifier(dictionary::msg_sender_column());
        let packed_col = db::quote_identifier(dictionary::msg_packed_meta_column());
        conn.execute(
            &format!(
                "CREATE TABLE {} (
                    local_id INTEGER,
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
                "INSERT INTO {} (local_id, local_type, create_time, {}, {}, {}, {})
                 VALUES (1, 1, ?1, 'original old row', NULL, 0, x'')",
                table, body_col, marker_col, sender_col, packed_col
            ),
            params![time::default_recent_since() + 10],
        )
        .unwrap();
        drop(conn);

        let index = ensure_recent(dir.path(), 1).unwrap();
        drop(index);

        let contact_conn = Connection::open(&contact_path).unwrap();
        contact_conn
            .execute(
                "UPDATE contact SET remark = 'After' WHERE username = 'tgid_contact_cursor'",
                [],
            )
            .unwrap();
        drop(contact_conn);

        std::thread::sleep(Duration::from_millis(2));
        let conn = Connection::open(&message_path).unwrap();
        conn.execute(
            &format!(
                "UPDATE {} SET {} = 'mutated old row' WHERE local_id = 1",
                table, body_col
            ),
            [],
        )
        .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO {} (local_id, local_type, create_time, {}, {}, {}, {})
                 VALUES (2, 1, ?1, 'new row after contact change', NULL, 0, x'')",
                table, body_col, marker_col, sender_col, packed_col
            ),
            params![time::default_recent_since() + 20],
        )
        .unwrap();
        drop(conn);

        let index = ensure_recent(dir.path(), 1).unwrap();
        let indexed = Connection::open(index.path).unwrap();
        let (original_count, mutated_count, new_count): (i64, i64, i64) = indexed
            .query_row(
                "SELECT
                    SUM(CASE WHEN raw_body = 'original old row' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN raw_body = 'mutated old row' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN raw_body = 'new row after contact change' THEN 1 ELSE 0 END)
                 FROM messages",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert_eq!(original_count, 1);
        assert_eq!(mutated_count, 0);
        assert_eq!(new_count, 1);
    }

    #[test]
    fn rebuilds_old_schema_and_indexes_decoded_body() {
        let dir = tempdir().unwrap();
        let index_path = dir.path().join(INDEX_FILE);
        let old = Connection::open(&index_path).unwrap();
        old.execute_batch(
            "PRAGMA user_version = 2;
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE source_files (path TEXT PRIMARY KEY, mtime_ns INTEGER NOT NULL, size INTEGER NOT NULL);
             CREATE TABLE messages (
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
             INSERT INTO meta (key, value) VALUES ('index_since', '1');
             INSERT INTO messages (
                 source_db, table_name, session_id, session_display,
                 sender_account, sender_display, local_id, local_type,
                 create_time, body, marker, packed_info
             ) VALUES (
                 'old', 'old', 'old', 'old', '', '', NULL, 1, 1, 'stale', NULL, x''
             );",
        )
        .unwrap();
        drop(old);
        assert!(open_existing_recent(dir.path()).unwrap().is_some());

        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();
        let conn = Connection::open(message_dir.join("message_0.db")).unwrap();
        let table = db::msg_table_name("tgid_voice");
        let body_col = db::quote_identifier(dictionary::msg_body_column());
        let marker_col = db::quote_identifier(dictionary::msg_compression_marker_column());
        let sender_col = db::quote_identifier(dictionary::msg_sender_column());
        let packed_col = db::quote_identifier(dictionary::msg_packed_meta_column());
        conn.execute(
            &format!(
                "CREATE TABLE {} (
                    local_id INTEGER,
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
                "INSERT INTO {} (local_id, local_type, create_time, {}, {}, {}, {})
                 VALUES (42, 34, ?1, '', NULL, 0, x'')",
                table, body_col, marker_col, sender_col, packed_col
            ),
            params![time::default_recent_since() + 1],
        )
        .unwrap();
        drop(conn);

        let index = ensure_recent(dir.path(), 1).unwrap();
        let indexed = Connection::open(index.path).unwrap();
        let version: i64 = indexed
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let (raw_body, decoded_body, media_type): (String, String, String) = indexed
            .query_row(
                "SELECT raw_body, decoded_body, media_type FROM messages",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert_eq!(version, SCHEMA_VERSION);
        assert!(!db::table_has_column(&indexed, "messages", "body"));
        assert_eq!(raw_body, "");
        assert_eq!(decoded_body, "[voice:42]");
        assert_eq!(media_type, "voice");
    }

    #[test]
    fn open_existing_recent_returns_none_without_index_file() {
        let dir = tempdir().unwrap();

        let index = open_existing_recent(dir.path()).unwrap();

        assert!(index.is_none());
    }

    #[test]
    fn unchanged_session_table_produces_no_session_refresh() {
        let dir = tempdir().unwrap();
        let since = time::default_recent_since() + 10;
        create_session_db(dir.path(), &[("tgid_state_a", since, since, 1, 1)]);
        let message_path = create_message_db(dir.path(), &[("tgid_state_a", 1, since, "first")]);

        let initial = ensure_recent_sessions(dir.path(), 1).unwrap();
        update_message_body(&message_path, "tgid_state_a", 1, "mutated without session");
        let second = ensure_recent_sessions(dir.path(), 1).unwrap();
        let indexed = Connection::open(dir.path().join(INDEX_FILE)).unwrap();
        let body: String = indexed
            .query_row("SELECT raw_body FROM messages", [], |row| row.get(0))
            .unwrap();

        assert_eq!(
            initial,
            SessionCatchUpStats {
                changed_sessions: 1,
                refreshed_tables: 1
            }
        );
        assert_eq!(second, SessionCatchUpStats::default());
        assert_eq!(body, "first");
    }

    #[test]
    fn changed_session_refreshes_only_that_session_table() {
        let dir = tempdir().unwrap();
        let since = time::default_recent_since() + 10;
        create_session_db(
            dir.path(),
            &[
                ("tgid_state_a", since, since, 1, 1),
                ("tgid_state_b", since, since, 1, 1),
            ],
        );
        let message_path = create_message_db(
            dir.path(),
            &[
                ("tgid_state_a", 1, since, "a first"),
                ("tgid_state_b", 1, since, "b first"),
            ],
        );
        ensure_recent_sessions(dir.path(), 1).unwrap();

        update_message_body(&message_path, "tgid_state_a", 1, "a changed");
        update_message_body(&message_path, "tgid_state_b", 1, "b changed");
        update_session_last_message(dir.path(), "tgid_state_a", 2, since + 1);
        let stats = ensure_recent_sessions(dir.path(), 1).unwrap();

        let indexed = Connection::open(dir.path().join(INDEX_FILE)).unwrap();
        let a_count: i64 = indexed
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE raw_body = 'a changed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let b_old_count: i64 = indexed
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE raw_body = 'b first'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let b_changed_count: i64 = indexed
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE raw_body = 'b changed'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(
            stats,
            SessionCatchUpStats {
                changed_sessions: 1,
                refreshed_tables: 1
            }
        );
        assert_eq!(a_count, 1);
        assert_eq!(b_old_count, 1);
        assert_eq!(b_changed_count, 0);
    }

    #[test]
    fn explicit_recent_refresh_remains_source_driven_after_session_catch_up() {
        let dir = tempdir().unwrap();
        let since = time::default_recent_since() + 10;
        create_session_db(dir.path(), &[("tgid_source_a", since, since, 1, 1)]);
        create_message_db(
            dir.path(),
            &[
                ("tgid_source_a", 1, since, "session tracked"),
                ("tgid_source_b", 1, since, "source only"),
            ],
        );

        ensure_recent_sessions(dir.path(), 1).unwrap();
        let indexed = Connection::open(dir.path().join(INDEX_FILE)).unwrap();
        let source_states_after_catch_up: i64 = indexed
            .query_row("SELECT COUNT(*) FROM source_files", [], |row| row.get(0))
            .unwrap();
        let source_only_after_catch_up: i64 = indexed
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE raw_body = 'source only'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(indexed);

        ensure_recent(dir.path(), 1).unwrap();
        let catch_up_after_real_refresh = ensure_recent_sessions(dir.path(), 1).unwrap();
        let indexed = Connection::open(dir.path().join(INDEX_FILE)).unwrap();
        let source_states_after_real_refresh: i64 = indexed
            .query_row("SELECT COUNT(*) FROM source_files", [], |row| row.get(0))
            .unwrap();
        let source_only_after_real_refresh: i64 = indexed
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE raw_body = 'source only'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(source_states_after_catch_up, 0);
        assert_eq!(source_only_after_catch_up, 0);
        assert_eq!(catch_up_after_real_refresh, SessionCatchUpStats::default());
        assert_eq!(source_states_after_real_refresh, 1);
        assert_eq!(source_only_after_real_refresh, 1);
    }
}
