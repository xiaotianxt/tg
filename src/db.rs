use md5::{Digest, Md5};
use rusqlite::{params, Connection, OptionalExtension};
use std::cmp::Reverse;
use std::collections::HashMap;

use crate::contact::{self, Contact, DisplayNameMode};
use crate::dictionary;
use crate::fts;
use crate::message;
use crate::parallel;
use crate::source_store::SourceStore;
use crate::time;

const CONTACT_DB: &str = "contact/contact.db";
const SESSION_DB: &str = "session/session.db";
const MESSAGE_FTS_DB: &str = "message/message_fts.db";

pub(crate) fn has_database(store: &SourceStore, relative_path: &str) -> bool {
    store.databases().any(|path| path == relative_path)
}

pub(crate) fn message_databases(store: &SourceStore) -> Vec<String> {
    store
        .databases()
        .filter(|path| {
            path.strip_prefix("message/")
                .is_some_and(is_message_db_name)
        })
        .map(str::to_string)
        .collect()
}

fn open_optional_database(
    store: &SourceStore,
    relative_path: &str,
) -> Result<Option<Connection>, String> {
    has_database(store, relative_path)
        .then(|| store.open(relative_path))
        .transpose()
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

#[derive(Default)]
struct SessionInfo {
    count: i64,
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

#[derive(Debug, Clone)]
struct MessageRow {
    local_type: i64,
    create_time: i64,
    content: String,
    compression_marker: Option<i64>,
    sender_account_id: String,
    packed_info: Vec<u8>,
    local_id: Option<i64>,
}

type SearchRow = (i64, i64, String, String);
type SessionListRow = (String, i64, String, String);
type SessionListRows = Vec<SessionListRow>;
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

#[derive(Clone, Copy)]
enum SessionDbListMode {
    All,
    Unread,
}

impl SessionDbListMode {
    fn where_clause(self) -> &'static str {
        match self {
            Self::All => {
                "WHERE COALESCE(last_timestamp, 0) > 0
                    OR COALESCE(sort_timestamp, 0) > 0"
            }
            Self::Unread => {
                "WHERE COALESCE(unread_count, 0) > 0
                   AND (COALESCE(last_timestamp, 0) > 0
                        OR COALESCE(sort_timestamp, 0) > 0)"
            }
        }
    }

    fn total_label(self) -> &'static str {
        match self {
            Self::All => "sessions",
            Self::Unread => "unread sessions",
        }
    }
}

pub(crate) struct SearchMessagesOptions<'a> {
    pub query: &'a str,
    pub limit: usize,
    pub since: Option<i64>,
    pub name_mode: DisplayNameMode,
}

pub(crate) struct ListMembersOptions<'a> {
    pub session_query: &'a str,
    pub inactive_since: Option<i64>,
    pub name_mode: DisplayNameMode,
    pub jobs: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MemberActivity {
    pub username: String,
    pub display_name: String,
    pub is_owner: bool,
    pub last_spoke: Option<i64>,
    pub message_count: i64,
}

#[derive(Clone, Copy)]
struct MemberSpeech {
    first_spoke: i64,
    last_spoke: i64,
    message_count: i64,
}

pub(crate) fn msg_table_name(username: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(username.as_bytes());
    let hash = hasher.finalize();
    format!("Msg_{}", hex::encode(hash))
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

pub(crate) fn table_has_column(
    conn: &Connection,
    table_name: &str,
    column_name: &str,
) -> Result<bool, String> {
    let sql = format!("PRAGMA table_info({})", quote_identifier(table_name));
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|error| format!("Inspect columns of {table_name}: {error}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("Read columns of {table_name}: {error}"))?;
    for row in rows {
        if row.map_err(|error| format!("Read column of {table_name}: {error}"))? == column_name {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn list_message_tables(conn: &Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'")
        .map_err(|e| format!("Cannot list message tables: {}", e))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("Cannot read message table names: {}", e))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| format!("Read message table name: {error}"))
}

pub(crate) fn table_exists(conn: &Connection, table_name: &str) -> Result<bool, String> {
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1
        )",
        [table_name],
        |row| row.get(0),
    )
    .map_err(|error| format!("Check table {table_name}: {error}"))
}

pub(crate) fn load_name2id(conn: &Connection) -> Result<HashMap<i64, String>, String> {
    if !table_exists(conn, "Name2Id")? {
        return Ok(HashMap::new());
    }
    let mut stmt = conn
        .prepare("SELECT rowid, user_name FROM Name2Id")
        .map_err(|error| format!("Prepare Name2Id: {error}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| format!("Read Name2Id: {error}"))?;
    rows.collect::<rusqlite::Result<HashMap<_, _>>>()
        .map_err(|error| format!("Read Name2Id row: {error}"))
}

pub(crate) fn read_message_body(
    row: &rusqlite::Row<'_>,
    index: usize,
    marker: Option<i64>,
) -> rusqlite::Result<String> {
    if marker == Some(4) {
        let bytes = row.get::<_, Vec<u8>>(index)?;
        return Ok(message::try_decompress(&bytes).unwrap_or_default());
    }

    match row.get::<_, Option<String>>(index) {
        Ok(Some(value)) => Ok(value),
        Ok(None) => Ok(String::new()),
        _ => match row.get::<_, Option<Vec<u8>>>(index) {
            Ok(Some(bytes)) => Ok(String::from_utf8(bytes).unwrap_or_default()),
            Ok(None) => Ok(String::new()),
            Err(error) => Err(error),
        },
    }
}

/// List chat sessions.
pub fn list_sessions(
    store: &SourceStore,
    top_n: usize,
    query: Option<&str>,
) -> Result<SessionListRows, String> {
    let contact_conn = open_optional_database(store, CONTACT_DB)?;
    let contacts = match contact_conn.as_ref() {
        Some(conn) => contact::load_contacts(conn)?,
        None => HashMap::new(),
    };
    let session_conn = store
        .open(SESSION_DB)
        .map_err(|error| format!("Session database is unavailable: {error}"))?;
    list_sessions_from_connection(
        &session_conn,
        &contacts,
        top_n,
        query,
        SessionDbListMode::All,
    )
}

/// List unread chat sessions from the session database.
pub fn list_unread_sessions(
    store: &SourceStore,
    top_n: usize,
    query: Option<&str>,
) -> Result<SessionListRows, String> {
    let contact_conn = open_optional_database(store, CONTACT_DB)?;
    let contacts = match contact_conn.as_ref() {
        Some(conn) => contact::load_contacts(conn)?,
        None => HashMap::new(),
    };
    let session_conn = store
        .open(SESSION_DB)
        .map_err(|error| format!("Session database is unavailable: {error}"))?;
    list_sessions_from_connection(
        &session_conn,
        &contacts,
        top_n,
        query,
        SessionDbListMode::Unread,
    )
}

/// Intersect the cached roster with explicit local friend relationships.
/// Contact/session scope only: a newly joined group needs no message history.
pub(crate) fn list_member_friends(
    store: &SourceStore,
    session_query: &str,
    name_mode: DisplayNameMode,
    jobs: usize,
) -> Result<Vec<String>, String> {
    let conn = store.open(CONTACT_DB)?;
    let contacts = contact::load_contacts(&conn)?;
    let username = resolve_username_from_contacts(session_query, &contacts, store, None, jobs)?;
    if !username.ends_with("@chatroom") {
        return Err(format!("Not a group session: {username}"));
    }
    let roster = contact::load_chat_room_roster(&conn, &username)?;
    if roster.usernames.is_empty() {
        return Err(format!("No cached members found for group: {username}"));
    }
    let friends = contact::load_friend_usernames(&conn)?;
    let room_names = if name_mode == DisplayNameMode::Anonymous {
        contact::load_chat_room_member_names(&conn, &username)?
    } else {
        HashMap::new()
    };
    let mut matches: Vec<_> = roster
        .usernames
        .iter()
        .filter(|id| {
            friends.contains(*id) && contacts.get(*id).is_some_and(|value| !value.is_stranger)
        })
        .map(|id| {
            (
                contact::resolve_sender_name_with_mode(id, &contacts, name_mode, &room_names),
                id.clone(),
            )
        })
        .collect();
    matches.sort();
    matches.dedup_by(|left, right| left.1 == right.1);
    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());
    out.line(format_args!(
        "Group: {} ({username})",
        contacts
            .get(&username)
            .map(|value| value.display_name(name_mode))
            .unwrap_or(&username)
    ))?;
    out.line(format_args!("Cached members: {}", roster.usernames.len()))?;
    out.line(format_args!("Locally confirmed friends: {}", matches.len()))?;
    out.line(format_args!("Note: conservative local relationship evidence only; not a live or reciprocal friendship check."))?;
    out.blank_line()?;
    out.line(format_args!("{:<24} Username", "Display Name"))?;
    for (name, id) in &matches {
        out.line(format_args!(
            "{:<24} {}",
            name.escape_debug().to_string(),
            id.escape_debug()
        ))?;
    }
    out.flush()?;
    Ok(matches.into_iter().map(|(_, id)| id).collect())
}

/// List current group members and their locally recorded speaking activity.
pub fn list_members(
    store: &SourceStore,
    options: ListMembersOptions<'_>,
) -> Result<Vec<MemberActivity>, String> {
    let contact_conn = store
        .open(CONTACT_DB)
        .map_err(|error| format!("Contact database is unavailable: {error}"))?;
    let contacts = contact::load_contacts(&contact_conn)?;
    let message_dbs = message_databases(store);
    let username = resolve_username_for_messages(
        options.session_query,
        &contacts,
        store,
        &message_dbs,
        options.jobs,
    )?;
    if !username.ends_with("@chatroom") {
        return Err(format!("Not a group session: {username}"));
    }

    let roster = contact::load_chat_room_roster(&contact_conn, &username)?;
    if roster.usernames.is_empty() {
        return Err(format!("No current members found for group: {username}"));
    }
    let room_member_names = if options.name_mode == DisplayNameMode::Anonymous {
        contact::load_chat_room_member_names(&contact_conn, &username)?
    } else {
        HashMap::new()
    };
    let speech = load_member_speech(store, &message_dbs, &username, options.jobs)?;
    let history_start = speech.values().map(|row| row.first_spoke).min();
    let history_end = speech.values().map(|row| row.last_spoke).max();
    let group_display = contacts
        .get(&username)
        .map(|value| value.display_name(options.name_mode))
        .unwrap_or(&username);

    let total_members = roster.usernames.len();
    let mut members = roster
        .usernames
        .into_iter()
        .map(|member_username| {
            let activity = speech.get(&member_username);
            MemberActivity {
                display_name: contact::resolve_sender_name_with_mode(
                    &member_username,
                    &contacts,
                    options.name_mode,
                    &room_member_names,
                ),
                is_owner: roster.owner.as_deref() == Some(member_username.as_str()),
                last_spoke: activity.map(|row| row.last_spoke),
                message_count: activity.map(|row| row.message_count).unwrap_or(0),
                username: member_username,
            }
        })
        .filter(|member| {
            options
                .inactive_since
                .is_none_or(|cutoff| member.last_spoke.is_none_or(|last| last < cutoff))
        })
        .collect::<Vec<_>>();
    members.sort_by(|left, right| {
        left.last_spoke
            .cmp(&right.last_spoke)
            .then_with(|| left.display_name.cmp(&right.display_name))
            .then_with(|| left.username.cmp(&right.username))
    });

    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());
    out.line(format_args!("Group: {group_display} ({username})"))?;
    out.line(format_args!("Current members: {total_members}"))?;
    match (history_start, history_end) {
        (Some(start), Some(end)) => out.line(format_args!(
            "Local sender history: {} to {}",
            time::format_local_timestamp_minutes(start),
            time::format_local_timestamp_minutes(end)
        ))?,
        _ => out.line(format_args!("Local sender history: none"))?,
    }
    if let Some(cutoff) = options.inactive_since {
        out.line(format_args!(
            "Filter: no recorded messages since {}",
            time::format_local_timestamp_minutes(cutoff)
        ))?;
        out.line(format_args!(
            "Note: the current roster has no join times; review recent joiners before removal."
        ))?;
        if history_start.is_none_or(|start| start > cutoff) {
            out.line(format_args!(
                "Warning: local sender history does not reach the cutoff; this cannot prove a full period of silence."
            ))?;
        }
    }
    out.blank_line()?;
    out.line(format_args!(
        "{:<4} {:<20} {:<10} {:<7} {:<22} Username",
        "Rank", "Last Spoke", "Messages", "Role", "Display Name"
    ))?;
    out.line(format_args!("{}", "-".repeat(130)))?;
    for (index, member) in members.iter().enumerate() {
        let last_spoke = member
            .last_spoke
            .map(time::format_local_timestamp_minutes)
            .unwrap_or_else(|| "never recorded".to_string());
        out.line(format_args!(
            "{:<4} {:<20} {:<10} {:<7} {:<22} {}",
            index + 1,
            last_spoke,
            member.message_count,
            if member.is_owner { "owner" } else { "" },
            member.display_name,
            member.username
        ))?;
    }
    out.blank_line()?;
    if options.inactive_since.is_some() {
        out.line(format_args!(
            "Candidates: {} of {} current members (local records only)",
            members.len(),
            total_members
        ))?;
    } else {
        out.line(format_args!("Listed: {} current members", members.len()))?;
    }
    out.flush()?;
    Ok(members)
}

fn load_member_speech(
    store: &SourceStore,
    message_dbs: &[String],
    username: &str,
    jobs: usize,
) -> Result<HashMap<String, MemberSpeech>, String> {
    let table_name = msg_table_name(username);
    let db_jobs = parallel::job_count(jobs, parallel::SOURCE_DB_AUTO_CAP);
    let per_db = parallel::map_ordered(
        message_dbs.to_vec(),
        db_jobs,
        |db_path| -> Result<HashMap<String, MemberSpeech>, String> {
            let conn = store.open(&db_path)?;
            if !table_exists(&conn, &table_name)? {
                return Ok(HashMap::new());
            }
            let name2id = load_name2id(&conn)?;
            let sender_col = quote_identifier(dictionary::msg_sender_column());
            let sql = format!(
                "SELECT {sender_col}, MIN(create_time), MAX(create_time), COUNT(*)
                 FROM {table}
                 WHERE create_time > 0 AND {sender_col} IS NOT NULL AND {sender_col} != 0
                 GROUP BY {sender_col}",
                table = quote_identifier(&table_name),
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|error| format!("Prepare member activity in {db_path}: {error}"))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })
                .map_err(|error| format!("Read member activity in {db_path}: {error}"))?;
            let mut activity = HashMap::new();
            for row in rows {
                let (sender_id, first_spoke, last_spoke, message_count) =
                    row.map_err(|error| format!("Read member activity row in {db_path}: {error}"))?;
                let Some(sender) = name2id.get(&sender_id) else {
                    return Err(format!(
                        "Cannot resolve sender identity {sender_id} in {db_path}; refusing a partial inactivity report"
                    ));
                };
                activity.insert(
                    sender.clone(),
                    MemberSpeech {
                        first_spoke,
                        last_spoke,
                        message_count,
                    },
                );
            }
            Ok(activity)
        },
    );

    let mut activity: HashMap<String, MemberSpeech> = HashMap::new();
    for db_activity in per_db {
        for (username, row) in db_activity? {
            if let Some(entry) = activity.get_mut(&username) {
                entry.first_spoke = entry.first_spoke.min(row.first_spoke);
                entry.last_spoke = entry.last_spoke.max(row.last_spoke);
                entry.message_count = entry
                    .message_count
                    .checked_add(row.message_count)
                    .ok_or_else(|| "Member message count overflow".to_string())?;
            } else {
                activity.insert(username, row);
            }
        }
    }
    Ok(activity)
}

fn list_sessions_from_connection(
    conn: &Connection,
    contacts: &HashMap<String, Contact>,
    top_n: usize,
    query: Option<&str>,
    mode: SessionDbListMode,
) -> Result<SessionListRows, String> {
    let sql = format!(
        "SELECT username,
                COALESCE(unread_count, 0),
                COALESCE(last_timestamp, 0),
                COALESCE(sort_timestamp, 0)
         FROM SessionTable
         {}",
        mode.where_clause()
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("Prepare session list: {}", e))?;

    let rows = stmt
        .query_map([], |row| {
            let username: String = row.get(0)?;
            let unread_count: i64 = row.get::<_, Option<i64>>(1)?.unwrap_or(0).max(0);
            let last_timestamp: i64 = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
            let sort_timestamp: i64 = row.get::<_, Option<i64>>(3)?.unwrap_or(0);
            let activity = sort_timestamp.max(last_timestamp);
            Ok((username, unread_count, activity))
        })
        .map_err(|e| format!("Read sessions: {}", e))?;
    let session_rows = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("Read sessions: {}", e))?;

    let mut entries = session_rows
        .into_iter()
        .filter(|(username, _, activity)| !username.trim().is_empty() && *activity > 0)
        .map(|(username, unread_count, activity)| {
            let display = contacts
                .get(&username)
                .map(|c| c.personal_display_name().to_string())
                .unwrap_or_else(|| username.clone());

            SessionListEntry {
                username,
                info: SessionInfo {
                    count: unread_count,
                    latest: Some(activity),
                },
                display,
                match_info: None,
            }
        })
        .collect::<Vec<_>>();

    let total_sessions = entries.len();
    let query = query.map(str::trim).filter(|value| !value.is_empty());
    if let Some(query) = query {
        entries = entries
            .into_iter()
            .filter_map(|mut entry| {
                let match_info =
                    session_match_info(contacts, &entry.username, &entry.display, query)?;
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
                .then_with(|| a.display.cmp(&b.display))
                .then_with(|| a.username.cmp(&b.username))
        });
    } else {
        entries.sort_by_key(|entry| Reverse(entry.info.latest.unwrap_or_default()));
    }

    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let matched_sessions = entries.len();

    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());

    out.line(format_args!(
        "{:<4} {:<8} {:<20} {:<22} Username",
        "Rank", "Unread", "Last Activity", "Display Name"
    ))?;
    out.line(format_args!("{}", "-".repeat(120)))?;

    let mut result = Vec::new();
    for (i, entry) in entries.iter().enumerate().take(top_n) {
        let activity = entry
            .info
            .latest
            .map(time::format_local_timestamp_minutes)
            .unwrap_or_default();

        out.line(format_args!(
            "{:<4} {:<8} {:<20} {:<22} {}",
            i + 1,
            entry.info.count,
            activity,
            entry.display,
            entry.username
        ))?;

        result.push((
            entry.username.clone(),
            entry.info.count,
            activity,
            entry.display.clone(),
        ));
    }

    out.blank_line()?;
    if query.is_some() {
        out.line(format_args!(
            "Matched: {} of {} {}",
            matched_sessions,
            total_sessions,
            mode.total_label()
        ))?;
    } else {
        out.line(format_args!(
            "Total: {} {}",
            total_sessions,
            mode.total_label()
        ))?;
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
        return best_contact_score(contact, query).map(|score| SessionMatchInfo {
            score: score.score,
            field_priority: score.field_priority,
            distance: score.distance,
            field_len: score.field_len,
        });
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
    store: &SourceStore,
    options: ReadMessagesOptions<'_>,
) -> Result<usize, String> {
    let message_dbs = message_databases(store);
    let contact_conn = open_optional_database(store, CONTACT_DB)?;
    let contacts = match contact_conn.as_ref() {
        Some(conn) => contact::load_contacts(conn)?,
        None => HashMap::new(),
    };
    let username = resolve_username_for_messages(
        options.session_query,
        &contacts,
        store,
        &message_dbs,
        options.jobs,
    )?;
    let table_name = msg_table_name(&username);

    let display_name = contacts
        .get(&username)
        .map(|c| c.display_name(options.name_mode))
        .unwrap_or(&username)
        .to_string();

    let room_member_names = if options.name_mode == DisplayNameMode::Anonymous {
        match contact_conn.as_ref() {
            Some(conn) => contact::load_chat_room_member_names(conn, &username)?,
            None => HashMap::new(),
        }
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

    let query_limit = query_window_limit(options.limit, options.offset, options.tail);
    let db_jobs = parallel::job_count(options.jobs, parallel::SOURCE_DB_AUTO_CAP);
    let per_db_messages = parallel::map_ordered(
        message_dbs,
        db_jobs,
        |db_path| -> Result<Vec<MessageRow>, String> {
            let conn = store.open(&db_path)?;

            let table = quote_identifier(&table_name);
            if !table_exists(&conn, &table_name)? {
                return Ok(Vec::new());
            }

            let name2id = load_name2id(&conn)?;

            let order_dir = if options.tail { "DESC" } else { "ASC" };

            // Query messages - collect eagerly to avoid borrow issues
            let body_col = quote_identifier(dictionary::msg_body_column());
            let marker_col = quote_identifier(dictionary::msg_compression_marker_column());
            let sender_col = quote_identifier(dictionary::msg_sender_column());
            let packed_col = quote_identifier(dictionary::msg_packed_meta_column());
            let local_id_col = if table_has_column(&conn, &table_name, "local_id")? {
                quote_identifier("local_id")
            } else {
                "NULL".to_string()
            };
            let order_col = if table_has_column(&conn, &table_name, "sort_seq")? {
                "sort_seq"
            } else {
                "create_time"
            };
            let sql = format!(
            "SELECT {local_id_col}, local_type, create_time, {body_col}, {marker_col}, {sender_col}, {packed_col} \
             FROM {table_name} WHERE create_time > 0{search_clause}{since_clause} ORDER BY {order_col} {order_dir}{limit_clause}",
            local_id_col = local_id_col,
            body_col = body_col,
            marker_col = marker_col,
            sender_col = sender_col,
            packed_col = packed_col,
            table_name = table,
            order_col = order_col,
            search_clause = search_clause,
            since_clause = since_clause,
            order_dir = order_dir,
            limit_clause = query_limit
                .map(|n| format!(" LIMIT {}", n))
                .unwrap_or_default()
        );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|error| format!("Prepare messages from {db_path}: {error}"))?;
            let map_row = |row: &rusqlite::Row<'_>| {
                let compression_marker: Option<i64> = row.get::<_, Option<i64>>(4)?;
                let content = read_message_body(row, 3, compression_marker)?;
                let sender_id: i64 = row.get::<_, Option<i64>>(5)?.unwrap_or(0);
                let sender_account_id = name2id.get(&sender_id).cloned().unwrap_or_default();
                let packed_info: Vec<u8> = row.get::<_, Option<Vec<u8>>>(6)?.unwrap_or_default();
                Ok(MessageRow {
                    local_id: row.get::<_, Option<i64>>(0)?,
                    local_type: row.get::<_, Option<i64>>(1)?.unwrap_or(-1),
                    create_time: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    content,
                    compression_marker,
                    sender_account_id,
                    packed_info,
                })
            };
            let rows = match &search_pattern {
                Some(pattern) => stmt.query_map(params![pattern], map_row),
                None => stmt.query_map([], map_row),
            }
            .map_err(|error| format!("Query messages from {db_path}: {error}"))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|error| format!("Read messages from {db_path}: {error}"))
        },
    );

    let mut all_messages = Vec::new();
    for rows in per_db_messages {
        all_messages.extend(rows?);
    }

    if options.tail {
        // Each DB returns its latest rows; merge globally, then reverse for chronological display.
        all_messages.sort_by_key(|row| Reverse(row.create_time));
        if let Some(limit) = options.limit {
            all_messages.truncate(limit);
        }
        all_messages.reverse();
    } else {
        all_messages.sort_by_key(|row| row.create_time);
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
    let displayed_count = messages.len();

    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());

    out.line(format_args!("Chat with: {} ({})", display_name, username))?;
    if let Some(q) = options.search_query {
        out.line(format_args!("Search: '{}'", q))?;
    }
    if options.tail {
        if options.limit.is_some() {
            out.line(format_args!("Showing latest {} messages", messages.len()))?;
        } else {
            out.line(format_args!("Showing {} messages", messages.len()))?;
        }
    } else if options.limit.is_some() || options.offset > 0 {
        out.line(format_args!(
            "Showing messages {}-{}",
            options.offset + 1,
            options.offset + messages.len()
        ))?;
    } else {
        out.line(format_args!("Showing {} messages", messages.len()))?;
    }
    out.blank_line()?;

    let mut last_time_key = None;
    let mut last_sender: Option<String> = None;
    for row in messages {
        // 1-on-1 chat: if the sender is not the chat partner, it's "me"
        let sender_display =
            if row.sender_account_id.is_empty() || row.sender_account_id == username {
                display_name.as_str()
            } else {
                "我"
            };

        let voice_id = if row.local_type == 34 {
            row.local_id.filter(|id| *id > 0)
        } else {
            None
        };
        let decoded = message::decode_message_with_context(
            row.local_type as i32,
            &row.content,
            sender_display,
            row.compression_marker,
            &row.packed_info,
            message::DecodeContext {
                time_bucket: options.time_bucket,
                voice_id,
            },
            |id| {
                contact::resolve_sender_name_with_mode(
                    id,
                    &contacts,
                    options.name_mode,
                    &room_member_names,
                )
            },
        );

        if options.time_bucket == time::MessageTimeBucket::PerMessage {
            out.line(format_args!(
                "[{}] {}: {}",
                time::format_local_timestamp(row.create_time),
                decoded.display_name,
                decoded.content
            ))?;
        } else {
            if let Some(time_key) = time::message_time_key(row.create_time, options.time_bucket) {
                if last_time_key != Some(time_key) {
                    out.line(format_args!(
                        "[{}]",
                        time::format_message_time_bucket(row.create_time, options.time_bucket)
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
    Ok(displayed_count)
}

fn query_window_limit(limit: Option<usize>, offset: usize, tail: bool) -> Option<usize> {
    if tail {
        limit
    } else {
        limit.and_then(|n| n.checked_add(offset))
    }
}

pub(crate) fn probe_session(
    store: &SourceStore,
    session_query: &str,
    jobs: usize,
) -> Result<SessionProbe, String> {
    let message_dbs = message_databases(store);
    let contact_conn = open_optional_database(store, CONTACT_DB)?;
    let contacts = match contact_conn.as_ref() {
        Some(conn) => contact::load_contacts(conn)?,
        None => HashMap::new(),
    };
    let username =
        resolve_username_for_messages(session_query, &contacts, store, &message_dbs, jobs)?;
    let table_name = msg_table_name(&username);
    let display_name = contacts
        .get(&username)
        .map(|contact| contact.personal_display_name().to_string())
        .unwrap_or_else(|| username.clone());

    let db_jobs = parallel::job_count(jobs, parallel::SOURCE_DB_AUTO_CAP);
    let per_db_counts = parallel::map_ordered(
        message_dbs,
        db_jobs,
        |db_path| -> Result<Option<usize>, String> {
            let conn = store.open(&db_path)?;
            if !table_exists(&conn, &table_name)? {
                return Ok(None);
            }
            let sql = format!(
                "SELECT COUNT(*) FROM {} WHERE create_time > 0",
                quote_identifier(&table_name)
            );
            conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
                .map(|count| Some(count.max(0) as usize))
                .map_err(|error| format!("Count messages in {db_path}: {error}"))
        },
    );

    let mut matching_dbs = 0usize;
    let mut message_count = 0usize;
    for count in per_db_counts {
        if let Some(count) = count? {
            matching_dbs += 1;
            message_count += count;
        }
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
    store: &SourceStore,
    options: SearchMessagesOptions<'_>,
) -> Result<usize, String> {
    let contact_conn = open_optional_database(store, CONTACT_DB)?;
    let contacts = match contact_conn.as_ref() {
        Some(conn) => contact::load_contacts(conn)?,
        None => HashMap::new(),
    };
    let (displayed, has_more, results) = search_messages_with_fts(store, &options)?;

    print_search_results(
        options.query,
        displayed,
        has_more,
        results,
        &contacts,
        options.name_mode,
    )
}

fn search_messages_with_fts(
    store: &SourceStore,
    options: &SearchMessagesOptions<'_>,
) -> Result<(usize, bool, Vec<SearchRow>), String> {
    let conn = store
        .open(MESSAGE_FTS_DB)
        .map_err(|error| format!("Message search index is unavailable: {error}"))?;
    let content_tables = telegram_fts_content_tables(&conn)?;
    if content_tables.is_empty() {
        return Err("Message search index has no supported FTS tables".to_string());
    }

    let name2id = load_fts_name2id(&conn)?;
    let result_limit = options.limit.saturating_add(1);
    let match_query = fts::ideograph_match_query(options.query)
        .filter(|_| fts::register_compatible_tokenizer(&conn).is_ok());
    let mut results = match match_query {
        Some(query) => collect_telegram_fts_rows(
            &conn,
            &content_tables,
            &name2id,
            options,
            result_limit,
            Some(query),
        )
        .or_else(|match_error| {
            collect_telegram_fts_rows(
                &conn,
                &content_tables,
                &name2id,
                options,
                result_limit,
                None,
            )
            .map_err(|fallback_error| {
                format!(
                    "Search message index failed ({match_error}); content fallback failed: {fallback_error}"
                )
            })
        })?,
        None => collect_telegram_fts_rows(
            &conn,
            &content_tables,
            &name2id,
            options,
            result_limit,
            None,
        )?,
    };

    results.sort_by_key(|row| Reverse(row.1));
    let has_more = results.len() > options.limit;
    results.truncate(options.limit);
    let displayed = results.len();
    results.sort_by_key(|row| row.1);

    Ok((displayed, has_more, results))
}

fn collect_telegram_fts_rows(
    conn: &Connection,
    content_tables: &[String],
    name2id: &HashMap<i64, String>,
    options: &SearchMessagesOptions<'_>,
    result_limit: usize,
    match_query: Option<&str>,
) -> Result<Vec<SearchRow>, String> {
    let search_pattern = like_contains_pattern(options.query);
    let mut results = Vec::new();

    for content_table in content_tables {
        let (table, columns, search_clause, time_column, search_value) =
            if let Some(query) = match_query {
                let table = content_table
                    .strip_suffix("_content")
                    .ok_or_else(|| format!("Invalid FTS content table name: {content_table}"))?;
                let quoted = quote_identifier(table);
                (
                    quoted.clone(),
                    "local_type, create_time, acontent, session_id",
                    format!("{} MATCH ?1", quoted),
                    "create_time",
                    query,
                )
            } else {
                (
                    quote_identifier(content_table),
                    "c3, c6, c0, c4",
                    fts_content_like_clause().to_string(),
                    "c6",
                    search_pattern.as_str(),
                )
            };
        let since_clause = options
            .since
            .map(|ts| format!(" AND {time_column} >= {ts}"))
            .unwrap_or_default();
        let sql = format!(
            "SELECT {columns} FROM {table} \
             WHERE {search_clause}{since_clause} \
             ORDER BY {time_column} DESC LIMIT {result_limit}"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|error| format!("Prepare search of {content_table}: {error}"))?;
        let rows = stmt
            .query_map(params![search_value], |row| {
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
            .map_err(|error| format!("Query search index table {content_table}: {error}"))?;
        let rows = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| format!("Read search index table {content_table}: {error}"))?;
        results.extend(rows);
    }

    if match_query.is_some() {
        results.retain(|row| row.2.contains(options.query));
    }
    Ok(results)
}

fn print_search_results(
    query: &str,
    displayed: usize,
    has_more: bool,
    results: Vec<SearchRow>,
    contacts: &HashMap<String, Contact>,
    name_mode: DisplayNameMode,
) -> Result<usize, String> {
    if displayed == 0 {
        return Ok(0);
    }

    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());

    if has_more {
        out.line(format_args!(
            "Search results for '{}': showing latest {} matches (more available)",
            query, displayed
        ))?;
    } else {
        out.line(format_args!(
            "Search results for '{}': {} matches",
            query, displayed
        ))?;
    }
    out.blank_line()?;

    let display_results = &results;

    for (i, (_, create_time, content, table_name)) in display_results.iter().enumerate() {
        let time_str = time::format_local_timestamp(*create_time);

        let display = search_session_display(contacts, table_name, name_mode);

        let display_content = truncate_preview(content, SEARCH_PREVIEW_CHARS);

        out.line(format_args!(
            "[{}] {} | {}: {}",
            i + 1,
            time_str,
            display,
            display_content
        ))?;
    }

    if has_more {
        out.line(format_args!("... more older results available"))?;
    }

    out.flush()?;
    Ok(displayed)
}

fn telegram_fts_content_tables(conn: &Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type='table' AND name LIKE 'message_fts_v4_%_content' \
             ORDER BY name",
        )
        .map_err(|error| format!("Inspect message search index tables: {error}"))?;
    let tables: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| format!("Query message search index tables: {error}"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| format!("Read message search index tables: {error}"))?
        .into_iter()
        .filter(|name| {
            name.starts_with(TELEGRAM_FTS_CONTENT_TABLE_PREFIX)
                && name.ends_with(TELEGRAM_FTS_CONTENT_TABLE_SUFFIX)
                && name[TELEGRAM_FTS_CONTENT_TABLE_PREFIX.len()
                    ..name.len() - TELEGRAM_FTS_CONTENT_TABLE_SUFFIX.len()]
                    .chars()
                    .all(|c| c.is_ascii_digit())
        })
        .collect();
    Ok(tables)
}

fn load_fts_name2id(conn: &Connection) -> Result<HashMap<i64, String>, String> {
    let mut stmt = conn
        .prepare("SELECT rowid, username FROM name2id")
        .map_err(|error| format!("Prepare message search session map: {error}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| format!("Query message search session map: {error}"))?;
    rows.collect::<rusqlite::Result<HashMap<_, _>>>()
        .map_err(|error| format!("Read message search session map: {error}"))
}

fn search_session_display(
    contacts: &HashMap<String, Contact>,
    session: &str,
    name_mode: DisplayNameMode,
) -> String {
    if session.starts_with("Msg_") {
        return find_username_by_table(contacts, session).unwrap_or_else(|| "(?)".to_string());
    }

    contacts
        .get(session)
        .map(|c| c.display_name(name_mode).to_string())
        .unwrap_or_else(|| session.to_string())
}

fn msg_body_like_clause() -> String {
    format!(
        "{} LIKE ? ESCAPE '\\'",
        quote_identifier(dictionary::msg_body_column())
    )
}

fn fts_content_like_clause() -> &'static str {
    "c0 LIKE ? ESCAPE '\\'"
}

pub(crate) fn like_contains_pattern(query: &str) -> String {
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
    contacts: &HashMap<String, Contact>,
    store: &SourceStore,
    message_dbs: &[String],
    jobs: usize,
) -> Result<String, String> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(query.to_string());
    }

    // If it looks like a native account id, use it directly.
    if query.starts_with(dictionary::account_id_prefix())
        || query.starts_with("gh_")
        || query.contains("@chatroom")
    {
        return Ok(query.to_string());
    }

    resolve_username_from_contacts(query, contacts, store, Some(message_dbs), jobs)
}

fn resolve_username_from_contacts(
    query: &str,
    contacts: &HashMap<String, Contact>,
    store: &SourceStore,
    message_dbs: Option<&[String]>,
    jobs: usize,
) -> Result<String, String> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(query.to_string());
    }

    if query.starts_with(dictionary::account_id_prefix())
        || query.starts_with("gh_")
        || query.contains("@chatroom")
    {
        return Ok(query.to_string());
    }

    // Try exact username match
    if let Some(c) = contacts.get(query) {
        return Ok(c.username.clone());
    }

    if let Some(username) = unique_exact_direct_username(contacts, query) {
        return Ok(username);
    }

    let candidates = contact_match_candidates(contacts, query, store, message_dbs, jobs)?;
    if let Some(best) = candidates.first() {
        if candidates.len() > 1 || best.score < EXACT_MATCH_SCORE {
            log::info!(
                "Matched '{}' to {} ({}){}",
                query,
                best.contact.personal_display_name(),
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

#[derive(Clone, Copy)]
pub(crate) struct ContactFieldScore {
    pub score: i64,
    pub field_priority: usize,
    pub distance: usize,
    pub field_len: usize,
}

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
    store: &SourceStore,
    message_dbs: Option<&[String]>,
    jobs: usize,
) -> Result<Vec<ContactMatch<'a>>, String> {
    let mut candidates: Vec<_> = contacts
        .values()
        .filter_map(|contact| {
            let score = best_contact_score(contact, query)?;
            Some(ContactMatch {
                contact,
                score: score.score,
                field_priority: score.field_priority,
                distance: score.distance,
                field_len: score.field_len,
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
            latest_message_times_for_candidates(store, message_dbs, &candidates, jobs)?;
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
            .then_with(|| {
                a.contact
                    .personal_display_name()
                    .cmp(b.contact.personal_display_name())
            })
            .then_with(|| a.contact.username.cmp(&b.contact.username))
    });
    Ok(candidates)
}

fn unique_exact_direct_username(
    contacts: &HashMap<String, Contact>,
    query: &str,
) -> Option<String> {
    let mut direct_matches = 0usize;
    let mut exact_username = None;

    for contact in contacts.values() {
        let Some(score) = best_contact_score(contact, query) else {
            continue;
        };
        if score.score < DIRECT_MATCH_SCORE_FLOOR {
            continue;
        }

        direct_matches += 1;
        if score.score == EXACT_MATCH_SCORE {
            exact_username = Some(contact.username.clone());
        }
        if direct_matches > 1 {
            return None;
        }
    }

    exact_username
}

fn best_contact_score(contact: &Contact, query: &str) -> Option<ContactFieldScore> {
    let fields = contact_match_fields(contact);
    best_contact_field_score(&fields, query)
}

pub(crate) fn best_contact_field_score(
    fields: &[(usize, &str)],
    query: &str,
) -> Option<ContactFieldScore> {
    let normalized_query = normalize_match_text(query);
    if normalized_query.is_empty() {
        return None;
    }
    let query_tokens = tokenize_match_text(query);

    fields
        .iter()
        .filter_map(|(priority, field)| {
            field_score(field, query, &normalized_query, &query_tokens).map(
                |(score, distance, field_len)| ContactFieldScore {
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

fn contact_match_fields(contact: &Contact) -> [(usize, &str); 5] {
    [
        (0, contact.personal_display_name()),
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
    store: &SourceStore,
    message_dbs: &[String],
    candidates: &[ContactMatch<'_>],
    jobs: usize,
) -> Result<HashMap<String, i64>, String> {
    if candidates.is_empty() {
        return Ok(HashMap::new());
    }
    if has_database(store, SESSION_DB) {
        let conn = store.open(SESSION_DB)?;
        let mut stmt = conn
            .prepare(
                "SELECT MAX(COALESCE(last_timestamp, 0), COALESCE(sort_timestamp, 0))
                 FROM SessionTable WHERE username = ?1",
            )
            .map_err(|error| format!("Prepare session activity lookup: {error}"))?;
        let mut latest = HashMap::new();
        for candidate in candidates {
            let timestamp = stmt
                .query_row([&candidate.contact.username], |row| row.get::<_, i64>(0))
                .optional()
                .map_err(|error| format!("Read session activity: {error}"))?;
            if let Some(timestamp) = timestamp.filter(|timestamp| *timestamp > 0) {
                latest.insert(candidate.contact.username.clone(), timestamp);
            }
        }
        return Ok(latest);
    }

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

    let db_jobs = parallel::job_count(jobs, parallel::SOURCE_DB_AUTO_CAP);
    let per_db_latest = parallel::map_ordered(
        message_dbs.to_vec(),
        db_jobs,
        |db_path| -> Result<HashMap<String, i64>, String> {
            let mut latest_by_username: HashMap<String, i64> = HashMap::new();
            let conn = store.open(&db_path)?;

            let tables = list_message_tables(&conn)?;

            for table_name in tables {
                let username = match table_to_username.get(&table_name) {
                    Some(username) => username,
                    None => continue,
                };
                let sql = format!(
                    "SELECT MAX(create_time) FROM {} WHERE create_time > 0",
                    quote_identifier(&table_name)
                );
                let mut stmt = conn.prepare(&sql).map_err(|error| {
                    format!("Prepare latest-message query in {db_path}: {error}")
                })?;
                if let Some(ts) = stmt
                    .query_row([], |row| row.get::<_, Option<i64>>(0))
                    .map_err(|error| format!("Read latest message in {db_path}: {error}"))?
                {
                    let entry = latest_by_username.entry(username.clone()).or_insert(ts);
                    if ts > *entry {
                        *entry = ts;
                    }
                }
            }
            Ok(latest_by_username)
        },
    );

    for db_latest in per_db_latest {
        for (username, ts) in db_latest? {
            let entry = latest_by_username.entry(username).or_insert(ts);
            if ts > *entry {
                *entry = ts;
            }
        }
    }

    Ok(latest_by_username)
}

fn format_match_time(timestamp: i64) -> String {
    time::format_local_timestamp(timestamp)
}

fn find_username_by_table(contacts: &HashMap<String, Contact>, table_name: &str) -> Option<String> {
    for username in contacts.keys() {
        if msg_table_name(username) == table_name {
            return Some(format!(
                "{} ({})",
                contacts.get(username)?.personal_display_name(),
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
    use std::path::{Path, PathBuf};
    use tempfile::{tempdir, TempDir};

    fn list_sessions(
        root: &Path,
        top_n: usize,
        query: Option<&str>,
    ) -> Result<SessionListRows, String> {
        let store = SourceStore::from_plaintext(root)?;
        super::list_sessions(&store, top_n, query)
    }

    fn list_unread_sessions(
        root: &Path,
        top_n: usize,
        query: Option<&str>,
    ) -> Result<SessionListRows, String> {
        let store = SourceStore::from_plaintext(root)?;
        super::list_unread_sessions(&store, top_n, query)
    }

    fn read_messages(root: &Path, options: ReadMessagesOptions<'_>) -> Result<usize, String> {
        let store = SourceStore::from_plaintext(root)?;
        super::read_messages(&store, options)
    }

    fn search_messages(root: &Path, options: SearchMessagesOptions<'_>) -> Result<usize, String> {
        let store = SourceStore::from_plaintext(root)?;
        super::search_messages(&store, options)
    }

    fn load_contacts(path: &Path) -> HashMap<String, Contact> {
        let conn = Connection::open(path).unwrap();
        contact::load_contacts(&conn).unwrap()
    }

    fn resolve_username_for_test(
        query: &str,
        contact_db: &Path,
        message_db: Option<&Path>,
        jobs: usize,
    ) -> Result<String, String> {
        let contacts = load_contacts(contact_db);
        let paths = message_db
            .map(|path| vec![("message_0.db".to_string(), path.to_path_buf())])
            .unwrap_or_default();
        let store = SourceStore::from_plaintext_paths(paths);
        let labels = message_db
            .map(|_| vec!["message_0.db".to_string()])
            .unwrap_or_default();
        super::resolve_username_for_messages(query, &contacts, &store, &labels, jobs)
    }

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
            let body_col = quote_identifier(dictionary::msg_body_column());
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

    fn create_decrypted_dir_with_session_table(
        contacts: &[(&str, &str, &str, &str)],
        rows: &[(&str, i64, i64, i64)],
    ) -> TempDir {
        let dir = tempdir().unwrap();
        let contact_dir = dir.path().join("contact");
        let session_dir = dir.path().join("session");
        std::fs::create_dir_all(&contact_dir).unwrap();
        std::fs::create_dir_all(&session_dir).unwrap();

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

        let session_conn = Connection::open(session_dir.join("session.db")).unwrap();
        session_conn
            .execute(
                "CREATE TABLE SessionTable (
                    username TEXT PRIMARY KEY,
                    unread_count INTEGER,
                    last_timestamp INTEGER,
                    sort_timestamp INTEGER
                )",
                [],
            )
            .unwrap();
        for (username, unread_count, last_timestamp, sort_timestamp) in rows {
            session_conn
                .execute(
                    "INSERT INTO SessionTable
                     (username, unread_count, last_timestamp, sort_timestamp)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![username, unread_count, last_timestamp, sort_timestamp],
                )
                .unwrap();
        }
        drop(session_conn);

        dir
    }

    fn create_decrypted_dir_with_messages(username: &str, contents: &[&str]) -> TempDir {
        let dir = tempdir().unwrap();
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();
        let path = message_dir.join("message_0.db");
        let conn = Connection::open(&path).unwrap();
        let table_name = msg_table_name(username);
        let body_col = quote_identifier(dictionary::msg_body_column());
        let marker_col = quote_identifier(dictionary::msg_compression_marker_column());
        let sender_col = quote_identifier(dictionary::msg_sender_column());
        let packed_col = quote_identifier(dictionary::msg_packed_meta_column());
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
        crate::fts::register_compatible_tokenizer(&conn).unwrap();
        conn.execute("CREATE TABLE name2id(username TEXT PRIMARY KEY)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO name2id (rowid, username) VALUES (7, 'tgid_fts_session')",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE message_fts_v4_0 USING fts5(
                 tokenize = 'MMFtsTokenizer disable_pinyin',
                 acontent,
                 message_local_id UNINDEXED,
                 sort_seq UNINDEXED,
                 local_type UNINDEXED,
                 session_id UNINDEXED,
                 sender_id UNINDEXED,
                 create_time UNINDEXED
             )",
        )
        .unwrap();

        for (i, content) in contents.iter().enumerate() {
            conn.execute(
                "INSERT INTO message_fts_v4_0 (
                     rowid, acontent, message_local_id, sort_seq,
                     local_type, session_id, sender_id, create_time
                 ) VALUES (?1, ?2, ?1, 0, 1, 7, 0, ?3)",
                params![i as i64 + 1, content, 1000 + i as i64],
            )
            .unwrap();
        }

        drop(conn);
        dir
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
    fn malformed_identity_and_body_rows_fail_closed() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE Name2Id(user_name INTEGER)", [])
            .unwrap();
        conn.execute("INSERT INTO Name2Id VALUES (7)", []).unwrap();
        assert!(load_name2id(&conn).is_err());

        conn.execute("CREATE TABLE bad_body(value INTEGER)", [])
            .unwrap();
        conn.execute("INSERT INTO bad_body VALUES (9)", []).unwrap();
        assert!(conn
            .query_row("SELECT value FROM bad_body", [], |row| {
                read_message_body(row, 0, None)
            })
            .is_err());
    }

    #[test]
    fn list_sessions_reads_session_db_without_message_dbs() {
        let dir = create_decrypted_dir_with_session_table(
            &[
                ("tgid_old", "Old Nick", "Old Remark", ""),
                ("tgid_recent", "Recent Nick", "", ""),
            ],
            &[("tgid_old", 0, 1000, 1000), ("tgid_recent", 3, 2000, 2100)],
        );

        let sessions = list_sessions(dir.path(), 10, None).unwrap();

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].0, "tgid_recent");
        assert_eq!(sessions[0].1, 3);
        assert_eq!(sessions[0].3, "Recent Nick");
        assert_eq!(sessions[1].0, "tgid_old");
        assert_eq!(sessions[1].1, 0);
        assert_eq!(sessions[1].3, "Old Remark");
    }

    #[test]
    fn list_sessions_requires_session_db() {
        let dir = tempdir().unwrap();

        let error = list_sessions(dir.path(), 10, None).unwrap_err();

        assert!(error.contains("Session database is unavailable"));
    }

    #[test]
    fn list_sessions_filters_session_db_by_contact_query() {
        let dir = create_decrypted_dir_with_session_table(
            &[
                ("tgid_alice", "Alice Zhang", "", ""),
                ("tgid_bob", "Bob Lee", "", ""),
            ],
            &[("tgid_alice", 0, 1000, 1000), ("tgid_bob", 0, 2000, 2000)],
        );

        let sessions = list_sessions(dir.path(), 10, Some("Alic Zhang")).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0, "tgid_alice");
        assert_eq!(sessions[0].3, "Alice Zhang");
    }

    #[test]
    fn list_unread_sessions_returns_only_unread_from_session_db() {
        let dir = create_decrypted_dir_with_session_table(
            &[
                ("tgid_read", "Read Nick", "", ""),
                ("tgid_unread", "Unread Nick", "", ""),
            ],
            &[("tgid_read", 0, 2000, 2000), ("tgid_unread", 2, 1000, 3000)],
        );

        let sessions = list_unread_sessions(dir.path(), 10, None).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0, "tgid_unread");
        assert_eq!(sessions[0].1, 2);
        assert_eq!(sessions[0].3, "Unread Nick");
    }

    #[test]
    fn list_unread_sessions_filters_by_query() {
        let dir = create_decrypted_dir_with_session_table(
            &[
                ("tgid_alice", "Alice Zhang", "", ""),
                ("tgid_bob", "Bob Lee", "", ""),
            ],
            &[("tgid_alice", 4, 1000, 1000), ("tgid_bob", 1, 2000, 2000)],
        );

        let sessions = list_unread_sessions(dir.path(), 10, Some("Alic Zhang")).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0, "tgid_alice");
        assert_eq!(sessions[0].1, 4);
        assert_eq!(sessions[0].3, "Alice Zhang");
    }

    #[test]
    fn member_friends_requires_relationship_evidence_without_messages() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("contact")).unwrap();
        let conn = Connection::open(dir.path().join("contact/contact.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE contact(id INTEGER PRIMARY KEY, username TEXT, nick_name TEXT, remark TEXT, alias TEXT,
                local_type INTEGER, flag INTEGER, delete_flag INTEGER, verify_flag INTEGER);
             CREATE TABLE chat_room(id INTEGER PRIMARY KEY, username TEXT, owner TEXT, ext_buffer BLOB);
             CREATE TABLE chatroom_member(room_id INTEGER, member_id INTEGER);
             CREATE TABLE stranger(username TEXT, nick_name TEXT, remark TEXT, alias TEXT);
             INSERT INTO contact VALUES
                (1, 'friend', 'Alice', '', '', 1, 3, 0, 0),
                (2, 'extra_bits', 'Bob', '', '', 1, 32771, 0, 0),
                (3, 'self', 'Self', '', '', 1, 1, 0, 0),
                (4, 'stranger', 'Stranger', 'Has a remark', '', 3, 4, 0, 0),
                (5, 'deleted', 'Deleted', '', '', 1, 3, 1, 0),
                (6, 'service', 'Service', '', '', 1, 3, 0, 8),
                (7, 'unknown', 'Unknown', '', '', 1, NULL, 0, 0),
                (8, 'not_in_group', 'Absent', '', '', 1, 3, 0, 0),
                (9, 'conflict', 'Conflicting evidence', '', '', 1, 3, 0, 0),
                (10, 'room@chatroom', 'Study Group', '', '', 2, 2, 0, 0);
             INSERT INTO stranger VALUES ('conflict', 'Conflicting evidence', '', '');
             INSERT INTO chat_room(id, username) VALUES (20, 'room@chatroom');
             INSERT INTO chatroom_member VALUES (20,1),(20,2),(20,3),(20,4),(20,5),(20,6),(20,7),(20,9);"
        ).unwrap();
        drop(conn);
        let store = SourceStore::from_plaintext(dir.path()).unwrap();
        let matches =
            super::list_member_friends(&store, "room@chatroom", DisplayNameMode::Anonymous, 1)
                .unwrap();
        assert_eq!(matches, ["friend", "extra_bits"]);
        assert!(
            super::list_member_friends(&store, "friend", DisplayNameMode::Anonymous, 1).is_err()
        );
    }

    #[test]
    fn members_filters_complete_roster_by_last_recorded_speech() {
        let dir = tempdir().unwrap();
        let contact_dir = dir.path().join("contact");
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&contact_dir).unwrap();
        std::fs::create_dir_all(&message_dir).unwrap();

        let contact_conn = Connection::open(contact_dir.join("contact.db")).unwrap();
        contact_conn
            .execute_batch(
                "CREATE TABLE contact(id INTEGER PRIMARY KEY, username TEXT, nick_name TEXT, remark TEXT, alias TEXT);
                 CREATE TABLE chat_room(id INTEGER PRIMARY KEY, username TEXT, owner TEXT, ext_buffer BLOB);
                 CREATE TABLE chatroom_member(room_id INTEGER, member_id INTEGER);
                 INSERT INTO contact VALUES
                    (1, 'tgid_alice', 'Alice', '', ''),
                    (2, 'tgid_bob', 'Bob', '', ''),
                    (3, 'tgid_cara', 'Cara', '', ''),
                    (10, 'room@chatroom', 'Study Group', '', '');
                 INSERT INTO chat_room(id, username, owner) VALUES (20, 'room@chatroom', 'tgid_bob');
                 INSERT INTO chatroom_member VALUES (20, 1), (20, 2), (20, 3);",
            )
            .unwrap();
        drop(contact_conn);

        let message_conn = Connection::open(message_dir.join("message_0.db")).unwrap();
        let table = quote_identifier(&msg_table_name("room@chatroom"));
        let sender = quote_identifier(dictionary::msg_sender_column());
        message_conn
            .execute_batch(
                "CREATE TABLE Name2Id(user_name TEXT PRIMARY KEY);
                 INSERT INTO Name2Id(rowid, user_name) VALUES (1, 'tgid_alice'), (2, 'tgid_bob');",
            )
            .unwrap();
        message_conn
            .execute(
                &format!("CREATE TABLE {table}(create_time INTEGER, {sender} INTEGER)"),
                [],
            )
            .unwrap();
        message_conn
            .execute(
                &format!("INSERT INTO {table}(create_time, {sender}) VALUES (100, 1), (2000, 2)"),
                [],
            )
            .unwrap();
        drop(message_conn);

        let store = SourceStore::from_plaintext(dir.path()).unwrap();
        let members = super::list_members(
            &store,
            ListMembersOptions {
                session_query: "Study Group",
                inactive_since: Some(1000),
                name_mode: DisplayNameMode::PersonalRemark,
                jobs: 1,
            },
        )
        .unwrap();

        assert_eq!(
            members
                .iter()
                .map(|member| member.username.as_str())
                .collect::<Vec<_>>(),
            ["tgid_cara", "tgid_alice"]
        );
        assert_eq!(members[0].last_spoke, None);
        assert_eq!(members[1].last_spoke, Some(100));
        assert_eq!(members[1].message_count, 1);
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
                name_mode: DisplayNameMode::PersonalRemark,
            },
        )
        .unwrap();

        assert_eq!(count, 2);
    }

    #[test]
    fn search_messages_requires_fts() {
        let dir = tempdir().unwrap();

        let error = search_messages(
            dir.path(),
            SearchMessagesOptions {
                query: "needle",
                limit: 20,
                since: None,
                name_mode: DisplayNameMode::PersonalRemark,
            },
        )
        .unwrap_err();

        assert!(error.contains("search index is unavailable"));
    }

    #[test]
    fn search_messages_reports_malformed_fts_schema() {
        let dir = tempdir().unwrap();
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();
        let conn = Connection::open(message_dir.join("message_fts.db")).unwrap();
        conn.execute("CREATE TABLE name2id(username TEXT PRIMARY KEY)", [])
            .unwrap();
        conn.execute("CREATE TABLE message_fts_v4_0_content (c0 TEXT)", [])
            .unwrap();
        drop(conn);

        let error = search_messages(
            dir.path(),
            SearchMessagesOptions {
                query: "needle",
                limit: 20,
                since: None,
                name_mode: DisplayNameMode::PersonalRemark,
            },
        )
        .unwrap_err();

        assert!(error.contains("Prepare search of message_fts_v4_0_content"));
        assert!(!error.contains("search index is unavailable"));
    }

    #[test]
    fn search_messages_uses_internal_match_for_ideographs() {
        let dir = create_decrypted_dir_with_fts(&["开源计划", "开放源码", "普通消息"]);

        let count = search_messages(
            dir.path(),
            SearchMessagesOptions {
                query: "开源",
                limit: 20,
                since: None,
                name_mode: DisplayNameMode::PersonalRemark,
            },
        )
        .unwrap();

        assert_eq!(count, 1);
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
                name_mode: DisplayNameMode::PersonalRemark,
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
                name_mode: DisplayNameMode::PersonalRemark,
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
                name_mode: DisplayNameMode::PersonalRemark,
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
            resolve_username_for_test("田雨坤", &contact_db, Some(&message_db), 1).unwrap();

        assert_eq!(username, "tgid_active");
    }

    #[test]
    fn duplicate_name_resolution_reuses_session_activity() {
        let dir = create_decrypted_dir_with_session_table(
            &[
                ("tgid_old", "田雨坤", "", ""),
                ("tgid_active", "田雨坤", "", ""),
            ],
            &[("tgid_old", 0, 1000, 1000), ("tgid_active", 0, 2000, 2000)],
        );
        let store = SourceStore::from_plaintext(dir.path()).unwrap();
        let contacts = contact::load_contacts(&store.open(CONTACT_DB).unwrap()).unwrap();

        let username = resolve_username_for_messages("田雨坤", &contacts, &store, &[], 1).unwrap();

        assert_eq!(username, "tgid_active");
    }

    #[test]
    fn unique_exact_direct_username_accepts_unambiguous_display_name() {
        let (_contact_dir, contact_db) = create_contact_db(&[
            ("linux_2025@chatroom", "Linux 俱乐部 #2025", "", ""),
            ("linux_2026@chatroom", "Linux 俱乐部 #2026🌅", "", ""),
        ]);
        let contacts = load_contacts(&contact_db);

        let username = unique_exact_direct_username(&contacts, "Linux 俱乐部 #2026🌅");

        assert_eq!(username.as_deref(), Some("linux_2026@chatroom"));
    }

    #[test]
    fn unique_exact_direct_username_rejects_other_direct_candidates() {
        let (_contact_dir, contact_db) = create_contact_db(&[
            ("tgid_empty", "豆", "", ""),
            ("tgid_doubao", "豆宝", "", ""),
            ("tgid_meilidou", "美丽豆", "", ""),
        ]);
        let contacts = load_contacts(&contact_db);

        let username = unique_exact_direct_username(&contacts, "豆");

        assert_eq!(username, None);
    }

    #[test]
    fn unique_exact_direct_username_rejects_duplicate_exact_names() {
        let (_contact_dir, contact_db) = create_contact_db(&[
            ("tgid_old", "田雨坤", "", ""),
            ("tgid_active", "田雨坤", "", ""),
        ]);
        let contacts = load_contacts(&contact_db);

        let username = unique_exact_direct_username(&contacts, "田雨坤");

        assert_eq!(username, None);
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

        let username = resolve_username_for_test("豆", &contact_db, Some(&message_db), 1).unwrap();

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
            resolve_username_for_test("Linux", &contact_db, Some(&message_db), 1).unwrap();

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
            resolve_username_for_test("Linux 2026", &contact_db, Some(&message_db), 1).unwrap();

        assert_eq!(username, "linux_2026@chatroom");
    }

    #[test]
    fn resolve_username_uses_fuzzy_match_after_exact_and_contains_fail() {
        let (_contact_dir, contact_db) = create_contact_db(&[
            ("tgid_alice", "Alice Zhang", "", ""),
            ("tgid_bob", "Bob Lee", "", ""),
        ]);

        let username = resolve_username_for_test("Alic Zhang", &contact_db, None, 1).unwrap();

        assert_eq!(username, "tgid_alice");
    }
}
