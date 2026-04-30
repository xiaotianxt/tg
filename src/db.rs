use md5::{Digest, Md5};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::dictionary;
use crate::message;
use crate::parallel;
use crate::time;

/// Resolve a sender ID to a display name from contacts.
pub fn resolve_sender_name(sender_id: &str, contacts: &HashMap<String, Contact>) -> String {
    resolve_sender_name_with_mode(
        sender_id,
        contacts,
        DisplayNameMode::PersonalRemark,
        &HashMap::new(),
    )
}

fn resolve_sender_name_with_mode(
    sender_id: &str,
    contacts: &HashMap<String, Contact>,
    mode: DisplayNameMode,
    room_member_names: &HashMap<String, String>,
) -> String {
    if mode == DisplayNameMode::Anonymous {
        if let Some(name) = room_member_names
            .get(sender_id)
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
        {
            return name.to_string();
        }
    }

    contacts
        .get(sender_id)
        .map(|c| c.display_name(mode))
        .unwrap_or(sender_id)
        .to_string()
}

/// Find decrypted databases.
pub(crate) fn find_decrypted_dbs(decrypted_dir: &Path) -> (Option<PathBuf>, Vec<PathBuf>) {
    // Find contact.db
    let contact_db = decrypted_dir.join("contact/contact.db");
    let contact_db = if contact_db.exists() {
        Some(contact_db)
    } else {
        // Try alternative location
        let alt = decrypted_dir
            .parent()
            .map(|p| p.join("decrypted/contact/contact.db"))
            .filter(|p| p.exists());
        alt
    };

    // Find message databases
    let msg_dir = decrypted_dir.join("message");
    let mut message_dbs = Vec::new();
    if msg_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&msg_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("db") {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if is_message_db_name(name) {
                        message_dbs.push(path);
                    }
                }
            }
        }
    }
    message_dbs.sort();

    (contact_db, message_dbs)
}

pub(crate) fn is_message_db_name(name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix("message_")
        .and_then(|s| s.strip_suffix(".db"))
    else {
        return false;
    };

    !stem.is_empty() && stem.chars().all(|c| c.is_ascii_digit())
}

/// Contact info.
pub(crate) struct Contact {
    pub username: String,
    pub nick_name: String,
    pub remark: String,
    pub alias: String,
    pub display: String,
}

impl Contact {
    fn display_name(&self, mode: DisplayNameMode) -> &str {
        match mode {
            DisplayNameMode::PersonalRemark => first_non_empty([&self.display, &self.username]),
            DisplayNameMode::Anonymous => first_non_empty([&self.nick_name, &self.username]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayNameMode {
    PersonalRemark,
    Anonymous,
}

#[derive(Default)]
struct SessionInfo {
    count: i64,
    earliest: Option<i64>,
    latest: Option<i64>,
}

struct SessionListEntry {
    username: String,
    info: SessionInfo,
    display: String,
    match_info: Option<SessionMatchInfo>,
}

#[derive(Clone, Copy)]
struct SessionMatchInfo {
    score: i64,
    field_priority: usize,
    distance: usize,
    field_len: usize,
}

type MessageRow = (i64, i64, String, Option<i64>, String, Vec<u8>);
type SearchRow = (i64, i64, String, String);
const SESSION_TIME_BOUNDARY_ROWS: usize = 128;
const SEARCH_PREVIEW_CHARS: usize = 100;
const TELEGRAM_FTS_CONTENT_TABLE_PREFIX: &str = "message_fts_v4_";
const TELEGRAM_FTS_CONTENT_TABLE_SUFFIX: &str = "_content";

pub(crate) struct ReadMessagesOptions<'a> {
    pub session_query: &'a str,
    pub limit: Option<usize>,
    pub offset: usize,
    pub search_query: Option<&'a str>,
    pub since: Option<i64>,
    pub tail: bool,
    pub time_bucket: time::MessageTimeBucket,
    pub name_mode: DisplayNameMode,
    pub jobs: usize,
}

pub(crate) struct SessionProbe {
    pub username: String,
    pub display_name: String,
    pub table_name: String,
    pub matching_dbs: usize,
    pub message_count: usize,
}

pub(crate) struct SearchMessagesOptions<'a> {
    pub query: &'a str,
    pub limit: usize,
    pub since: Option<i64>,
    pub use_telegram_fts: bool,
    pub jobs: usize,
}

pub(crate) fn load_contacts(contact_db: &Path) -> Result<HashMap<String, Contact>, String> {
    let conn =
        Connection::open(contact_db).map_err(|e| format!("Cannot open contact DB: {}", e))?;

    let mut stmt = conn
        .prepare("SELECT username, nick_name, remark, alias FROM contact")
        .map_err(|e| format!("Contact query error: {}", e))?;

    let contacts: HashMap<String, Contact> = stmt
        .query_map([], |row| {
            let username: String = row.get(0)?;
            let nick_name: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
            let remark: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
            let alias: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
            let display = if !remark.is_empty() {
                remark.clone()
            } else {
                nick_name.clone()
            };
            Ok((
                username.clone(),
                Contact {
                    username,
                    nick_name,
                    remark,
                    alias,
                    display,
                },
            ))
        })
        .map_err(|e| format!("Contact read error: {}", e))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(contacts)
}

fn first_non_empty<const N: usize>(values: [&str; N]) -> &str {
    values
        .into_iter()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or("")
}

fn load_chat_room_member_names(
    contact_db: &Path,
    room_username: &str,
) -> Result<HashMap<String, String>, String> {
    let conn =
        Connection::open(contact_db).map_err(|e| format!("Cannot open contact DB: {}", e))?;
    let ext_buffer: Option<Vec<u8>> = conn
        .query_row(
            "SELECT ext_buffer FROM chat_room WHERE username = ?1",
            params![room_username],
            |row| row.get::<_, Option<Vec<u8>>>(0),
        )
        .optional()
        .map_err(|e| format!("Chat room member query error: {}", e))?
        .flatten();

    Ok(ext_buffer
        .as_deref()
        .map(parse_chat_room_member_names)
        .unwrap_or_default())
}

fn parse_chat_room_member_names(data: &[u8]) -> HashMap<String, String> {
    use crate::media_pb::wire::{decode_varint, skip_field, tag_field};

    let mut names = HashMap::new();
    let mut pos = 0;
    while pos < data.len() {
        let Some(tag) = decode_varint(data, &mut pos) else {
            break;
        };
        let (field, wire) = tag_field(tag);
        if (field, wire) != (1, 2) {
            if skip_field(data, &mut pos, wire).is_none() {
                break;
            }
            continue;
        }

        let Some(len) = decode_varint(data, &mut pos).map(|len| len as usize) else {
            break;
        };
        let Some(end) = pos.checked_add(len) else {
            break;
        };
        let Some(member) = data.get(pos..end) else {
            break;
        };
        pos = end;

        if let Some((username, display_name)) = parse_chat_room_member_name(member) {
            names.insert(username, display_name);
        }
    }
    names
}

fn parse_chat_room_member_name(data: &[u8]) -> Option<(String, String)> {
    use crate::media_pb::wire::{decode_string, decode_varint, skip_field, tag_field};

    let mut username = String::new();
    let mut display_name = String::new();
    let mut pos = 0;
    while pos < data.len() {
        let tag = decode_varint(data, &mut pos)?;
        let (field, wire) = tag_field(tag);
        match (field, wire) {
            (1, 2) => username = decode_string(data, &mut pos)?,
            (2, 2) => display_name = decode_string(data, &mut pos)?,
            _ => skip_field(data, &mut pos, wire)?,
        }
    }

    let username = username.trim();
    let display_name = display_name.trim();
    if username.is_empty() || display_name.is_empty() {
        return None;
    }
    Some((username.to_string(), display_name.to_string()))
}

pub(crate) fn msg_table_name(username: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(username.as_bytes());
    let hash = hasher.finalize();
    format!("Msg_{:x}", hash)
}

pub(crate) fn quote_identifier(name: &str) -> String {
    let mut quoted = String::with_capacity(name.len() + 2);
    quoted.push('"');
    for ch in name.chars() {
        if ch == '"' {
            quoted.push('"');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    quoted
}

fn session_stats_for_table(conn: &Connection, table_name: &str) -> Option<SessionInfo> {
    let table = quote_identifier(table_name);
    let fast_sql = format!(
        "SELECT \
            (SELECT COUNT(*) FROM {table}), \
            (SELECT MIN(create_time) FROM (SELECT create_time FROM {table} WHERE create_time > 0 ORDER BY sort_seq ASC LIMIT {SESSION_TIME_BOUNDARY_ROWS})), \
            (SELECT MAX(create_time) FROM (SELECT create_time FROM {table} WHERE create_time > 0 ORDER BY sort_seq DESC LIMIT {SESSION_TIME_BOUNDARY_ROWS}))"
    );

    // Telegram message tables have a SORTSEQ index. These boundary lookups avoid
    // full-table MIN/MAX(create_time) scans on large chats while tolerating
    // small same-second ordering differences near each edge.
    if let Ok((count, earliest, latest)) = conn.query_row(&fast_sql, [], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<i64>>(2)?,
        ))
    }) {
        if count > 0 && (earliest.is_some() || latest.is_some()) {
            return Some(SessionInfo {
                count,
                earliest,
                latest,
            });
        }
        return None;
    }

    let exact_sql = format!(
        "SELECT COUNT(*), MIN(create_time), MAX(create_time) FROM {} WHERE create_time > 0",
        table
    );
    let (count, earliest, latest) = conn
        .query_row(&exact_sql, [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })
        .ok()?;
    (count > 0).then_some(SessionInfo {
        count,
        earliest,
        latest,
    })
}

/// List all sessions/messages with counts.
pub fn list_sessions(
    decrypted_dir: &Path,
    top_n: usize,
    query: Option<&str>,
    jobs: usize,
) -> Result<Vec<(String, i64, String, String)>, String> {
    let (contact_db, message_dbs) = find_decrypted_dbs(decrypted_dir);

    // Load contacts
    let contacts = match &contact_db {
        Some(path) => load_contacts(path).unwrap_or_default(),
        None => HashMap::new(),
    };

    // Map: table_name -> SessionInfo
    // Also track: table_name -> contact username
    let mut table_to_username: HashMap<String, String> = HashMap::new();
    for (username, contact) in &contacts {
        let table = msg_table_name(username);
        table_to_username.insert(table, username.clone());
        // Also store by display name
        let display_table = msg_table_name(&contact.display);
        table_to_username
            .entry(display_table)
            .or_insert_with(|| username.clone());
    }

    let db_jobs = parallel::job_count(jobs, 8);
    let per_db_sessions = parallel::map_ordered(message_dbs.clone(), db_jobs, |db_path| {
        let mut sessions: HashMap<String, SessionInfo> = HashMap::new();
        let conn = match Connection::open(&db_path) {
            Ok(c) => c,
            Err(_) => return sessions,
        };

        // Find all Msg_ tables
        let tables: Vec<String> = match conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'")
        {
            Ok(mut stmt) => match stmt.query_map([], |row| row.get::<_, String>(0)) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(_) => vec![],
            },
            Err(_) => vec![],
        };

        for table_name in &tables {
            if let Some(table_info) = session_stats_for_table(&conn, table_name) {
                let info = sessions.entry(table_name.clone()).or_default();
                info.count += table_info.count;
                if table_info
                    .earliest
                    .is_none_or(|e| info.earliest.is_none_or(|ie| e < ie))
                {
                    info.earliest = table_info.earliest;
                }
                if table_info
                    .latest
                    .is_none_or(|l| info.latest.is_none_or(|il| l > il))
                {
                    info.latest = table_info.latest;
                }
            }
        }
        sessions
    });

    let mut sessions: HashMap<String, SessionInfo> = HashMap::new();
    for db_sessions in per_db_sessions {
        for (table_name, db_info) in db_sessions {
            let info = sessions.entry(table_name).or_default();
            info.count += db_info.count;
            if db_info
                .earliest
                .is_none_or(|e| info.earliest.is_none_or(|ie| e < ie))
            {
                info.earliest = db_info.earliest;
            }
            if db_info
                .latest
                .is_none_or(|l| info.latest.is_none_or(|il| l > il))
            {
                info.latest = db_info.latest;
            }
        }
    }

    // Sort by message count
    let mut sorted: Vec<_> = sessions.into_iter().collect();
    sorted.sort_by(|a, b| b.1.count.cmp(&a.1.count));

    if sorted.is_empty() {
        return Ok(Vec::new());
    }
    let total_sessions = sorted.len();

    let mut entries: Vec<_> = sorted
        .into_iter()
        .map(|(table, info)| {
            let username = table_to_username
                .get(&table)
                .cloned()
                .unwrap_or_else(|| table.clone());
            let display = contacts
                .get(&username)
                .map(|c| c.display.clone())
                .unwrap_or_else(|| "(?)".to_string());

            SessionListEntry {
                username,
                info,
                display,
                match_info: None,
            }
        })
        .collect();

    let query = query.map(str::trim).filter(|value| !value.is_empty());
    if let Some(query) = query {
        entries = entries
            .into_iter()
            .filter_map(|mut entry| {
                let match_info =
                    session_match_info(&contacts, &entry.username, &entry.display, query)?;
                entry.match_info = Some(match_info);
                Some(entry)
            })
            .collect();

        entries.sort_by(|a, b| {
            let a_match = a.match_info.expect("filtered session entry has match info");
            let b_match = b.match_info.expect("filtered session entry has match info");
            a_match
                .field_priority
                .cmp(&b_match.field_priority)
                .then_with(|| b_match.score.cmp(&a_match.score))
                .then_with(|| {
                    b.info
                        .latest
                        .unwrap_or_default()
                        .cmp(&a.info.latest.unwrap_or_default())
                })
                .then_with(|| a_match.distance.cmp(&b_match.distance))
                .then_with(|| a_match.field_len.cmp(&b_match.field_len))
                .then_with(|| b.info.count.cmp(&a.info.count))
                .then_with(|| a.display.cmp(&b.display))
                .then_with(|| a.username.cmp(&b.username))
        });
    }

    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let matched_sessions = entries.len();

    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());

    out.line(format_args!(
        "{:<4} {:<8} {:<46} {:<22} Username",
        "Rank", "Count", "Time Range", "Display Name"
    ))?;
    out.line(format_args!("{}", "-".repeat(120)))?;

    let mut result = Vec::new();

    for (i, entry) in entries.iter().enumerate().take(top_n) {
        let time_range = match (entry.info.earliest, entry.info.latest) {
            (Some(e), Some(l)) => {
                let e_ts = time::format_local_timestamp_minutes(e);
                let l_ts = time::format_local_timestamp_minutes(l);
                format!("{} ~ {}", e_ts, l_ts)
            }
            _ => String::new(),
        };

        out.line(format_args!(
            "{:<4} {:<8} {:<46} {:<22} {}",
            i + 1,
            entry.info.count,
            time_range,
            entry.display,
            entry.username
        ))?;

        result.push((
            entry.username.clone(),
            entry.info.count,
            time_range,
            entry.display.clone(),
        ));
    }

    out.blank_line()?;
    if query.is_some() {
        out.line(format_args!(
            "Matched: {} of {} sessions",
            matched_sessions, total_sessions
        ))?;
    } else {
        out.line(format_args!("Total: {} sessions", total_sessions))?;
    }
    out.flush()?;
    Ok(result)
}

fn session_match_info(
    contacts: &HashMap<String, Contact>,
    username: &str,
    display: &str,
    query: &str,
) -> Option<SessionMatchInfo> {
    if let Some(contact) = contacts.get(username) {
        return best_contact_score(contact, query).map(
            |(score, field_priority, distance, field_len)| SessionMatchInfo {
                score,
                field_priority,
                distance,
                field_len,
            },
        );
    }

    let normalized_query = normalize_match_text(query);
    if normalized_query.is_empty() {
        return None;
    }
    let query_tokens = tokenize_match_text(query);

    [(0, display), (4, username)]
        .iter()
        .filter_map(|(priority, field)| {
            field_score(field, query, &normalized_query, &query_tokens).map(
                |(score, distance, field_len)| SessionMatchInfo {
                    score,
                    field_priority: *priority,
                    distance,
                    field_len,
                },
            )
        })
        .max_by(|a, b| {
            b.field_priority
                .cmp(&a.field_priority)
                .then_with(|| a.score.cmp(&b.score))
                .then_with(|| b.distance.cmp(&a.distance))
                .then_with(|| b.field_len.cmp(&a.field_len))
        })
}

/// Read messages from a specific session.
pub fn read_messages(
    decrypted_dir: &Path,
    options: ReadMessagesOptions<'_>,
) -> Result<usize, String> {
    let (contact_db, message_dbs) = find_decrypted_dbs(decrypted_dir);

    let username = resolve_username_for_messages(
        options.session_query,
        contact_db.as_deref(),
        &message_dbs,
        options.jobs,
    )?;
    let table_name = msg_table_name(&username);

    let contacts = contact_db
        .as_ref()
        .and_then(|p| load_contacts(p).ok())
        .unwrap_or_default();

    let display_name = contacts
        .get(&username)
        .map(|c| c.display_name(options.name_mode))
        .unwrap_or(&username)
        .to_string();

    let room_member_names = if options.name_mode == DisplayNameMode::Anonymous {
        contact_db
            .as_ref()
            .and_then(|p| load_chat_room_member_names(p, &username).ok())
            .unwrap_or_default()
    } else {
        HashMap::new()
    };

    let search_pattern = options.search_query.map(like_contains_pattern);
    let search_clause = options
        .search_query
        .map(|_| format!(" AND {}", msg_body_like_clause()))
        .unwrap_or_default();

    let since_clause = options
        .since
        .map(|ts| format!(" AND create_time >= {}", ts))
        .unwrap_or_default();

    let db_jobs = parallel::job_count(options.jobs, 8);
    let per_db_messages = parallel::map_ordered(message_dbs.clone(), db_jobs, |db_path| {
        let mut total_count = 0usize;
        let mut rows: Vec<MessageRow> = Vec::new();
        let conn = match Connection::open(&db_path) {
            Ok(c) => c,
            Err(_) => return (total_count, rows),
        };

        // Check if table exists quickly
        let table = quote_identifier(&table_name);
        let table_exists = conn
            .prepare(&format!("SELECT 1 FROM {} LIMIT 1", table))
            .is_ok();
        if !table_exists {
            return (total_count, rows);
        }

        // Get total count for this DB
        let count_sql = format!(
            "SELECT COUNT(*) FROM {} WHERE create_time > 0{}{}",
            table, search_clause, since_clause
        );
        if let Ok(mut stmt) = conn.prepare(&count_sql) {
            let count = match &search_pattern {
                Some(pattern) => stmt.query_row(params![pattern], |row| row.get::<_, i64>(0)),
                None => stmt.query_row([], |row| row.get::<_, i64>(0)),
            };
            if let Ok(cnt) = count {
                total_count += cnt.max(0) as usize;
            }
        }

        // Load Name2Id mapping (sender_id to account id)
        let name2id: HashMap<i64, String> =
            match conn.prepare("SELECT rowid, user_name FROM Name2Id") {
                Ok(mut stmt) => stmt
                    .query_map([], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })
                    .ok()
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default(),
                Err(_) => HashMap::new(),
            };

        let order_dir = if options.tail { "DESC" } else { "ASC" };

        // Query messages - collect eagerly to avoid borrow issues
        let body_col = quote_identifier(&dictionary::msg_body_column());
        let marker_col = quote_identifier(&dictionary::msg_compression_marker_column());
        let sender_col = quote_identifier(&dictionary::msg_sender_column());
        let packed_col = quote_identifier(&dictionary::msg_packed_meta_column());
        let sql = format!(
            "SELECT local_type, create_time, {body_col}, {marker_col}, {sender_col}, {packed_col} \
             FROM {table_name} WHERE create_time > 0{search_clause}{since_clause} ORDER BY create_time {order_dir}{limit_clause}",
            body_col = body_col,
            marker_col = marker_col,
            sender_col = sender_col,
            packed_col = packed_col,
            table_name = table,
            search_clause = search_clause,
            since_clause = since_clause,
            order_dir = order_dir,
            limit_clause =
                if options.tail { options.limit.map(|n| format!(" LIMIT {}", n)).unwrap_or_default() } else { String::new() }
        );
        rows = match conn.prepare(&sql) {
            Ok(mut stmt) => {
                let map_row = |row: &rusqlite::Row<'_>| {
                    let wcdb_ct: Option<i64> = row.get::<_, Option<i64>>(3)?;
                    let content: String = if wcdb_ct == Some(4) {
                        if let Ok(b) = row.get::<_, Vec<u8>>(2) {
                            message::try_decompress(&b).unwrap_or_default()
                        } else {
                            String::new()
                        }
                    } else {
                        match row.get::<_, Option<String>>(2) {
                            Ok(Some(s)) => s,
                            _ => match row.get::<_, Option<Vec<u8>>>(2) {
                                Ok(Some(b)) => String::from_utf8(b).unwrap_or_default(),
                                _ => String::new(),
                            },
                        }
                    };
                    let sender_id: i64 = row.get::<_, Option<i64>>(4)?.unwrap_or(0);
                    let sender_account_id = name2id.get(&sender_id).cloned().unwrap_or_default();
                    let packed_info: Vec<u8> =
                        row.get::<_, Option<Vec<u8>>>(5)?.unwrap_or_default();
                    Ok((
                        row.get::<_, Option<i64>>(0)?.unwrap_or(-1),
                        row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        content,
                        wcdb_ct,
                        sender_account_id,
                        packed_info,
                    ))
                };
                let rows = match &search_pattern {
                    Some(pattern) => stmt.query_map(params![pattern], map_row),
                    None => stmt.query_map([], map_row),
                };
                match rows {
                    Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                    Err(_) => vec![],
                }
            }
            Err(_) => vec![],
        };
        (total_count, rows)
    });

    let mut all_messages = Vec::new();
    let mut total_count: usize = 0;
    for (db_count, rows) in per_db_messages {
        total_count += db_count;
        all_messages.extend(rows);
    }

    if options.tail {
        // Each DB returns its latest rows; merge globally, then reverse for chronological display.
        all_messages.sort_by(|a, b| b.1.cmp(&a.1));
        if let Some(limit) = options.limit {
            all_messages.truncate(limit);
        }
        all_messages.reverse();
    } else {
        all_messages.sort_by(|a, b| a.1.cmp(&b.1));
    }
    let messages: Vec<_> = if options.tail {
        all_messages.iter().collect()
    } else if let Some(limit) = options.limit {
        all_messages
            .iter()
            .skip(options.offset)
            .take(limit)
            .collect()
    } else {
        all_messages.iter().skip(options.offset).collect()
    };

    if messages.is_empty() {
        return Ok(0);
    }

    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());

    out.line(format_args!("Chat with: {} ({})", display_name, username))?;
    if let Some(q) = options.search_query {
        out.line(format_args!("Search: '{}'", q))?;
    }
    if options.tail {
        if options.limit.is_some() {
            out.line(format_args!(
                "Showing latest {} of {} messages",
                messages.len(),
                total_count
            ))?;
        } else {
            out.line(format_args!(
                "Showing {} of {} messages",
                messages.len(),
                total_count
            ))?;
        }
    } else if options.limit.is_some() || options.offset > 0 {
        out.line(format_args!(
            "Showing {}-{} of {} messages",
            options.offset + 1,
            options.offset + messages.len(),
            total_count
        ))?;
    } else {
        out.line(format_args!(
            "Showing {} of {} messages",
            messages.len(),
            total_count
        ))?;
    }
    out.blank_line()?;

    let mut last_time_key = None;
    let mut last_sender: Option<String> = None;
    for (local_type, create_time, content, wcdb_ct, sender_account_id, packed_info) in &messages {
        // 1-on-1 chat: if the sender is not the chat partner, it's "me"
        let sender_display = if sender_account_id.is_empty() || sender_account_id == &username {
            display_name.as_str()
        } else {
            "我"
        };

        let decoded = message::decode_message_with_time_bucket(
            *local_type as i32,
            content,
            sender_display,
            *wcdb_ct,
            packed_info,
            options.time_bucket,
            |id| {
                resolve_sender_name_with_mode(id, &contacts, options.name_mode, &room_member_names)
            },
        );

        if options.time_bucket == time::MessageTimeBucket::PerMessage {
            out.line(format_args!(
                "[{}] {}: {}",
                time::format_local_timestamp(*create_time),
                decoded.display_name,
                decoded.content
            ))?;
        } else {
            if let Some(time_key) = time::message_time_key(*create_time, options.time_bucket) {
                if last_time_key != Some(time_key) {
                    out.line(format_args!(
                        "[{}]",
                        time::format_message_time_bucket(*create_time, options.time_bucket)
                    ))?;
                    last_time_key = Some(time_key);
                    last_sender = None;
                }
            }
            if last_sender.as_deref() == Some(decoded.display_name.as_str()) {
                out.line(format_args!(" {}", decoded.content))?;
            } else {
                out.line(format_args!(
                    "{}: {}",
                    decoded.display_name, decoded.content
                ))?;
                last_sender = Some(decoded.display_name);
            }
        }
    }

    out.blank_line()?;
    out.line(format_args!("--- End of messages ---"))?;
    out.flush()?;
    Ok(total_count)
}

pub(crate) fn probe_session(
    decrypted_dir: &Path,
    session_query: &str,
    jobs: usize,
) -> Result<SessionProbe, String> {
    let (contact_db, message_dbs) = find_decrypted_dbs(decrypted_dir);
    let username =
        resolve_username_for_messages(session_query, contact_db.as_deref(), &message_dbs, jobs)?;
    let table_name = msg_table_name(&username);
    let contacts = contact_db
        .as_ref()
        .and_then(|p| load_contacts(p).ok())
        .unwrap_or_default();
    let display_name = contacts
        .get(&username)
        .map(|contact| contact.display.clone())
        .unwrap_or_else(|| username.clone());

    let db_jobs = parallel::job_count(jobs, 8);
    let per_db_counts = parallel::map_ordered(message_dbs, db_jobs, |db_path| {
        let conn = match Connection::open(&db_path) {
            Ok(conn) => conn,
            Err(_) => return None,
        };
        let sql = format!(
            "SELECT COUNT(*) FROM {} WHERE create_time > 0",
            quote_identifier(&table_name)
        );
        conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
            .ok()
            .map(|count| count.max(0) as usize)
    });

    let mut matching_dbs = 0usize;
    let mut message_count = 0usize;
    for count in per_db_counts.into_iter().flatten() {
        matching_dbs += 1;
        message_count += count;
    }

    Ok(SessionProbe {
        username,
        display_name,
        table_name,
        matching_dbs,
        message_count,
    })
}

/// Search across all sessions.
pub fn search_messages(
    decrypted_dir: &Path,
    options: SearchMessagesOptions<'_>,
) -> Result<usize, String> {
    let (contact_db, message_dbs) = find_decrypted_dbs(decrypted_dir);
    let contacts = contact_db
        .as_ref()
        .and_then(|p| load_contacts(p).ok())
        .unwrap_or_default();

    let (total, results) = if options.use_telegram_fts {
        search_messages_with_telegram_fts(decrypted_dir, &options)
            .unwrap_or_else(|| search_messages_by_scanning(message_dbs, &options))
    } else {
        search_messages_by_scanning(message_dbs, &options)
    };

    print_search_results(options.query, total, results, &contacts)
}

fn search_messages_by_scanning(
    message_dbs: Vec<PathBuf>,
    options: &SearchMessagesOptions<'_>,
) -> (usize, Vec<SearchRow>) {
    let search_clause = msg_body_like_clause();
    let search_pattern = like_contains_pattern(options.query);
    let since_clause = options
        .since
        .map(|ts| format!(" AND create_time >= {}", ts))
        .unwrap_or_default();
    let result_limit = options.limit;
    let db_jobs = parallel::job_count(options.jobs, 8);
    let per_db_results = parallel::map_ordered(message_dbs.clone(), db_jobs, |db_path| {
        let mut total_count = 0usize;
        let mut results: Vec<SearchRow> = Vec::new();
        let conn = match Connection::open(&db_path) {
            Ok(c) => c,
            Err(_) => return (total_count, results),
        };

        // Find Msg_ tables - collect eagerly
        let tables: Vec<String> = match conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'")
        {
            Ok(mut stmt) => match stmt.query_map([], |row| row.get::<_, String>(0)) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(_) => vec![],
            },
            Err(_) => vec![],
        };

        for table_name in &tables {
            let table = quote_identifier(table_name);
            let count_sql = format!(
                "SELECT COUNT(*) FROM {} WHERE {}{}",
                table, search_clause, since_clause
            );
            if let Ok(mut stmt) = conn.prepare(&count_sql) {
                if let Ok(count) =
                    stmt.query_row(params![&search_pattern], |row| row.get::<_, i64>(0))
                {
                    total_count += count.max(0) as usize;
                }
            }

            let sql = format!(
                "SELECT local_type, create_time, {} \
                 FROM {} WHERE {}{} \
                 ORDER BY create_time DESC LIMIT {}",
                quote_identifier(&dictionary::msg_body_column()),
                table,
                search_clause,
                since_clause,
                result_limit
            );
            let rows: Vec<SearchRow> = match conn.prepare(&sql) {
                Ok(mut q) => match q.query_map(params![&search_pattern], |row| {
                    let content: String = match row.get::<_, Option<String>>(2) {
                        Ok(Some(s)) => s,
                        _ => match row.get::<_, Option<Vec<u8>>>(2) {
                            Ok(Some(b)) => String::from_utf8(b).unwrap_or_default(),
                            _ => String::new(),
                        },
                    };
                    Ok((
                        row.get::<_, Option<i64>>(0)?.unwrap_or(-1),
                        row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        content,
                        table_name.clone(),
                    ))
                }) {
                    Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                    Err(_) => vec![],
                },
                Err(_) => vec![],
            };
            results.extend(rows);
        }
        (total_count, results)
    });

    let mut results = Vec::new();
    let mut total = 0usize;
    for (count, rows) in per_db_results {
        total += count;
        results.extend(rows);
    }

    results.sort_by(|a, b| b.1.cmp(&a.1));
    results.truncate(options.limit);
    results.sort_by(|a, b| a.1.cmp(&b.1));

    (total, results)
}

fn search_messages_with_telegram_fts(
    decrypted_dir: &Path,
    options: &SearchMessagesOptions<'_>,
) -> Option<(usize, Vec<SearchRow>)> {
    let fts_db = find_message_fts_db(decrypted_dir)?;
    let conn = Connection::open(fts_db).ok()?;
    let content_tables = telegram_fts_content_tables(&conn)?;
    if content_tables.is_empty() {
        return None;
    }

    let name2id = load_fts_name2id(&conn);
    let search_clause = fts_content_like_clause();
    let search_pattern = like_contains_pattern(options.query);
    let since_clause = options
        .since
        .map(|ts| format!(" AND c6 >= {}", ts))
        .unwrap_or_default();

    let mut total = 0usize;
    let mut results: Vec<SearchRow> = Vec::new();

    for table_name in content_tables {
        let table = quote_identifier(&table_name);
        let count_sql = format!(
            "SELECT COUNT(*) FROM {} WHERE {}{}",
            table, search_clause, since_clause
        );
        if let Ok(mut stmt) = conn.prepare(&count_sql) {
            if let Ok(count) = stmt.query_row(params![&search_pattern], |row| row.get::<_, i64>(0))
            {
                total += count.max(0) as usize;
            }
        }

        let sql = format!(
            "SELECT c1, c6, c0, c4 \
             FROM {} WHERE {}{} \
             ORDER BY c6 DESC LIMIT {}",
            table, search_clause, since_clause, options.limit
        );
        let rows: Vec<SearchRow> = match conn.prepare(&sql) {
            Ok(mut stmt) => stmt
                .query_map(params![&search_pattern], |row| {
                    let session_id = row.get::<_, Option<i64>>(3)?.unwrap_or(0);
                    let session = name2id
                        .get(&session_id)
                        .cloned()
                        .unwrap_or_else(|| session_id.to_string());
                    Ok((
                        row.get::<_, Option<i64>>(0)?.unwrap_or(-1),
                        row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        session,
                    ))
                })
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        results.extend(rows);
    }

    results.sort_by(|a, b| b.1.cmp(&a.1));
    results.truncate(options.limit);
    results.sort_by(|a, b| a.1.cmp(&b.1));

    Some((total, results))
}

fn print_search_results(
    query: &str,
    total: usize,
    results: Vec<SearchRow>,
    contacts: &HashMap<String, Contact>,
) -> Result<usize, String> {
    if total == 0 {
        return Ok(0);
    }

    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());

    out.line(format_args!(
        "Search results for '{}': {} matches",
        query, total
    ))?;
    out.blank_line()?;

    let display_results = &results;

    for (i, (_, create_time, content, table_name)) in display_results.iter().enumerate() {
        let time_str = time::format_local_timestamp(*create_time);

        let display = search_session_display(contacts, table_name);

        let display_content = truncate_preview(content, SEARCH_PREVIEW_CHARS);

        out.line(format_args!(
            "[{}] {} | {}: {}",
            i + 1,
            time_str,
            display,
            display_content
        ))?;
    }

    if total > display_results.len() {
        out.line(format_args!(
            "... and {} older results",
            total - display_results.len()
        ))?;
    }

    out.flush()?;
    Ok(total)
}

fn find_message_fts_db(decrypted_dir: &Path) -> Option<PathBuf> {
    let path = decrypted_dir.join("message/message_fts.db");
    if path.exists() {
        return Some(path);
    }

    decrypted_dir
        .parent()
        .map(|p| p.join("decrypted/message/message_fts.db"))
        .filter(|p| p.exists())
}

fn telegram_fts_content_tables(conn: &Connection) -> Option<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type='table' AND name LIKE 'message_fts_v4_%_content' \
             ORDER BY name",
        )
        .ok()?;
    let tables: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .ok()?
        .filter_map(|row| row.ok())
        .filter(|name| {
            name.starts_with(TELEGRAM_FTS_CONTENT_TABLE_PREFIX)
                && name.ends_with(TELEGRAM_FTS_CONTENT_TABLE_SUFFIX)
                && name[TELEGRAM_FTS_CONTENT_TABLE_PREFIX.len()
                    ..name.len() - TELEGRAM_FTS_CONTENT_TABLE_SUFFIX.len()]
                    .chars()
                    .all(|c| c.is_ascii_digit())
        })
        .collect();
    Some(tables)
}

fn load_fts_name2id(conn: &Connection) -> HashMap<i64, String> {
    match conn.prepare("SELECT rowid, username FROM name2id") {
        Ok(mut stmt) => stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

fn search_session_display(contacts: &HashMap<String, Contact>, session: &str) -> String {
    if session.starts_with("Msg_") {
        return find_username_by_table(contacts, session).unwrap_or_else(|| "(?)".to_string());
    }

    contacts
        .get(session)
        .map(|c| c.display.clone())
        .unwrap_or_else(|| session.to_string())
}

fn msg_body_like_clause() -> String {
    format!(
        "{} LIKE ? ESCAPE '\\'",
        quote_identifier(&dictionary::msg_body_column())
    )
}

fn fts_content_like_clause() -> &'static str {
    "c0 LIKE ? ESCAPE '\\'"
}

fn like_contains_pattern(query: &str) -> String {
    let mut pattern = String::with_capacity(query.len() + 2);
    pattern.push('%');
    for ch in query.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

fn truncate_preview(content: &str, max_chars: usize) -> String {
    let mut chars = content.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{}...", preview)
    } else {
        content.to_string()
    }
}

pub(crate) fn resolve_username_for_messages(
    query: &str,
    contact_db: Option<&Path>,
    message_dbs: &[PathBuf],
    jobs: usize,
) -> Result<String, String> {
    resolve_username_with_context(query, contact_db, Some(message_dbs), jobs)
}

#[allow(dead_code)]
pub(crate) fn resolve_username(query: &str, contact_db: Option<&Path>) -> Result<String, String> {
    resolve_username_with_context(query, contact_db, None, 1)
}

fn resolve_username_with_context(
    query: &str,
    contact_db: Option<&Path>,
    message_dbs: Option<&[PathBuf]>,
    jobs: usize,
) -> Result<String, String> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(query.to_string());
    }

    // If it looks like a native account id, use it directly.
    if query.starts_with(&dictionary::account_id_prefix())
        || query.starts_with("gh_")
        || query.contains("@chatroom")
    {
        return Ok(query.to_string());
    }

    let contact_db = match contact_db {
        Some(p) => p,
        None => return Ok(query.to_string()),
    };

    let contacts = load_contacts(contact_db)?;

    // Try exact username match
    if let Some(c) = contacts.get(query) {
        return Ok(c.username.clone());
    }

    let candidates = contact_match_candidates(&contacts, query, message_dbs, jobs);
    if let Some(best) = candidates.first() {
        if candidates.len() > 1 || best.score < EXACT_MATCH_SCORE {
            log::info!(
                "Matched '{}' to {} ({}){}",
                query,
                best.contact.display,
                best.contact.username,
                best.latest_message_time
                    .map(|ts| format!(" latest {}", format_match_time(ts)))
                    .unwrap_or_default()
            );
        }
        if candidates.len() > 1 {
            log::info!("Other matches:");
            for candidate in candidates.iter().skip(1).take(4) {
                log::info!(
                    "  {} (nick: {}, remark: {}, alias: {}){}",
                    candidate.contact.username,
                    candidate.contact.nick_name,
                    candidate.contact.remark,
                    candidate.contact.alias,
                    candidate
                        .latest_message_time
                        .map(|ts| format!(" - latest {}", format_match_time(ts)))
                        .unwrap_or_default()
                );
            }
        }
        return Ok(best.contact.username.clone());
    }

    // No contact match. Treat the input as a raw username so callers can still
    // read sessions whose contact row is missing.
    Ok(query.to_string())
}

const EXACT_MATCH_SCORE: i64 = 400;
const NORMALIZED_EXACT_MATCH_SCORE: i64 = 390;
const CONTAINS_MATCH_SCORE: i64 = 300;
const NORMALIZED_CONTAINS_MATCH_SCORE: i64 = 290;
const TOKEN_MATCH_SCORE: i64 = 280;
const DIRECT_MATCH_SCORE_FLOOR: i64 = TOKEN_MATCH_SCORE;
const FUZZY_MATCH_SCORE: i64 = 100;

struct ContactMatch<'a> {
    contact: &'a Contact,
    score: i64,
    field_priority: usize,
    distance: usize,
    field_len: usize,
    latest_message_time: Option<i64>,
}

fn contact_match_candidates<'a>(
    contacts: &'a HashMap<String, Contact>,
    query: &str,
    message_dbs: Option<&[PathBuf]>,
    jobs: usize,
) -> Vec<ContactMatch<'a>> {
    let mut candidates: Vec<_> = contacts
        .values()
        .filter_map(|contact| {
            let (score, field_priority, distance, field_len) = best_contact_score(contact, query)?;
            Some(ContactMatch {
                contact,
                score,
                field_priority,
                distance,
                field_len,
                latest_message_time: None,
            })
        })
        .collect();

    if candidates
        .iter()
        .any(|candidate| candidate.score >= DIRECT_MATCH_SCORE_FLOOR)
    {
        candidates.retain(|candidate| candidate.score >= DIRECT_MATCH_SCORE_FLOOR);
    }

    if let Some(message_dbs) = message_dbs {
        let latest_by_username =
            latest_message_times_for_candidates(message_dbs, &candidates, jobs);
        for candidate in &mut candidates {
            candidate.latest_message_time =
                latest_by_username.get(&candidate.contact.username).copied();
        }
    }

    candidates.sort_by(|a, b| {
        let a_latest = a.latest_message_time.unwrap_or(0);
        let b_latest = b.latest_message_time.unwrap_or(0);
        b.latest_message_time
            .is_some()
            .cmp(&a.latest_message_time.is_some())
            .then_with(|| a.field_priority.cmp(&b.field_priority))
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| b_latest.cmp(&a_latest))
            .then_with(|| a.distance.cmp(&b.distance))
            .then_with(|| a.field_len.cmp(&b.field_len))
            .then_with(|| a.contact.display.cmp(&b.contact.display))
            .then_with(|| a.contact.username.cmp(&b.contact.username))
    });
    candidates
}

fn best_contact_score(contact: &Contact, query: &str) -> Option<(i64, usize, usize, usize)> {
    let normalized_query = normalize_match_text(query);
    if normalized_query.is_empty() {
        return None;
    }
    let query_tokens = tokenize_match_text(query);

    contact_match_fields(contact)
        .iter()
        .filter_map(|(priority, field)| {
            field_score(field, query, &normalized_query, &query_tokens)
                .map(|(score, distance, field_len)| (score, *priority, distance, field_len))
        })
        .max_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| b.3.cmp(&a.3))
        })
}

fn contact_match_fields(contact: &Contact) -> [(usize, &str); 5] {
    [
        (0, &contact.display),
        (1, &contact.remark),
        (2, &contact.nick_name),
        (3, &contact.alias),
        (4, &contact.username),
    ]
}

fn field_score(
    field: &str,
    query: &str,
    normalized_query: &str,
    query_tokens: &[String],
) -> Option<(i64, usize, usize)> {
    let field = field.trim();
    if field.is_empty() {
        return None;
    }

    let normalized_field = normalize_match_text(field);
    let field_len = normalized_field.chars().count();
    if field_len == 0 {
        return None;
    }

    if field == query {
        return Some((EXACT_MATCH_SCORE, 0, field_len));
    }
    if normalized_field == normalized_query {
        return Some((NORMALIZED_EXACT_MATCH_SCORE, 0, field_len));
    }
    if field.contains(query) {
        return Some((CONTAINS_MATCH_SCORE, 0, field_len));
    }
    if normalized_field.contains(normalized_query) {
        return Some((NORMALIZED_CONTAINS_MATCH_SCORE, 0, field_len));
    }
    if query_tokens.len() > 1
        && query_tokens
            .iter()
            .all(|token| normalized_field.contains(token))
    {
        return Some((TOKEN_MATCH_SCORE, 0, field_len));
    }

    let query_len = normalized_query.chars().count();
    if query_len < 2 {
        return None;
    }

    let distance = levenshtein_distance(&normalized_field, normalized_query);
    let max_len = field_len.max(query_len);
    let allowed_distance = if max_len <= 3 {
        1
    } else {
        (max_len / 3).max(1)
    };
    if distance > allowed_distance {
        return None;
    }

    let similarity = ((max_len - distance) * 100 / max_len) as i64;
    Some((FUZZY_MATCH_SCORE + similarity, distance, field_len))
}

fn normalize_match_text(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn tokenize_match_text(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for c in value.trim().chars().flat_map(|c| c.to_lowercase()) {
        if c.is_alphanumeric() {
            current.push(c);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

fn levenshtein_distance(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();

    if left.is_empty() {
        return right.len();
    }
    if right.is_empty() {
        return left.len();
    }

    let mut prev: Vec<usize> = (0..=right.len()).collect();
    let mut curr = vec![0; right.len() + 1];

    for (i, left_char) in left.iter().enumerate() {
        curr[0] = i + 1;
        for (j, right_char) in right.iter().enumerate() {
            let substitution = prev[j] + if left_char == right_char { 0 } else { 1 };
            let insertion = curr[j] + 1;
            let deletion = prev[j + 1] + 1;
            curr[j + 1] = substitution.min(insertion).min(deletion);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[right.len()]
}

fn latest_message_times_for_candidates(
    message_dbs: &[PathBuf],
    candidates: &[ContactMatch<'_>],
    jobs: usize,
) -> HashMap<String, i64> {
    let table_to_username: HashMap<String, String> = candidates
        .iter()
        .map(|candidate| {
            (
                msg_table_name(&candidate.contact.username),
                candidate.contact.username.clone(),
            )
        })
        .collect();
    let mut latest_by_username: HashMap<String, i64> = HashMap::new();

    let db_jobs = parallel::job_count(jobs, 8);
    let per_db_latest = parallel::map_ordered(message_dbs.to_vec(), db_jobs, |db_path| {
        let mut latest_by_username: HashMap<String, i64> = HashMap::new();
        let conn = match Connection::open(&db_path) {
            Ok(c) => c,
            Err(_) => return latest_by_username,
        };

        let tables: Vec<String> = match conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'")
        {
            Ok(mut stmt) => match stmt.query_map([], |row| row.get::<_, String>(0)) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(_) => vec![],
            },
            Err(_) => vec![],
        };

        for table_name in tables {
            let username = match table_to_username.get(&table_name) {
                Some(username) => username,
                None => continue,
            };
            let sql = format!(
                "SELECT MAX(create_time) FROM {} WHERE create_time > 0",
                quote_identifier(&table_name)
            );
            if let Ok(mut stmt) = conn.prepare(&sql) {
                if let Ok(Some(ts)) = stmt.query_row([], |row| row.get::<_, Option<i64>>(0)) {
                    let entry = latest_by_username.entry(username.clone()).or_insert(ts);
                    if ts > *entry {
                        *entry = ts;
                    }
                }
            };
        }
        latest_by_username
    });

    for db_latest in per_db_latest {
        for (username, ts) in db_latest {
            let entry = latest_by_username.entry(username).or_insert(ts);
            if ts > *entry {
                *entry = ts;
            }
        }
    }

    latest_by_username
}

fn format_match_time(timestamp: i64) -> String {
    time::format_local_timestamp(timestamp)
}

fn find_username_by_table(contacts: &HashMap<String, Contact>, table_name: &str) -> Option<String> {
    for username in contacts.keys() {
        if msg_table_name(username) == table_name {
            return Some(format!(
                "{} ({})",
                contacts.get(username)?.display,
                username
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::{tempdir, TempDir};

    fn create_contact_db(rows: &[(&str, &str, &str, &str)]) -> (TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("contact.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE contact (
                username TEXT,
                nick_name TEXT,
                remark TEXT,
                alias TEXT
            )",
            [],
        )
        .unwrap();

        for (username, nick_name, remark, alias) in rows {
            conn.execute(
                "INSERT INTO contact (username, nick_name, remark, alias) VALUES (?1, ?2, ?3, ?4)",
                params![username, nick_name, remark, alias],
            )
            .unwrap();
        }

        drop(conn);
        (dir, path)
    }

    fn create_message_db(rows: &[(&str, usize, i64)]) -> (TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("message_0.db");
        let conn = Connection::open(&path).unwrap();

        for (username, count, latest_time) in rows {
            let table_name = msg_table_name(username);
            let body_col = quote_identifier(&dictionary::msg_body_column());
            conn.execute(
                &format!(
                    "CREATE TABLE {} (
                        local_type INTEGER,
                        create_time INTEGER,
                        {} TEXT
                    )",
                    table_name, body_col
                ),
                [],
            )
            .unwrap();

            for i in 0..*count {
                let create_time = latest_time - (*count - i - 1) as i64;
                conn.execute(
                    &format!(
                        "INSERT INTO {} (local_type, create_time, {}) VALUES (1, ?1, 'hello')",
                        table_name, body_col
                    ),
                    params![create_time],
                )
                .unwrap();
            }
        }

        drop(conn);
        (dir, path)
    }

    fn create_decrypted_dir_with_session_counts(
        contacts: &[(&str, &str, &str, &str)],
        rows: &[(&str, usize, i64)],
    ) -> TempDir {
        let dir = tempdir().unwrap();
        let contact_dir = dir.path().join("contact");
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&contact_dir).unwrap();
        std::fs::create_dir_all(&message_dir).unwrap();

        let contact_conn = Connection::open(contact_dir.join("contact.db")).unwrap();
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
        for (username, nick_name, remark, alias) in contacts {
            contact_conn
                .execute(
                    "INSERT INTO contact (username, nick_name, remark, alias)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![username, nick_name, remark, alias],
                )
                .unwrap();
        }
        drop(contact_conn);

        let message_conn = Connection::open(message_dir.join("message_0.db")).unwrap();
        let body_col = quote_identifier(&dictionary::msg_body_column());
        for (username, count, latest_time) in rows {
            let table_name = msg_table_name(username);
            message_conn
                .execute(
                    &format!(
                        "CREATE TABLE {} (
                            local_type INTEGER,
                            create_time INTEGER,
                            {} TEXT
                        )",
                        table_name, body_col
                    ),
                    [],
                )
                .unwrap();

            for i in 0..*count {
                let create_time = latest_time - (*count - i - 1) as i64;
                message_conn
                    .execute(
                        &format!(
                            "INSERT INTO {} (local_type, create_time, {})
                             VALUES (1, ?1, 'hello')",
                            table_name, body_col
                        ),
                        params![create_time],
                    )
                    .unwrap();
            }
        }
        drop(message_conn);

        dir
    }

    fn create_decrypted_dir_with_messages(username: &str, contents: &[&str]) -> TempDir {
        let dir = tempdir().unwrap();
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();
        let path = message_dir.join("message_0.db");
        let conn = Connection::open(&path).unwrap();
        let table_name = msg_table_name(username);
        let body_col = quote_identifier(&dictionary::msg_body_column());
        let marker_col = quote_identifier(&dictionary::msg_compression_marker_column());
        let sender_col = quote_identifier(&dictionary::msg_sender_column());
        let packed_col = quote_identifier(&dictionary::msg_packed_meta_column());
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
                table_name, body_col, marker_col, sender_col, packed_col
            ),
            [],
        )
        .unwrap();

        for (i, content) in contents.iter().enumerate() {
            conn.execute(
                &format!(
                    "INSERT INTO {} (
                        local_type,
                        create_time,
                        {},
                        {},
                        {},
                        {}
                    ) VALUES (1, ?1, ?2, NULL, 0, x'')",
                    table_name, body_col, marker_col, sender_col, packed_col
                ),
                params![1000 + i as i64, content],
            )
            .unwrap();
        }

        drop(conn);
        dir
    }

    fn create_decrypted_dir_with_fts(contents: &[&str]) -> TempDir {
        let dir = tempdir().unwrap();
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();
        let path = message_dir.join("message_fts.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE name2id(username TEXT PRIMARY KEY)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO name2id (rowid, username) VALUES (7, 'tgid_fts_session')",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE message_fts_v4_0_content (
                id INTEGER PRIMARY KEY,
                c0,
                c1,
                c2,
                c3,
                c4,
                c5,
                c6
            )",
            [],
        )
        .unwrap();

        for (i, content) in contents.iter().enumerate() {
            conn.execute(
                "INSERT INTO message_fts_v4_0_content
                 (id, c0, c1, c2, c3, c4, c5, c6)
                 VALUES (?1, ?2, 1, 0, ?1, 7, 0, ?3)",
                params![i as i64 + 1, content, 1000 + i as i64],
            )
            .unwrap();
        }

        drop(conn);
        dir
    }

    fn push_proto_varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    fn push_proto_len_field(out: &mut Vec<u8>, field: u32, value: &[u8]) {
        push_proto_varint(out, ((field as u64) << 3) | 2);
        push_proto_varint(out, value.len() as u64);
        out.extend_from_slice(value);
    }

    fn push_proto_varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
        push_proto_varint(out, (field as u64) << 3);
        push_proto_varint(out, value);
    }

    fn chat_room_member_record(username: &str, display_name: &str) -> Vec<u8> {
        let mut record = Vec::new();
        push_proto_len_field(&mut record, 1, username.as_bytes());
        push_proto_len_field(&mut record, 2, display_name.as_bytes());
        push_proto_varint_field(&mut record, 3, 1);
        record
    }

    fn chat_room_ext_buffer(members: &[(&str, &str)]) -> Vec<u8> {
        let mut data = Vec::new();
        for (username, display_name) in members {
            push_proto_len_field(
                &mut data,
                1,
                &chat_room_member_record(username, display_name),
            );
        }
        data
    }

    #[test]
    fn filters_only_numbered_message_dbs() {
        assert!(is_message_db_name("message_0.db"));
        assert!(is_message_db_name("message_12.db"));
        assert!(!is_message_db_name("message_fts.db"));
        assert!(!is_message_db_name("message_resource.db"));
        assert!(!is_message_db_name("biz_message_0.db"));
    }

    #[test]
    fn parses_chat_room_member_names_from_ext_buffer() {
        let names = parse_chat_room_member_names(&chat_room_ext_buffer(&[
            ("tgid_alice", "Alice In Group"),
            ("tgid_bob", ""),
            ("tgid_cara", "Cara"),
        ]));

        assert_eq!(
            names.get("tgid_alice").map(String::as_str),
            Some("Alice In Group")
        );
        assert_eq!(names.get("tgid_cara").map(String::as_str), Some("Cara"));
        assert!(!names.contains_key("tgid_bob"));
    }

    #[test]
    fn loads_chat_room_member_names_for_room() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("contact.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE chat_room (
                id INTEGER PRIMARY KEY,
                username TEXT,
                owner TEXT,
                ext_buffer BLOB
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chat_room (username, ext_buffer) VALUES (?1, ?2)",
            params![
                "room@chatroom",
                chat_room_ext_buffer(&[("tgid_alice", "Alice In Group")])
            ],
        )
        .unwrap();
        drop(conn);

        let names = load_chat_room_member_names(&path, "room@chatroom").unwrap();

        assert_eq!(
            names.get("tgid_alice").map(String::as_str),
            Some("Alice In Group")
        );
    }

    #[test]
    fn sender_name_modes_choose_expected_display_source() {
        let mut contacts = HashMap::new();
        contacts.insert(
            "tgid_alice".to_string(),
            Contact {
                username: "tgid_alice".to_string(),
                nick_name: "Alice Default".to_string(),
                remark: "Alice Remark".to_string(),
                alias: String::new(),
                display: "Alice Remark".to_string(),
            },
        );
        let mut room_names = HashMap::new();
        room_names.insert("tgid_alice".to_string(), "Alice In Group".to_string());

        assert_eq!(
            resolve_sender_name_with_mode(
                "tgid_alice",
                &contacts,
                DisplayNameMode::PersonalRemark,
                &room_names,
            ),
            "Alice Remark"
        );
        assert_eq!(
            resolve_sender_name_with_mode(
                "tgid_alice",
                &contacts,
                DisplayNameMode::Anonymous,
                &room_names,
            ),
            "Alice In Group"
        );
        assert_eq!(
            resolve_sender_name_with_mode(
                "tgid_alice",
                &contacts,
                DisplayNameMode::Anonymous,
                &HashMap::new(),
            ),
            "Alice Default"
        );
    }

    #[test]
    fn session_stats_uses_sort_seq_boundaries() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE Msg_test (
                local_id INTEGER PRIMARY KEY AUTOINCREMENT,
                sort_seq INTEGER,
                create_time INTEGER
            )",
            [],
        )
        .unwrap();
        conn.execute("CREATE INDEX Msg_test_SORTSEQ ON Msg_test(sort_seq)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO Msg_test (sort_seq, create_time) VALUES
             (10, 1001), (11, 1000), (20, 2000), (21, 1999)",
            [],
        )
        .unwrap();

        let stats = session_stats_for_table(&conn, "Msg_test").unwrap();

        assert_eq!(stats.count, 4);
        assert_eq!(stats.earliest, Some(1000));
        assert_eq!(stats.latest, Some(2000));
    }

    #[test]
    fn session_stats_falls_back_without_sort_seq() {
        let conn = Connection::open_in_memory().unwrap();
        let body_col = quote_identifier(&dictionary::msg_body_column());
        conn.execute(
            &format!(
                "CREATE TABLE Msg_test (
                local_type INTEGER,
                create_time INTEGER,
                {} TEXT
            )",
                body_col
            ),
            [],
        )
        .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_type, create_time, {}) VALUES
             (1, 2000, 'newer'), (1, 1000, 'older')",
                body_col
            ),
            [],
        )
        .unwrap();

        let stats = session_stats_for_table(&conn, "Msg_test").unwrap();

        assert_eq!(stats.count, 2);
        assert_eq!(stats.earliest, Some(1000));
        assert_eq!(stats.latest, Some(2000));
    }

    #[test]
    fn list_sessions_returns_sorted_contact_display_names() {
        let dir = create_decrypted_dir_with_session_counts(
            &[
                ("tgid_low", "Low Nick", "Low Remark", ""),
                ("tgid_high", "High Nick", "", ""),
            ],
            &[("tgid_low", 1, 1000), ("tgid_high", 3, 2000)],
        );

        let sessions = list_sessions(dir.path(), 10, None, 1).unwrap();

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].0, "tgid_high");
        assert_eq!(sessions[0].1, 3);
        assert_eq!(sessions[0].3, "High Nick");
        assert_eq!(sessions[1].0, "tgid_low");
        assert_eq!(sessions[1].1, 1);
        assert_eq!(sessions[1].3, "Low Remark");
    }

    #[test]
    fn list_sessions_filters_by_fuzzy_contact_query() {
        let dir = create_decrypted_dir_with_session_counts(
            &[
                ("tgid_alice", "Alice Zhang", "", ""),
                ("tgid_bob", "Bob Lee", "", ""),
                ("tgid_inactive", "Alice Archive", "", ""),
            ],
            &[("tgid_alice", 2, 1000), ("tgid_bob", 5, 2000)],
        );

        let sessions = list_sessions(dir.path(), 10, Some("Alic Zhang"), 1).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0, "tgid_alice");
        assert_eq!(sessions[0].3, "Alice Zhang");
    }

    #[test]
    fn search_messages_matches_query_inside_content() {
        let dir = create_decrypted_dir_with_messages(
            "tgid_search",
            &["before needle after", "before needle", "no match"],
        );

        let count = search_messages(
            dir.path(),
            SearchMessagesOptions {
                query: "needle",
                limit: 20,
                since: None,
                use_telegram_fts: true,
                jobs: 1,
            },
        )
        .unwrap();

        assert_eq!(count, 2);
    }

    #[test]
    fn search_messages_treats_query_as_bound_text() {
        let dir = create_decrypted_dir_with_messages(
            "tgid_search",
            &["before needle after", "before needle", "no match"],
        );

        let count = search_messages(
            dir.path(),
            SearchMessagesOptions {
                query: "' OR 1=1 --",
                limit: 20,
                since: None,
                use_telegram_fts: false,
                jobs: 1,
            },
        )
        .unwrap();

        assert_eq!(count, 0);
    }

    #[test]
    fn search_messages_treats_wildcards_as_text() {
        let dir = create_decrypted_dir_with_messages(
            "tgid_search",
            &["literal 100%_match", "ordinary update"],
        );

        let count = search_messages(
            dir.path(),
            SearchMessagesOptions {
                query: "%",
                limit: 20,
                since: None,
                use_telegram_fts: false,
                jobs: 1,
            },
        )
        .unwrap();

        assert_eq!(count, 1);
    }

    #[test]
    fn search_messages_respects_since_filter() {
        let dir = create_decrypted_dir_with_messages(
            "tgid_search",
            &["old needle", "new needle", "no match"],
        );

        let count = search_messages(
            dir.path(),
            SearchMessagesOptions {
                query: "needle",
                limit: 20,
                since: Some(1001),
                use_telegram_fts: false,
                jobs: 1,
            },
        )
        .unwrap();

        assert_eq!(count, 1);
    }

    #[test]
    fn read_messages_search_matches_query_inside_content() {
        let dir = create_decrypted_dir_with_messages(
            "tgid_search",
            &["before needle after", "before needle", "no match"],
        );

        let count = read_messages(
            dir.path(),
            ReadMessagesOptions {
                session_query: "tgid_search",
                limit: None,
                offset: 0,
                search_query: Some("needle"),
                since: None,
                tail: false,
                time_bucket: time::MessageTimeBucket::PerMessage,
                name_mode: DisplayNameMode::PersonalRemark,
                jobs: 1,
            },
        )
        .unwrap();

        assert_eq!(count, 2);
    }

    #[test]
    fn read_messages_search_treats_wildcards_as_text() {
        let dir = create_decrypted_dir_with_messages(
            "tgid_search",
            &["before needle after", "before needle", "no match"],
        );

        let count = read_messages(
            dir.path(),
            ReadMessagesOptions {
                session_query: "tgid_search",
                limit: None,
                offset: 0,
                search_query: Some("%"),
                since: None,
                tail: false,
                time_bucket: time::MessageTimeBucket::PerMessage,
                name_mode: DisplayNameMode::PersonalRemark,
                jobs: 1,
            },
        )
        .unwrap();

        assert_eq!(count, 0);
    }

    #[test]
    fn read_messages_respects_since_filter() {
        let dir = create_decrypted_dir_with_messages(
            "tgid_search",
            &["old needle", "new needle", "no match"],
        );

        let count = read_messages(
            dir.path(),
            ReadMessagesOptions {
                session_query: "tgid_search",
                limit: None,
                offset: 0,
                search_query: Some("needle"),
                since: Some(1001),
                tail: false,
                time_bucket: time::MessageTimeBucket::PerMessage,
                name_mode: DisplayNameMode::PersonalRemark,
                jobs: 1,
            },
        )
        .unwrap();

        assert_eq!(count, 1);
    }

    #[test]
    fn search_messages_uses_telegram_fts_content_cache() {
        let dir =
            create_decrypted_dir_with_fts(&["before needle after", "before needle", "no match"]);

        let count = search_messages(
            dir.path(),
            SearchMessagesOptions {
                query: "needle",
                limit: 20,
                since: None,
                use_telegram_fts: true,
                jobs: 1,
            },
        )
        .unwrap();

        assert_eq!(count, 2);
    }

    #[test]
    fn fts_search_treats_query_as_bound_text() {
        let dir =
            create_decrypted_dir_with_fts(&["before needle after", "before needle", "no match"]);

        let count = search_messages(
            dir.path(),
            SearchMessagesOptions {
                query: "' OR 1=1 --",
                limit: 20,
                since: None,
                use_telegram_fts: true,
                jobs: 1,
            },
        )
        .unwrap();

        assert_eq!(count, 0);
    }

    #[test]
    fn fts_search_treats_wildcards_as_text() {
        let dir = create_decrypted_dir_with_fts(&["literal 100%_match", "ordinary update"]);

        let count = search_messages(
            dir.path(),
            SearchMessagesOptions {
                query: "%",
                limit: 20,
                since: None,
                use_telegram_fts: true,
                jobs: 1,
            },
        )
        .unwrap();

        assert_eq!(count, 1);
    }

    #[test]
    fn fts_search_respects_since_filter() {
        let dir = create_decrypted_dir_with_fts(&["old needle", "new needle", "no match"]);

        let count = search_messages(
            dir.path(),
            SearchMessagesOptions {
                query: "needle",
                limit: 20,
                since: Some(1001),
                use_telegram_fts: true,
                jobs: 1,
            },
        )
        .unwrap();

        assert_eq!(count, 1);
    }

    #[test]
    fn truncate_preview_handles_multibyte_content() {
        assert_eq!(truncate_preview("提取一下", 2), "提取...");
    }

    #[test]
    fn resolve_username_for_messages_prefers_latest_duplicate_name() {
        let (_contact_dir, contact_db) = create_contact_db(&[
            ("tgid_old", "田雨坤", "", ""),
            ("tgid_active", "田雨坤", "", ""),
        ]);
        let (_message_dir, message_db) =
            create_message_db(&[("tgid_old", 190, 1000), ("tgid_active", 2, 2000)]);

        let username =
            resolve_username_for_messages("田雨坤", Some(&contact_db), &[message_db], 1).unwrap();

        assert_eq!(username, "tgid_active");
    }

    #[test]
    fn resolve_username_for_messages_prefers_non_empty_contains_match() {
        let (_contact_dir, contact_db) = create_contact_db(&[
            ("tgid_empty", "豆", "", ""),
            ("tgid_doubao", "豆宝", "", ""),
            ("tgid_meilidou", "美丽豆", "", ""),
        ]);
        let (_message_dir, message_db) =
            create_message_db(&[("tgid_doubao", 17, 1000), ("tgid_meilidou", 100, 2000)]);

        let username =
            resolve_username_for_messages("豆", Some(&contact_db), &[message_db], 1).unwrap();

        assert_eq!(username, "tgid_meilidou");
    }

    #[test]
    fn resolve_username_for_messages_prefers_better_field_before_latest_time() {
        let (_contact_dir, contact_db) = create_contact_db(&[
            ("tgid_remark", "Someone", "Linux", ""),
            ("tgid_alias", "Someone Else", "", "Linux"),
        ]);
        let (_message_dir, message_db) =
            create_message_db(&[("tgid_remark", 1, 1000), ("tgid_alias", 1, 2000)]);

        let username =
            resolve_username_for_messages("Linux", Some(&contact_db), &[message_db], 1).unwrap();

        assert_eq!(username, "tgid_remark");
    }

    #[test]
    fn resolve_username_for_messages_matches_all_query_tokens() {
        let (_contact_dir, contact_db) = create_contact_db(&[
            ("linux_2025@chatroom", "Linux 俱乐部 #2025", "", ""),
            ("linux_2026@chatroom", "Linux 俱乐部 #2026🌅", "", ""),
        ]);
        let (_message_dir, message_db) = create_message_db(&[
            ("linux_2025@chatroom", 1, 1000),
            ("linux_2026@chatroom", 1, 2000),
        ]);

        let username =
            resolve_username_for_messages("Linux 2026", Some(&contact_db), &[message_db], 1)
                .unwrap();

        assert_eq!(username, "linux_2026@chatroom");
    }

    #[test]
    fn resolve_username_uses_fuzzy_match_after_exact_and_contains_fail() {
        let (_contact_dir, contact_db) = create_contact_db(&[
            ("tgid_alice", "Alice Zhang", "", ""),
            ("tgid_bob", "Bob Lee", "", ""),
        ]);

        let username = resolve_username("Alic Zhang", Some(&contact_db)).unwrap();

        assert_eq!(username, "tgid_alice");
    }
}
