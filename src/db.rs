use md5::{Digest, Md5};
use rusqlite::Connection;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::message;
use crate::parallel;

/// Resolve a sender ID to a display name from contacts.
pub fn resolve_sender_name(sender_id: &str, contacts: &HashMap<String, Contact>) -> String {
    contacts
        .get(sender_id)
        .map(|c| c.display.as_str())
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

#[derive(Default)]
struct SessionInfo {
    count: i64,
    earliest: Option<i64>,
    latest: Option<i64>,
}

type MessageRow = (i64, i64, String, Option<i64>, String, Vec<u8>);
type SearchRow = (i64, i64, String, String);
const SESSION_TIME_BOUNDARY_ROWS: usize = 128;

pub(crate) struct ReadMessagesOptions<'a> {
    pub session_query: &'a str,
    pub limit: Option<usize>,
    pub offset: usize,
    pub search_query: Option<&'a str>,
    pub since: Option<i64>,
    pub tail: bool,
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

pub(crate) fn msg_table_name(username: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(username.as_bytes());
    let hash = hasher.finalize();
    format!("Msg_{:x}", hash)
}

fn quote_identifier(name: &str) -> String {
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

    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());

    out.line(format_args!(
        "{:<4} {:<8} {:<46} {:<22} Username",
        "Rank", "Count", "Time Range", "Display Name"
    ))?;
    out.line(format_args!("{}", "-".repeat(120)))?;

    let mut result = Vec::new();

    for (i, (table, info)) in sorted.iter().enumerate().take(top_n) {
        let username = table_to_username
            .get(table)
            .cloned()
            .unwrap_or_else(|| table.clone());
        let display = contacts
            .get(&username)
            .map(|c| c.display.as_str())
            .unwrap_or("(?))");

        let time_range = match (info.earliest, info.latest) {
            (Some(e), Some(l)) => {
                let e_ts = chrono::DateTime::from_timestamp(e, 0)
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                let l_ts = chrono::DateTime::from_timestamp(l, 0)
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                format!("{} ~ {}", e_ts, l_ts)
            }
            _ => String::new(),
        };

        out.line(format_args!(
            "{:<4} {:<8} {:<46} {:<22} {}",
            i + 1,
            info.count,
            time_range,
            display,
            username
        ))?;

        result.push((
            username.clone(),
            info.count,
            time_range,
            display.to_string(),
        ));
    }

    out.blank_line()?;
    out.line(format_args!("Total: {} sessions", sorted.len()))?;
    out.flush()?;
    Ok(result)
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
    let cst_offset = chrono::FixedOffset::east_opt(8 * 3600).unwrap();

    let contacts = contact_db
        .as_ref()
        .and_then(|p| load_contacts(p).ok())
        .unwrap_or_default();

    let display_name = contacts
        .get(&username)
        .map(|c| c.display.as_str())
        .unwrap_or(&username);

    let search_clause = options
        .search_query
        .map(|q| format!(" AND message_content LIKE '%{}'", q.replace('\'', "''")))
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
        let table_exists = conn
            .prepare(&format!("SELECT 1 FROM {} LIMIT 1", table_name))
            .is_ok();
        if !table_exists {
            return (total_count, rows);
        }

        // Get total count for this DB
        let count_sql = format!(
            "SELECT COUNT(*) FROM {} WHERE create_time > 0{}{}",
            table_name, search_clause, since_clause
        );
        if let Ok(mut stmt) = conn.prepare(&count_sql) {
            if let Ok(cnt) = stmt.query_row([], |row| row.get::<_, i64>(0)) {
                total_count += cnt as usize;
            }
        }

        // Load Name2Id mapping (sender_id → tgid)
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
        let sql = format!(
            "SELECT local_type, create_time, message_content, WCDB_CT_message_content, real_sender_id, packed_info_data \
             FROM {} WHERE create_time > 0{}{} ORDER BY create_time {}{}",
            table_name,
            search_clause,
            since_clause,
            order_dir,
            if options.tail { options.limit.map(|n| format!(" LIMIT {}", n)).unwrap_or_default() } else { String::new() }
        );
        rows = match conn.prepare(&sql) {
            Ok(mut stmt) => match stmt.query_map([], |row| {
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
                let sender_tgid = name2id.get(&sender_id).cloned().unwrap_or_default();
                let packed_info: Vec<u8> = row.get::<_, Option<Vec<u8>>>(5)?.unwrap_or_default();
                Ok((
                    row.get::<_, Option<i64>>(0)?.unwrap_or(-1),
                    row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    content,
                    wcdb_ct,
                    sender_tgid,
                    packed_info,
                ))
            }) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(_) => vec![],
            },
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

    for (local_type, create_time, content, wcdb_ct, sender_tgid, packed_info) in &messages {
        let time_str = chrono::DateTime::from_timestamp(*create_time, 0)
            .map(|t| {
                t.with_timezone(&cst_offset)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_default();

        // 1-on-1 chat: if the sender is not the chat partner, it's "me"
        let sender_display = if sender_tgid.is_empty() || sender_tgid == &username {
            display_name
        } else {
            "我"
        };

        let decoded = message::decode_message(
            *local_type as i32,
            content,
            sender_display,
            *wcdb_ct,
            packed_info,
            |id| resolve_sender_name(id, &contacts),
        );

        out.line(format_args!(
            "[{}] {}: {}",
            time_str, decoded.display_name, decoded.content
        ))?;
    }

    out.blank_line()?;
    out.line(format_args!("--- End of messages ---"))?;
    out.flush()?;
    Ok(total_count)
}

/// Search across all sessions.
pub fn search_messages(
    decrypted_dir: &Path,
    query: &str,
    limit: usize,
    jobs: usize,
) -> Result<usize, String> {
    let (contact_db, message_dbs) = find_decrypted_dbs(decrypted_dir);
    let contacts = contact_db
        .as_ref()
        .and_then(|p| load_contacts(p).ok())
        .unwrap_or_default();

    let escaped = query.replace('\'', "''");
    let db_jobs = parallel::job_count(jobs, 8);
    let per_db_results = parallel::map_ordered(message_dbs.clone(), db_jobs, |db_path| {
        let mut results: Vec<SearchRow> = Vec::new();
        let conn = match Connection::open(&db_path) {
            Ok(c) => c,
            Err(_) => return results,
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
            let sql = format!(
                "SELECT local_type, create_time, message_content \
                 FROM {} WHERE message_content LIKE '%{}' \
                 ORDER BY create_time ASC LIMIT 50",
                table_name, escaped
            );
            let rows: Vec<SearchRow> = match conn.prepare(&sql) {
                Ok(mut q) => match q.query_map([], |row| {
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
        results
    });

    let mut results = Vec::new();
    for rows in per_db_results {
        results.extend(rows);
    }

    results.sort_by(|a, b| a.1.cmp(&b.1));
    let total = results.len();
    let cst_offset = chrono::FixedOffset::east_opt(8 * 3600).unwrap();

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

    for (i, (_, create_time, content, table_name)) in results.iter().enumerate().take(limit) {
        let time_str = chrono::DateTime::from_timestamp(*create_time, 0)
            .map(|t| {
                t.with_timezone(&cst_offset)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_default();

        let display =
            find_username_by_table(&contacts, table_name).unwrap_or_else(|| "(?)".to_string());

        let display_content = if content.len() > 100 {
            format!("{}...", &content[..100])
        } else {
            content.clone()
        };

        out.line(format_args!(
            "[{}] {} | {}: {}",
            i + 1,
            time_str,
            display,
            display_content
        ))?;
    }

    if total > limit {
        out.line(format_args!("... and {} more results", total - limit))?;
    }

    out.flush()?;
    Ok(total)
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

    // If it looks like a tgid, use it directly
    if query.starts_with("tgid_") || query.starts_with("gh_") || query.contains("@chatroom") {
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
                table_name
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
    let cst_offset = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|t| {
            t.with_timezone(&cst_offset)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| timestamp.to_string())
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
            conn.execute(
                &format!(
                    "CREATE TABLE {} (
                        local_type INTEGER,
                        create_time INTEGER,
                        message_content TEXT
                    )",
                    table_name
                ),
                [],
            )
            .unwrap();

            for i in 0..*count {
                let create_time = latest_time - (*count - i - 1) as i64;
                conn.execute(
                    &format!(
                        "INSERT INTO {} (local_type, create_time, message_content) VALUES (1, ?1, 'hello')",
                        table_name
                    ),
                    params![create_time],
                ).unwrap();
            }
        }

        drop(conn);
        (dir, path)
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
        conn.execute(
            "CREATE TABLE Msg_test (
                local_type INTEGER,
                create_time INTEGER,
                message_content TEXT
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO Msg_test (local_type, create_time, message_content) VALUES
             (1, 2000, 'newer'), (1, 1000, 'older')",
            [],
        )
        .unwrap();

        let stats = session_stats_for_table(&conn, "Msg_test").unwrap();

        assert_eq!(stats.count, 2);
        assert_eq!(stats.earliest, Some(1000));
        assert_eq!(stats.latest, Some(2000));
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
