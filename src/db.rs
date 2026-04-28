use md5::{Md5, Digest};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::fs;

use crate::message;

/// Resolve a sender ID to a display name from contacts.
pub fn resolve_sender_name(sender_id: &str, contacts: &HashMap<String, Contact>) -> String {
    contacts.get(sender_id)
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
        let alt = decrypted_dir.parent()
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
                    let name = path.file_name()
                        .and_then(|n| n.to_str()).unwrap_or("");
                    if name.starts_with("message_") && !name.contains("fts") {
                        message_dbs.push(path);
                    }
                }
            }
        }
    }
    message_dbs.sort();

    (contact_db, message_dbs)
}

/// Contact info.
pub(crate) struct Contact {
    pub username: String,
    pub nick_name: String,
    pub remark: String,
    pub alias: String,
    pub display: String,
}

pub(crate) fn load_contacts(contact_db: &Path) -> Result<HashMap<String, Contact>, String> {
    let conn = Connection::open(contact_db)
        .map_err(|e| format!("Cannot open contact DB: {}", e))?;

    let mut stmt = conn.prepare(
        "SELECT username, nick_name, remark, alias FROM contact"
    ).map_err(|e| format!("Contact query error: {}", e))?;

    let contacts: HashMap<String, Contact> = stmt.query_map([], |row| {
        let username: String = row.get(0)?;
        let nick_name: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
        let remark: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
        let alias: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
        let display = if !remark.is_empty() { remark.clone() } else { nick_name.clone() };
        Ok((username.clone(), Contact { username, nick_name, remark, alias, display }))
    }).map_err(|e| format!("Contact read error: {}", e))?
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

/// List all sessions/messages with counts.
pub fn list_sessions(decrypted_dir: &Path, top_n: usize) -> Result<Vec<(String, i64, String, String)>, String> {
    let (contact_db, message_dbs) = find_decrypted_dbs(decrypted_dir);

    // Load contacts
    let contacts = match &contact_db {
        Some(path) => load_contacts(path).unwrap_or_default(),
        None => HashMap::new(),
    };

    // Enumerate all Msg_ tables across message DBs
    #[derive(Default)]
    struct SessionInfo {
        count: i64,
        earliest: Option<i64>,
        latest: Option<i64>,
    }

    // Map: table_name -> SessionInfo
    // Also track: table_name -> contact username
    let mut table_to_username: HashMap<String, String> = HashMap::new();
    for (username, contact) in &contacts {
        let table = msg_table_name(username);
        table_to_username.insert(table, username.clone());
        // Also store by display name
        let display_table = msg_table_name(&contact.display);
        table_to_username.entry(display_table).or_insert_with(|| username.clone());
    }

    let mut sessions: HashMap<String, SessionInfo> = HashMap::new();

    for db_path in &message_dbs {
        let conn = match Connection::open(db_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Find all Msg_ tables
        let tables: Vec<String> = match conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'"
        ) {
            Ok(mut stmt) => match stmt.query_map([], |row| row.get::<_, String>(0)) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(_) => vec![],
            },
            Err(_) => vec![],
        };

        for table_name in &tables {
            let sql = format!(
                "SELECT COUNT(*), MIN(create_time), MAX(create_time) FROM {} WHERE create_time > 0",
                table_name
            );
            let stats: Vec<(i64, Option<i64>, Option<i64>)> = match conn.prepare(&sql) {
                Ok(mut q) => match q.query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?, row.get::<_, Option<i64>>(2)?))
                }) {
                    Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                    Err(_) => vec![],
                },
                Err(_) => vec![],
            };
            if let Some((cnt, earliest, latest)) = stats.into_iter().next() {
                if cnt > 0 {
                    let info = sessions.entry(table_name.clone()).or_default();
                    info.count += cnt;
                    if earliest.is_none_or(|e| info.earliest.is_none_or(|ie| e < ie)) {
                        info.earliest = earliest;
                    }
                    if latest.is_none_or(|l| info.latest.is_none_or(|il| l > il)) {
                        info.latest = latest;
                    }
                }
            }
        }
    }

    // Sort by message count
    let mut sorted: Vec<_> = sessions.into_iter().collect();
    sorted.sort_by(|a, b| b.1.count.cmp(&a.1.count));

    // Print header
    println!("{:<4} {:<8} {:<46} {:<22} Username", "Rank", "Count", "Time Range", "Display Name");
    println!("{}", "-".repeat(120));

    let mut result = Vec::new();

    for (i, (table, info)) in sorted.iter().enumerate().take(top_n) {
        let username = table_to_username.get(table)
            .cloned()
            .unwrap_or_else(|| table.clone());
        let display = contacts.get(&username)
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

        println!("{:<4} {:<8} {:<46} {:<22} {}",
            i + 1, info.count, time_range, display, username);

        result.push((username.clone(), info.count, time_range, display.to_string()));
    }

    println!("\nTotal: {} sessions", sorted.len());
    Ok(result)
}

/// Read messages from a specific session.
pub fn read_messages(
    decrypted_dir: &Path,
    session_query: &str,
    limit: usize,
    offset: usize,
    search_query: Option<&str>,
    since: Option<i64>,
    tail: bool,
) -> Result<usize, String> {
    let (contact_db, message_dbs) = find_decrypted_dbs(decrypted_dir);

    let username = resolve_username(session_query, contact_db.as_deref())?;
    let table_name = msg_table_name(&username);
    let cst_offset = chrono::FixedOffset::east_opt(8 * 3600).unwrap();

    let contacts = contact_db.as_ref()
        .and_then(|p| load_contacts(p).ok())
        .unwrap_or_default();

    let display_name = contacts.get(&username)
        .map(|c| c.display.as_str())
        .unwrap_or(&username);

    let mut all_messages = Vec::new();
    let mut total_count: usize = 0;

    for db_path in &message_dbs {
        let conn = match Connection::open(db_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Check if table exists quickly
        let table_exists = conn.prepare(&format!("SELECT 1 FROM {} LIMIT 1", table_name)).is_ok();
        if !table_exists { continue; }

        let search_clause = search_query
            .map(|q| format!(" AND message_content LIKE '%{}'", q.replace('\'', "''")))
            .unwrap_or_default();

        let since_clause = since
            .map(|ts| format!(" AND create_time >= {}", ts))
            .unwrap_or_default();

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
        let name2id: HashMap<i64, String> = match conn.prepare("SELECT rowid, user_name FROM Name2Id") {
            Ok(mut stmt) => stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            }).ok().map(|rows| rows.filter_map(|r| r.ok()).collect()).unwrap_or_default(),
            Err(_) => HashMap::new(),
        };

        let order_dir = if tail { "DESC" } else { "ASC" };

        // Query messages - collect eagerly to avoid borrow issues
        let sql = format!(
            "SELECT local_type, create_time, message_content, WCDB_CT_message_content, real_sender_id \
             FROM {} WHERE create_time > 0{}{} ORDER BY create_time {}",
            table_name, search_clause, since_clause, order_dir
        );
        let rows: Vec<(i64, i64, String, Option<i64>, String)> = match conn.prepare(&sql) {
            Ok(mut stmt) => match stmt.query_map([], |row| {
                // message_content can be TEXT or BLOB; read as String when possible
                let content: String = match row.get::<_, Option<String>>(2) {
                    Ok(Some(s)) => s,
                    _ => match row.get::<_, Option<Vec<u8>>>(2) {
                        Ok(Some(b)) => String::from_utf8(b).unwrap_or_default(),
                        _ => String::new(),
                    },
                };
                let sender_id: i64 = row.get::<_, Option<i64>>(4)?.unwrap_or(0);
                let sender_tgid = name2id.get(&sender_id).cloned().unwrap_or_default();
                Ok((
                    row.get::<_, Option<i64>>(0)?.unwrap_or(-1),
                    row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    content,
                    row.get::<_, Option<i64>>(3)?,
                    sender_tgid,
                ))
            }) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(_) => vec![],
            },
            Err(_) => vec![],
        };
        all_messages.extend(rows);
    }

    if tail {
        // Query returned DESC, take `limit` latest and reverse for chronological display
        all_messages.truncate(limit);
        all_messages.reverse();
    } else {
        all_messages.sort_by(|a, b| a.1.cmp(&b.1));
    }
    let messages: Vec<_> = if tail {
        all_messages.iter().collect()
    } else {
        all_messages.iter().skip(offset).take(limit).collect()
    };

    if messages.is_empty() {
        if let Some(q) = search_query {
            println!("No messages found matching '{}' for {}", q, display_name);
        } else {
            println!("No messages found for {} ({})", display_name, username);
        }
        return Ok(0);
    }

    println!("\nChat with: {} ({})", display_name, username);
    if let Some(q) = search_query {
        println!("Search: '{}'", q);
    }
    if tail {
        println!("Showing latest {} of {} messages\n", messages.len(), total_count);
    } else {
        println!("Showing {}-{} of {} messages\n", offset + 1, offset + messages.len(), total_count);
    }

    for (local_type, create_time, content, wcdb_ct, sender_tgid) in &messages {
        let time_str = chrono::DateTime::from_timestamp(*create_time, 0)
            .map(|t| t.with_timezone(&cst_offset).format("%Y-%m-%d %H:%M:%S").to_string())
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
            |id| resolve_sender_name(id, &contacts),
        );

        println!("[{}] {}: {}", time_str, decoded.display_name, decoded.content);
    }

    println!("\n--- End of messages ---");
    Ok(total_count)
}

/// Search across all sessions.
pub fn search_messages(
    decrypted_dir: &Path,
    query: &str,
    limit: usize,
) -> Result<usize, String> {
    let (contact_db, message_dbs) = find_decrypted_dbs(decrypted_dir);
    let contacts = contact_db.as_ref()
        .and_then(|p| load_contacts(p).ok())
        .unwrap_or_default();

    let mut results = Vec::new();

    for db_path in &message_dbs {
        let conn = match Connection::open(db_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Find Msg_ tables - collect eagerly
        let tables: Vec<String> = match conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'"
        ) {
            Ok(mut stmt) => match stmt.query_map([], |row| row.get::<_, String>(0)) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(_) => vec![],
            },
            Err(_) => vec![],
        };

        let escaped = query.replace('\'', "''");
        for table_name in &tables {
            let sql = format!(
                "SELECT local_type, create_time, message_content \
                 FROM {} WHERE message_content LIKE '%{}' \
                 ORDER BY create_time ASC LIMIT 50",
                table_name, escaped
            );
            let rows: Vec<(i64, i64, String, String)> = match conn.prepare(&sql) {
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
    }

    results.sort_by(|a, b| a.1.cmp(&b.1));
    let total = results.len();
    let cst_offset = chrono::FixedOffset::east_opt(8 * 3600).unwrap();

    println!("Search results for '{}': {} matches\n", query, total);

    for (i, (_, create_time, content, table_name)) in results.iter().enumerate().take(limit) {
        let time_str = chrono::DateTime::from_timestamp(*create_time, 0)
            .map(|t| t.with_timezone(&cst_offset).format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default();

        let display = find_username_by_table(&contacts, table_name)
            .unwrap_or_else(|| "(?)".to_string());

        let display_content = if content.len() > 100 {
            format!("{}...", &content[..100])
        } else {
            content.clone()
        };

        println!("[{}] {} | {}: {}", i + 1, time_str, display, display_content);
    }

    if total > limit {
        println!("... and {} more results", total - limit);
    }

    Ok(total)
}

pub(crate) fn resolve_username(query: &str, contact_db: Option<&Path>) -> Result<String, String> {
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

    // Fuzzy search by display name, nick, remark, alias
    let results: Vec<_> = contacts.values()
        .filter(|c| {
            c.display.contains(query)
                || c.nick_name.contains(query)
                || c.remark.contains(query)
                || c.alias.contains(query)
        })
        .collect();

    if results.is_empty() {
        // Try matching the msg table name directly
        let table_name = if query.starts_with("Msg_") {
            query.to_string()
        } else {
            msg_table_name(query)
        };
        return Ok(table_name);
    }

    if results.len() == 1 {
        return Ok(results[0].username.clone());
    }

    // Multiple matches - use the first one
    eprintln!("Multiple matches for '{}':", query);
    for c in &results {
        eprintln!("  {} (nick: {}, remark: {}, alias: {})",
            c.username, c.nick_name, c.remark, c.alias);
    }
    eprintln!("Using: {}", results[0].username);
    Ok(results[0].username.clone())
}

fn find_username_by_table(contacts: &HashMap<String, Contact>, table_name: &str) -> Option<String> {
    for username in contacts.keys() {
        if msg_table_name(username) == table_name {
            return Some(format!("{} ({})", contacts.get(username)?.display, username));
        }
    }
    None
}

