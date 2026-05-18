use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection, OpenFlags, MAIN_DB};
use serde_json::{Map, Number, Value as JsonValue};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{contact, db, dictionary, media, message, message_index, output, parallel};

const MESSAGE_TARGETS: &[&str] = &["messages", "message", "all-messages"];
const MAX_QUERY_KEYWORDS: usize = 16;
const MAX_QUERY_KEYWORD_CHARS: usize = 256;
const MAX_QUERY_RESULT_WINDOW: usize = 10_000;
const MAX_TABLE_CELL_CHARS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryOutputFormat {
    Table,
    Json,
}

impl QueryOutputFormat {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "table" | "tsv" => Ok(Self::Table),
            "json" | "jsonl" => Ok(Self::Json),
            _ => Err("expected table or json".to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuerySort {
    Newest,
    Oldest,
}

impl QuerySort {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "newest" | "desc" => Ok(Self::Newest),
            "oldest" | "asc" => Ok(Self::Oldest),
            _ => Err("expected newest or oldest".to_string()),
        }
    }

    fn order_dir(self) -> &'static str {
        match self {
            Self::Newest => "DESC",
            Self::Oldest => "ASC",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryMatchMode {
    All,
    Any,
}

impl QueryMatchMode {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "all" => Ok(Self::All),
            "any" => Ok(Self::Any),
            _ => Err("expected all or any".to_string()),
        }
    }

    fn joiner(self) -> &'static str {
        match self {
            Self::All => " AND ",
            Self::Any => " OR ",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryMediaType {
    Voice,
    Image,
    Sticker,
    File,
    Video,
}

impl QueryMediaType {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "voice" => Ok(Self::Voice),
            "image" | "img" => Ok(Self::Image),
            "sticker" => Ok(Self::Sticker),
            "file" => Ok(Self::File),
            "video" => Ok(Self::Video),
            _ => Err("expected voice, image, sticker, file, or video".to_string()),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Voice => "voice",
            Self::Image => "image",
            Self::Sticker => "sticker",
            Self::File => "file",
            Self::Video => "video",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageField {
    Time,
    Session,
    Sender,
    Type,
    Body,
    RawBody,
    Timestamp,
}

#[derive(Debug, Clone)]
pub(crate) struct QueryFields {
    fields: Vec<MessageField>,
}

impl QueryFields {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        let mut fields = Vec::new();
        for raw in value.split(',') {
            let field = match raw.trim().to_ascii_lowercase().as_str() {
                "" => continue,
                "time" => MessageField::Time,
                "session" => MessageField::Session,
                "sender" => MessageField::Sender,
                "type" | "local_type" => MessageField::Type,
                "body" | "text" => MessageField::Body,
                "raw_body" | "raw" => MessageField::RawBody,
                "timestamp" | "create_time" => MessageField::Timestamp,
                other => {
                    return Err(format!(
                        "unknown field '{}'; expected time, session, sender, type, body, raw_body, timestamp",
                        other
                    ));
                }
            };
            fields.push(field);
        }

        if fields.is_empty() {
            return Err("at least one field is required".to_string());
        }
        Ok(Self { fields })
    }

    fn headers(&self) -> Vec<&'static str> {
        self.fields.iter().map(|field| field.header()).collect()
    }
}

impl MessageField {
    fn header(self) -> &'static str {
        match self {
            Self::Time => "time",
            Self::Session => "session",
            Self::Sender => "sender",
            Self::Type => "type",
            Self::Body => "body",
            Self::RawBody => "raw_body",
            Self::Timestamp => "timestamp",
        }
    }
}

pub(crate) struct QueryOptions<'a> {
    pub decrypted_dir: &'a Path,
    pub session: Option<&'a str>,
    pub contains: &'a [String],
    pub not_contains: &'a [String],
    pub raw_contains: &'a [String],
    pub has: &'a [QueryMediaType],
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub limit: usize,
    pub offset: usize,
    pub sort: QuerySort,
    pub match_mode: QueryMatchMode,
    pub fields: QueryFields,
    pub format: QueryOutputFormat,
    pub max_cell_chars: usize,
    pub name_mode: contact::DisplayNameMode,
    pub jobs: usize,
}

pub(crate) struct SchemaOptions<'a> {
    pub decrypted_dir: &'a Path,
    pub db_target: &'a str,
    pub format: QueryOutputFormat,
    pub max_cell_chars: usize,
}

#[derive(Debug, Clone)]
struct QueryTarget {
    label: String,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct MessageQueryContext {
    targets: Vec<QueryTarget>,
    selected_table: Option<String>,
    table_to_session: HashMap<String, String>,
    table_to_display: HashMap<String, String>,
    sender_display: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct MessageRow {
    session: String,
    sender: String,
    local_type: i64,
    create_time: i64,
    body: String,
    raw_body: String,
}

struct SchemaRow {
    section: &'static str,
    name: &'static str,
    value: String,
    description: &'static str,
}

pub(crate) fn run(options: QueryOptions<'_>) -> Result<usize, String> {
    let stdout = std::io::stdout();
    let mut out = output::Output::new(stdout.lock());
    run_messages_with_output(options, &mut out)
}

pub(crate) fn can_answer_from_existing_index(options: &QueryOptions<'_>) -> bool {
    existing_answerable_index(options).is_some()
}

pub(crate) fn run_schema(options: SchemaOptions<'_>) -> Result<usize, String> {
    let stdout = std::io::stdout();
    let mut out = output::Output::new(stdout.lock());
    run_schema_with_output(options, &mut out)
}

fn run_messages_with_output<W: Write>(
    options: QueryOptions<'_>,
    out: &mut output::Output<W>,
) -> Result<usize, String> {
    validate_message_options(&options)?;
    match try_run_indexed_messages_with_output(&options, out) {
        Ok(Some(displayed)) => return Ok(displayed),
        Ok(None) => {}
        Err(e) => log::warn!("Message index query failed; falling back: {}", e),
    }

    let context = build_message_context(&options)?;
    if context.targets.is_empty() {
        return Err("No message databases found. Try 'tg refresh' first.".to_string());
    }

    let db_jobs = parallel::job_count(options.jobs, 8);
    let per_db_rows = parallel::map_ordered(context.targets.clone(), db_jobs, |target| {
        query_message_target(&target, &context, &options)
    });

    let mut rows = Vec::new();
    for db_rows in per_db_rows {
        let db_rows = db_rows?;
        rows.extend(db_rows);
    }

    sort_message_rows(&mut rows, options.sort);
    let rows: Vec<MessageRow> = rows
        .into_iter()
        .skip(options.offset)
        .take(options.limit)
        .collect();
    let displayed = rows.len();

    write_message_rows(
        out,
        &rows,
        &options.fields,
        options.format,
        options.max_cell_chars,
    )?;
    out.flush()?;
    Ok(displayed)
}

fn try_run_indexed_messages_with_output<W: Write>(
    options: &QueryOptions<'_>,
    out: &mut output::Output<W>,
) -> Result<Option<usize>, String> {
    if options.name_mode == contact::DisplayNameMode::Anonymous {
        return Ok(None);
    }

    let Some(index) = existing_answerable_index(options) else {
        return Ok(None);
    };

    let rows = query_indexed_messages(&index, options)?;
    let displayed = rows.len();
    write_message_rows(
        out,
        &rows,
        &options.fields,
        options.format,
        options.max_cell_chars,
    )?;
    out.flush()?;
    Ok(Some(displayed))
}

fn existing_answerable_index(options: &QueryOptions<'_>) -> Option<message_index::HotIndex> {
    if options.name_mode == contact::DisplayNameMode::Anonymous {
        return None;
    }

    let since = options.since?;
    let index = match message_index::open_existing_recent(options.decrypted_dir) {
        Ok(Some(index)) if index.covers(since) => index,
        Ok(_) => return None,
        Err(e) => {
            log::warn!("Message index read failed; falling back: {}", e);
            return None;
        }
    };

    if (!options.contains.is_empty() || !options.not_contains.is_empty())
        && !index_has_decoded_body(&index).unwrap_or(false)
    {
        return None;
    }

    Some(index)
}

fn run_schema_with_output<W: Write>(
    options: SchemaOptions<'_>,
    out: &mut output::Output<W>,
) -> Result<usize, String> {
    let targets = resolve_targets(options.decrypted_dir, options.db_target)?;
    if targets.is_empty() {
        return Err(format!(
            "No databases matched --db '{}'. Try 'tg refresh' first.",
            options.db_target
        ));
    }

    let rows = public_schema_rows(options.db_target, targets.len());
    write_schema_rows(out, &rows, options.format, options.max_cell_chars)?;
    out.flush()?;
    Ok(rows.len())
}

fn validate_message_options(options: &QueryOptions<'_>) -> Result<(), String> {
    if options.limit == 0 {
        return Err("--limit must be greater than 0".to_string());
    }
    if options.max_cell_chars > MAX_TABLE_CELL_CHARS {
        return Err(format!(
            "--max-cell-chars must be <= {}",
            MAX_TABLE_CELL_CHARS
        ));
    }
    validate_query_terms("--contains", options.contains)?;
    validate_query_terms("--not", options.not_contains)?;
    validate_query_terms("--raw-contains", options.raw_contains)?;
    message_query_window(options)?;
    if options.contains.is_empty()
        && options.raw_contains.is_empty()
        && options.has.is_empty()
        && options.since.is_none()
    {
        return Err(
            "refusing an unbounded message query; pass --contains, --raw-contains, --has, or --since"
                .to_string(),
        );
    }
    Ok(())
}

fn validate_query_terms(flag: &str, terms: &[String]) -> Result<(), String> {
    if terms.len() > MAX_QUERY_KEYWORDS {
        return Err(format!(
            "{} accepts at most {} values",
            flag, MAX_QUERY_KEYWORDS
        ));
    }

    for term in terms {
        if term.trim().is_empty() {
            return Err(format!("{} cannot be empty", flag));
        }
        if term.chars().count() > MAX_QUERY_KEYWORD_CHARS {
            return Err(format!(
                "{} values must be <= {} characters",
                flag, MAX_QUERY_KEYWORD_CHARS
            ));
        }
    }
    Ok(())
}

fn message_query_window(options: &QueryOptions<'_>) -> Result<usize, String> {
    let window = options
        .limit
        .checked_add(options.offset)
        .ok_or_else(|| "--limit plus --offset is too large".to_string())?;
    if window > MAX_QUERY_RESULT_WINDOW {
        return Err(format!(
            "--limit plus --offset must be <= {}",
            MAX_QUERY_RESULT_WINDOW
        ));
    }
    Ok(window)
}

fn query_needs_post_filtering(options: &QueryOptions<'_>) -> bool {
    !options.contains.is_empty()
        || !options.not_contains.is_empty()
        || options.has.contains(&QueryMediaType::File)
}

fn indexed_query_needs_post_filtering(
    options: &QueryOptions<'_>,
    has_indexed_media_type: bool,
) -> bool {
    !has_indexed_media_type && options.has.contains(&QueryMediaType::File)
}

fn index_has_decoded_body(index: &message_index::HotIndex) -> Result<bool, String> {
    let conn = Connection::open(&index.path)
        .map_err(|e| format!("Cannot open message index {}: {}", index.path.display(), e))?;
    Ok(db::table_has_column(&conn, "messages", "decoded_body"))
}

fn build_message_context(options: &QueryOptions<'_>) -> Result<MessageQueryContext, String> {
    let (contact_db, message_dbs) = db::find_decrypted_dbs(options.decrypted_dir);
    let contacts = contact_db
        .as_ref()
        .and_then(|path| contact::load_contacts(path).ok())
        .unwrap_or_default();

    let mut table_to_session = HashMap::new();
    let mut table_to_display = HashMap::new();
    let mut sender_display = HashMap::new();
    for (username, contact) in &contacts {
        let display = contact.display_name(options.name_mode).to_string();
        table_to_session.insert(db::msg_table_name(username), username.clone());
        table_to_display.insert(db::msg_table_name(username), display.clone());
        sender_display.insert(username.clone(), display);
    }

    let selected_table = match options.session {
        Some(session) => {
            let username = if contacts.is_empty() {
                db::resolve_username_for_messages(
                    session,
                    contact_db.as_deref(),
                    &message_dbs,
                    options.jobs,
                )?
            } else {
                db::resolve_username_for_messages_with_contacts(
                    session,
                    &contacts,
                    &message_dbs,
                    options.jobs,
                )
            };
            let table = db::msg_table_name(&username);
            table_to_session
                .entry(table.clone())
                .or_insert_with(|| username.clone());
            table_to_display
                .entry(table.clone())
                .or_insert_with(|| username.clone());
            Some(table)
        }
        None => None,
    };

    let targets = message_dbs
        .into_iter()
        .map(|path| QueryTarget {
            label: relative_label(options.decrypted_dir, &path),
            path,
        })
        .collect();

    Ok(MessageQueryContext {
        targets,
        selected_table,
        table_to_session,
        table_to_display,
        sender_display,
    })
}

fn query_message_target(
    target: &QueryTarget,
    context: &MessageQueryContext,
    options: &QueryOptions<'_>,
) -> Result<Vec<MessageRow>, String> {
    let conn = open_readonly(&target.path)?;
    let tables = match &context.selected_table {
        Some(table) => vec![table.clone()],
        None => list_message_tables(&conn)?,
    };

    let name2id = load_name2id(&conn);
    let mut rows = Vec::new();
    for table in tables {
        if !table_exists(&conn, &table) {
            continue;
        }
        let mut table_rows =
            query_message_table(&conn, target, &table, &name2id, context, options)?;
        rows.append(&mut table_rows);
    }
    Ok(rows)
}

fn query_message_table(
    conn: &Connection,
    target: &QueryTarget,
    table: &str,
    name2id: &HashMap<i64, String>,
    context: &MessageQueryContext,
    options: &QueryOptions<'_>,
) -> Result<Vec<MessageRow>, String> {
    let body_col = db::quote_identifier(dictionary::msg_body_column());
    let marker_col = db::quote_identifier(dictionary::msg_compression_marker_column());
    let sender_col = db::quote_identifier(dictionary::msg_sender_column());
    let packed_col = if db::table_has_column(conn, table, dictionary::msg_packed_meta_column()) {
        db::quote_identifier(dictionary::msg_packed_meta_column())
    } else {
        "x''".to_string()
    };
    let local_id_col = if db::table_has_column(conn, table, "local_id") {
        db::quote_identifier("local_id")
    } else {
        "rowid".to_string()
    };
    let quoted_table = db::quote_identifier(table);
    let result_window = message_query_window(options)?;

    let mut clauses = vec!["create_time > 0".to_string()];
    let mut params = Vec::new();
    if let Some(since) = options.since {
        clauses.push("create_time >= ?".to_string());
        params.push(Value::Integer(since));
    }
    if let Some(until) = options.until {
        clauses.push("create_time <= ?".to_string());
        params.push(Value::Integer(until));
    }

    if !options.raw_contains.is_empty() {
        let contains_clause = options
            .raw_contains
            .iter()
            .map(|query| {
                params.push(Value::Text(like_contains_pattern(query)));
                format!("{body_col} LIKE ? ESCAPE '\\'")
            })
            .collect::<Vec<_>>()
            .join(options.match_mode.joiner());
        clauses.push(format!("({})", contains_clause));
    }
    if !options.has.is_empty() {
        let media_clause = options
            .has
            .iter()
            .map(|media_type| legacy_indexed_media_clause(*media_type, &body_col))
            .collect::<Vec<_>>()
            .join(" OR ");
        clauses.push(format!("({})", media_clause));
    }

    let limit_clause = if query_needs_post_filtering(options) {
        String::new()
    } else {
        format!(" LIMIT {}", result_window)
    };

    let sql = format!(
        "SELECT local_type, create_time, {body_col}, {marker_col}, {sender_col}, {packed_col}, {local_id_col} \
         FROM {quoted_table} \
         WHERE {where_clause} \
         ORDER BY create_time {order_dir}{limit_clause}",
        body_col = body_col,
        marker_col = marker_col,
        sender_col = sender_col,
        packed_col = packed_col,
        local_id_col = local_id_col,
        quoted_table = quoted_table,
        where_clause = clauses.join(" AND "),
        order_dir = options.sort.order_dir(),
        limit_clause = limit_clause,
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("Message query prepare error in {}: {}", target.label, e))?;
    let mapped = stmt
        .query_map(params_from_iter(params.iter()), |row| {
            let marker: Option<i64> = row.get::<_, Option<i64>>(3)?;
            let raw_body = read_message_body(row, 2, marker);
            let sender_id: i64 = row.get::<_, Option<i64>>(4)?.unwrap_or(0);
            let sender_account = name2id.get(&sender_id).cloned().unwrap_or_default();
            let packed_info: Vec<u8> = row.get::<_, Option<Vec<u8>>>(5)?.unwrap_or_default();
            let local_id = row.get::<_, Option<i64>>(6)?.unwrap_or(0);
            let session = context
                .table_to_display
                .get(table)
                .cloned()
                .or_else(|| context.table_to_session.get(table).cloned())
                .unwrap_or_else(|| table.to_string());
            let local_type = row.get::<_, Option<i64>>(0)?.unwrap_or(-1);
            let body = decode_query_body(
                local_type,
                &raw_body,
                marker,
                &packed_info,
                Some(local_id),
                &session,
                &context.sender_display,
            );
            Ok(MessageRow {
                session,
                sender: display_sender(&sender_account, context),
                local_type,
                create_time: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                body,
                raw_body,
            })
        })
        .map_err(|e| format!("Message query error in {}: {}", target.label, e))?;

    Ok(mapped
        .filter_map(|row| row.ok())
        .filter(|row| message_row_matches_filters(row, options))
        .collect())
}

fn query_indexed_messages(
    index: &message_index::HotIndex,
    options: &QueryOptions<'_>,
) -> Result<Vec<MessageRow>, String> {
    let (contact_db, message_dbs) = db::find_decrypted_dbs(options.decrypted_dir);
    let contacts = contact_db
        .as_ref()
        .and_then(|path| contact::load_contacts(path).ok())
        .unwrap_or_default();
    let selected_session = match options.session {
        Some(session) if contacts.is_empty() => db::resolve_username_for_messages(
            session,
            contact_db.as_deref(),
            &message_dbs,
            options.jobs,
        )
        .map(Some)?,
        Some(session) => Some(db::resolve_username_for_messages_with_contacts(
            session,
            &contacts,
            &message_dbs,
            options.jobs,
        )),
        None => None,
    };
    let result_window = message_query_window(options)?;
    let conn = Connection::open(&index.path)
        .map_err(|e| format!("Cannot open message index {}: {}", index.path.display(), e))?;
    let raw_body_col = if db::table_has_column(&conn, "messages", "raw_body") {
        "raw_body"
    } else {
        "body"
    };
    let has_decoded_body = db::table_has_column(&conn, "messages", "decoded_body");
    let decoded_body_col = if has_decoded_body {
        "decoded_body"
    } else {
        "body"
    };
    let has_media_type = db::table_has_column(&conn, "messages", "media_type");
    let local_id_col = if db::table_has_column(&conn, "messages", "local_id") {
        "local_id"
    } else {
        "NULL"
    };
    let mut clauses = vec!["create_time > 0".to_string()];
    let mut params = Vec::new();

    if let Some(session) = selected_session {
        clauses.push("session_id = ?".to_string());
        params.push(Value::Text(session));
    }
    if let Some(since) = options.since {
        clauses.push("create_time >= ?".to_string());
        params.push(Value::Integer(since));
    }
    if let Some(until) = options.until {
        clauses.push("create_time <= ?".to_string());
        params.push(Value::Integer(until));
    }

    if !options.contains.is_empty() {
        let contains_clause = options
            .contains
            .iter()
            .map(|query| {
                params.push(Value::Text(like_contains_pattern(query)));
                format!("{decoded_body_col} LIKE ? ESCAPE '\\'")
            })
            .collect::<Vec<_>>()
            .join(options.match_mode.joiner());
        clauses.push(format!("({})", contains_clause));
    }

    for query in options.not_contains {
        params.push(Value::Text(like_contains_pattern(query)));
        clauses.push(format!("{decoded_body_col} NOT LIKE ? ESCAPE '\\'"));
    }
    if !options.raw_contains.is_empty() {
        let contains_clause = options
            .raw_contains
            .iter()
            .map(|query| {
                params.push(Value::Text(like_contains_pattern(query)));
                format!("{raw_body_col} LIKE ? ESCAPE '\\'")
            })
            .collect::<Vec<_>>()
            .join(options.match_mode.joiner());
        clauses.push(format!("({})", contains_clause));
    }
    if !options.has.is_empty() {
        if has_media_type {
            let placeholders = options
                .has
                .iter()
                .map(|media_type| {
                    params.push(Value::Text(media_type.as_str().to_string()));
                    "?"
                })
                .collect::<Vec<_>>()
                .join(", ");
            clauses.push(format!("media_type IN ({})", placeholders));
        } else {
            let media_clause = options
                .has
                .iter()
                .map(|media_type| legacy_indexed_media_clause(*media_type, raw_body_col))
                .collect::<Vec<_>>()
                .join(" OR ");
            clauses.push(format!("({})", media_clause));
        }
    }

    let limit_clause = if indexed_query_needs_post_filtering(options, has_media_type) {
        String::new()
    } else {
        format!(" LIMIT {}", result_window)
    };
    let sql = format!(
        "SELECT session_display, sender_display, local_type, create_time, {raw_body_col}, {decoded_body_col}, marker, packed_info, {local_id_col}
         FROM messages
         WHERE {where_clause}
         ORDER BY create_time {order_dir}{limit_clause}",
        raw_body_col = raw_body_col,
        decoded_body_col = decoded_body_col,
        local_id_col = local_id_col,
        where_clause = clauses.join(" AND "),
        order_dir = options.sort.order_dir(),
        limit_clause = limit_clause,
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("Prepare indexed query: {}", e))?;
    let mapped = stmt
        .query_map(params_from_iter(params.iter()), |row| {
            let session = row.get::<_, Option<String>>(0)?.unwrap_or_default();
            let local_type = row.get::<_, Option<i64>>(2)?.unwrap_or(-1);
            let raw_body = row.get::<_, Option<String>>(4)?.unwrap_or_default();
            let body = if has_decoded_body {
                row.get::<_, Option<String>>(5)?.unwrap_or_default()
            } else {
                let marker = row.get::<_, Option<i64>>(6)?;
                let packed_info: Vec<u8> = row.get::<_, Option<Vec<u8>>>(7)?.unwrap_or_default();
                let local_id = row.get::<_, Option<i64>>(8)?.unwrap_or(0);
                decode_query_body(
                    local_type,
                    &raw_body,
                    marker,
                    &packed_info,
                    Some(local_id),
                    &session,
                    &HashMap::new(),
                )
            };
            Ok(MessageRow {
                session,
                sender: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                local_type,
                create_time: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                body,
                raw_body,
            })
        })
        .map_err(|e| format!("Run indexed query: {}", e))?;

    Ok(mapped
        .filter_map(|row| row.ok())
        .filter(|row| {
            if has_media_type {
                true
            } else if has_decoded_body {
                message_row_matches_media_filters(row, options)
            } else {
                message_row_matches_filters(row, options)
            }
        })
        .skip(options.offset)
        .take(options.limit)
        .collect())
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

fn display_sender(sender_account: &str, context: &MessageQueryContext) -> String {
    if sender_account.is_empty() {
        return String::new();
    }
    context
        .sender_display
        .get(sender_account)
        .cloned()
        .unwrap_or_else(|| sender_account.to_string())
}

fn decode_query_body(
    local_type: i64,
    raw_body: &str,
    marker: Option<i64>,
    packed_info: &[u8],
    voice_id: Option<i64>,
    session_display: &str,
    sender_display: &HashMap<String, String>,
) -> String {
    let local_type = i32::try_from(media::local_type_low32(local_type)).unwrap_or(-1);
    message::decode_message_with_context(
        local_type,
        raw_body,
        session_display,
        marker,
        packed_info,
        message::DecodeContext {
            time_bucket: crate::time::MessageTimeBucket::Minute(1),
            voice_id: (local_type == 34).then_some(voice_id).flatten(),
        },
        |id| {
            sender_display
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.to_string())
        },
    )
    .content
}

fn message_row_matches_filters(row: &MessageRow, options: &QueryOptions<'_>) -> bool {
    if !message_row_matches_media_filters(row, options) {
        return false;
    }

    if !options.contains.is_empty() {
        let matched = match options.match_mode {
            QueryMatchMode::All => options.contains.iter().all(|term| row.body.contains(term)),
            QueryMatchMode::Any => options.contains.iter().any(|term| row.body.contains(term)),
        };
        if !matched {
            return false;
        }
    }

    !options
        .not_contains
        .iter()
        .any(|term| row.body.contains(term))
}

fn message_row_matches_media_filters(row: &MessageRow, options: &QueryOptions<'_>) -> bool {
    if !options.has.is_empty()
        && !options
            .has
            .iter()
            .any(|media_type| message_has_media(row.local_type, &row.raw_body, *media_type))
    {
        return false;
    }

    true
}

fn legacy_indexed_media_clause(media_type: QueryMediaType, raw_body_col: &str) -> String {
    match media_type {
        QueryMediaType::Voice => "((local_type & 4294967295) = 34)".to_string(),
        QueryMediaType::Image => "((local_type & 4294967295) = 3)".to_string(),
        QueryMediaType::Sticker => "((local_type & 4294967295) = 47)".to_string(),
        QueryMediaType::Video => "((local_type & 4294967295) = 43)".to_string(),
        QueryMediaType::File => format!(
            "((local_type & 4294967295) = 62 OR (local_type & 4294967295) = 49 OR {raw_body_col} LIKE '%<type>6</type>%' OR {raw_body_col} LIKE '%<type>62</type>%')"
        ),
    }
}

fn message_has_media(local_type: i64, raw_body: &str, media_type: QueryMediaType) -> bool {
    media::message_media_type(local_type, raw_body) == Some(media_type.as_str())
}

fn list_message_tables(conn: &Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'")
        .map_err(|e| format!("Cannot list message tables: {}", e))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("Cannot read message tables: {}", e))?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.prepare(&format!(
        "SELECT 1 FROM {} LIMIT 1",
        db::quote_identifier(table)
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

fn sort_message_rows(rows: &mut [MessageRow], sort: QuerySort) {
    match sort {
        QuerySort::Newest => rows.sort_by_key(|row| Reverse(row.create_time)),
        QuerySort::Oldest => rows.sort_by_key(|row| row.create_time),
    }
}

fn write_message_rows<W: Write>(
    out: &mut output::Output<W>,
    rows: &[MessageRow],
    fields: &QueryFields,
    format: QueryOutputFormat,
    max_cell_chars: usize,
) -> Result<(), String> {
    match format {
        QueryOutputFormat::Table => write_message_table(out, rows, fields, max_cell_chars),
        QueryOutputFormat::Json => write_message_json(out, rows, fields),
    }
}

fn write_message_table<W: Write>(
    out: &mut output::Output<W>,
    rows: &[MessageRow],
    fields: &QueryFields,
    max_cell_chars: usize,
) -> Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }

    out.line(format_args!("{}", fields.headers().join("\t")))?;
    for row in rows {
        let cells: Vec<String> = fields
            .fields
            .iter()
            .map(|field| escape_table_cell(&message_field_text(row, *field), max_cell_chars))
            .collect();
        out.line(format_args!("{}", cells.join("\t")))?;
    }
    Ok(())
}

fn write_message_json<W: Write>(
    out: &mut output::Output<W>,
    rows: &[MessageRow],
    fields: &QueryFields,
) -> Result<(), String> {
    for row in rows {
        let mut object = Map::new();
        for field in &fields.fields {
            object.insert(field.header().to_string(), message_field_json(row, *field));
        }
        out.line(format_args!("{}", JsonValue::Object(object)))?;
    }
    Ok(())
}

fn message_field_text(row: &MessageRow, field: MessageField) -> String {
    match field {
        MessageField::Time => crate::time::format_local_timestamp(row.create_time),
        MessageField::Session => row.session.clone(),
        MessageField::Sender => row.sender.clone(),
        MessageField::Type => row.local_type.to_string(),
        MessageField::Body => row.body.clone(),
        MessageField::RawBody => row.raw_body.clone(),
        MessageField::Timestamp => row.create_time.to_string(),
    }
}

fn public_schema_rows(db_target: &str, target_count: usize) -> Vec<SchemaRow> {
    let mut rows = vec![
        SchemaRow {
            section: "target",
            name: "selected",
            value: db_target.to_string(),
            description: "cache target checked for availability",
        },
        SchemaRow {
            section: "target",
            name: "databases",
            value: target_count.to_string(),
            description: "matching decrypted cache files",
        },
        SchemaRow {
            section: "target",
            name: "mode",
            value: "read-only".to_string(),
            description: "tg opens cache files without write access",
        },
        SchemaRow {
            section: "command",
            name: "query",
            value: "tg query".to_string(),
            description: "structured message lookup; raw database SQL is not accepted",
        },
    ];

    rows.extend([
        SchemaRow {
            section: "field",
            name: "time",
            value: "string".to_string(),
            description: "local display time",
        },
        SchemaRow {
            section: "field",
            name: "timestamp",
            value: "integer".to_string(),
            description: "message create time from the local cache",
        },
        SchemaRow {
            section: "field",
            name: "session",
            value: "string".to_string(),
            description: "resolved session display name when known",
        },
        SchemaRow {
            section: "field",
            name: "sender",
            value: "string".to_string(),
            description: "resolved sender display name when known",
        },
        SchemaRow {
            section: "field",
            name: "type",
            value: "integer".to_string(),
            description: "message type code from the local cache",
        },
        SchemaRow {
            section: "field",
            name: "body",
            value: "string".to_string(),
            description: "message text or decoded text payload",
        },
        SchemaRow {
            section: "field",
            name: "raw_body",
            value: "string".to_string(),
            description: "raw message body from the local cache",
        },
        SchemaRow {
            section: "filter",
            name: "session",
            value: "--session".to_string(),
            description: "limit query to one resolved session",
        },
        SchemaRow {
            section: "filter",
            name: "contains",
            value: "--contains".to_string(),
            description: "required decoded/display text match; repeatable",
        },
        SchemaRow {
            section: "filter",
            name: "raw_contains",
            value: "--raw-contains".to_string(),
            description: "required raw database body match; repeatable",
        },
        SchemaRow {
            section: "filter",
            name: "has",
            value: "voice,image,sticker,file,video"
                .split(',')
                .filter_map(|value| QueryMediaType::parse(value).ok())
                .map(QueryMediaType::as_str)
                .collect::<Vec<_>>()
                .join(","),
            description: "require a decoded media category",
        },
        SchemaRow {
            section: "filter",
            name: "not",
            value: "--not".to_string(),
            description: "excluded text match; repeatable",
        },
        SchemaRow {
            section: "filter",
            name: "since",
            value: "--since".to_string(),
            description: "lower time bound",
        },
        SchemaRow {
            section: "filter",
            name: "until",
            value: "--until".to_string(),
            description: "upper time bound",
        },
        SchemaRow {
            section: "option",
            name: "format",
            value: "table,json".to_string(),
            description: "supported output formats",
        },
        SchemaRow {
            section: "option",
            name: "order",
            value: "newest,oldest".to_string(),
            description: "global result ordering",
        },
    ]);

    rows
}

fn write_schema_rows<W: Write>(
    out: &mut output::Output<W>,
    rows: &[SchemaRow],
    format: QueryOutputFormat,
    max_cell_chars: usize,
) -> Result<(), String> {
    match format {
        QueryOutputFormat::Table => {
            out.line(format_args!("section\tname\tvalue\tdescription"))?;
            for row in rows {
                out.line(format_args!(
                    "{}\t{}\t{}\t{}",
                    escape_table_cell(row.section, max_cell_chars),
                    escape_table_cell(row.name, max_cell_chars),
                    escape_table_cell(&row.value, max_cell_chars),
                    escape_table_cell(row.description, max_cell_chars)
                ))?;
            }
        }
        QueryOutputFormat::Json => {
            for row in rows {
                let mut object = Map::new();
                object.insert(
                    "section".to_string(),
                    JsonValue::String(row.section.to_string()),
                );
                object.insert("name".to_string(), JsonValue::String(row.name.to_string()));
                object.insert("value".to_string(), JsonValue::String(row.value.clone()));
                object.insert(
                    "description".to_string(),
                    JsonValue::String(row.description.to_string()),
                );
                out.line(format_args!("{}", JsonValue::Object(object)))?;
            }
        }
    }
    Ok(())
}

fn message_field_json(row: &MessageRow, field: MessageField) -> JsonValue {
    match field {
        MessageField::Type => JsonValue::Number(Number::from(row.local_type)),
        MessageField::Timestamp => JsonValue::Number(Number::from(row.create_time)),
        _ => JsonValue::String(message_field_text(row, field)),
    }
}

fn resolve_targets(decrypted_dir: &Path, target: &str) -> Result<Vec<QueryTarget>, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("--db cannot be empty".to_string());
    }

    let (contact_db, message_dbs) = db::find_decrypted_dbs(decrypted_dir);
    let lower = target.to_ascii_lowercase();
    if lower == "contact" {
        return contact_db
            .map(|path| {
                vec![QueryTarget {
                    label: "contact/contact.db".to_string(),
                    path,
                }]
            })
            .ok_or_else(|| "contact/contact.db was not found".to_string());
    }

    if MESSAGE_TARGETS.contains(&lower.as_str()) {
        return Ok(message_dbs
            .into_iter()
            .map(|path| QueryTarget {
                label: relative_label(decrypted_dir, &path),
                path,
            })
            .collect());
    }

    if matches!(lower.as_str(), "fts" | "message_fts" | "message_fts.db") {
        let path = decrypted_dir.join("message/message_fts.db");
        return path
            .exists()
            .then(|| {
                vec![QueryTarget {
                    label: "message/message_fts.db".to_string(),
                    path,
                }]
            })
            .ok_or_else(|| "message/message_fts.db was not found".to_string());
    }

    if let Some(path) = resolve_numbered_message_db(decrypted_dir, target) {
        return Ok(vec![QueryTarget {
            label: relative_label(decrypted_dir, &path),
            path,
        }]);
    }

    Err(format!(
        "unsupported --db '{}'; use messages, contact, fts, or message_N",
        target
    ))
}

fn resolve_numbered_message_db(decrypted_dir: &Path, target: &str) -> Option<PathBuf> {
    let target = target.trim().trim_start_matches("./");
    let file_name = target
        .strip_prefix("message/")
        .or_else(|| target.strip_prefix("message\\"))
        .unwrap_or(target);
    let file_name = if file_name.ends_with(".db") {
        file_name.to_string()
    } else {
        format!("{}.db", file_name)
    };

    if !db::is_message_db_name(&file_name) {
        return None;
    }

    let path = decrypted_dir.join("message").join(file_name);
    path.exists().then_some(path)
}

fn open_readonly(path: &Path) -> Result<Connection, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)
        .map_err(|e| format!("Cannot open {} read-only: {}", path.display(), e))?;
    if !conn
        .is_readonly(MAIN_DB)
        .map_err(|e| format!("Cannot verify read-only mode: {}", e))?
    {
        return Err(format!("{} did not open read-only", path.display()));
    }
    Ok(conn)
}

fn relative_label(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
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

fn escape_table_cell(value: &str, max_chars: usize) -> String {
    let mut escaped = String::new();
    let mut chars = value.chars();
    for _ in 0..max_chars {
        let Some(ch) = chars.next() else {
            return escaped;
        };
        match ch {
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            ch if ch.is_control() => escaped.push_str(&format!("\\u{{{:04x}}}", ch as u32)),
            _ => escaped.push(ch),
        }
    }
    if chars.next().is_some() {
        escaped.push_str("...");
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::{tempdir, TempDir};

    fn create_query_test_dir() -> TempDir {
        let dir = tempdir().unwrap();
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();
        let conn = Connection::open(message_dir.join("message_0.db")).unwrap();
        let marker_col = dictionary::msg_compression_marker_column();
        let packed_col = dictionary::msg_packed_meta_column();
        conn.execute(
            &format!(
                "CREATE TABLE Msg_test (
                local_type INTEGER,
                create_time INTEGER,
                message_content TEXT,
                {marker_col} INTEGER,
                real_sender_id INTEGER,
                {packed_col} BLOB
            )"
            ),
            [],
        )
        .unwrap();
        conn.execute("CREATE TABLE Name2Id (user_name TEXT)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO Name2Id (rowid, user_name) VALUES (7, 'tgid_sender')",
            [],
        )
        .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_type, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
             VALUES (1, 1000, 'before needle after', NULL, 7, x'')"
            ),
            [],
        )
        .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_type, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
             VALUES (1, 1001, 'ordinary update', NULL, 0, x'')"
            ),
            [],
        )
        .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_type, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
             VALUES (1, 1002, 'literal 100%_match', NULL, 0, x'')"
            ),
            [],
        )
        .unwrap();
        drop(conn);
        dir
    }

    fn default_query_options<'a>(
        dir: &'a Path,
        contains: &'a [String],
        not_contains: &'a [String],
    ) -> QueryOptions<'a> {
        QueryOptions {
            decrypted_dir: dir,
            session: None,
            contains,
            not_contains,
            raw_contains: &[],
            has: &[],
            since: None,
            until: None,
            limit: 100,
            offset: 0,
            sort: QuerySort::Newest,
            match_mode: QueryMatchMode::All,
            fields: QueryFields::parse("timestamp,body").unwrap(),
            format: QueryOutputFormat::Table,
            max_cell_chars: 500,
            name_mode: contact::DisplayNameMode::PersonalRemark,
            jobs: 1,
        }
    }

    fn run_messages_for_test(
        dir: &Path,
        contains: &[String],
        not_contains: &[String],
    ) -> Result<(usize, String), String> {
        let mut bytes = Vec::new();
        let count = {
            let mut out = output::Output::new(&mut bytes);
            let count = run_messages_with_output(
                default_query_options(dir, contains, not_contains),
                &mut out,
            )?;
            out.flush()?;
            count
        };
        Ok((count, String::from_utf8(bytes).unwrap()))
    }

    #[test]
    fn message_query_matches_contains() {
        let dir = create_query_test_dir();
        let contains = vec!["needle".to_string()];

        let (count, output) = run_messages_for_test(dir.path(), &contains, &[]).unwrap();

        assert_eq!(count, 1);
        assert!(output.contains("before needle after"));
    }

    #[test]
    fn message_query_decodes_app_and_image_xml_body() {
        let dir = create_query_test_dir();
        let conn = Connection::open(dir.path().join("message/message_0.db")).unwrap();
        let marker_col = dictionary::msg_compression_marker_column();
        let packed_col = dictionary::msg_packed_meta_column();
        let quote_xml = r#"<msg><appmsg><title>回复内容</title><type>57</type><refermsg><type>1</type><displayname>Bob</displayname><content>引用内容</content></refermsg></appmsg></msg>"#;
        let image_xml =
            r#"<msg><img aeskey="image-key" cdnthumbwidth="180" cdnthumbheight="153" /></msg>"#;
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_type, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
                 VALUES (49, 1003, ?1, NULL, 7, x'')"
            ),
            params![quote_xml],
        )
        .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_type, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
                 VALUES (3, 1004, ?1, NULL, 7, x'')"
            ),
            params![image_xml],
        )
        .unwrap();
        drop(conn);

        let contains = vec!["内容".to_string()];
        let (count, output) = run_messages_for_test(dir.path(), &contains, &[]).unwrap();

        assert_eq!(count, 1);
        assert!(output.contains("> Bob: 引用内容\\n 回复内容"));
        assert!(!output.contains("<appmsg"));

        let contains = vec!["image-key".to_string()];
        let (count, output) = run_messages_for_test(dir.path(), &contains, &[]).unwrap();

        assert_eq!(count, 1);
        assert!(output.contains("[img:image-key]"));
        assert!(!output.contains("<img"));
    }

    #[test]
    fn message_query_can_return_raw_body() {
        let dir = create_query_test_dir();
        let conn = Connection::open(dir.path().join("message/message_0.db")).unwrap();
        let marker_col = dictionary::msg_compression_marker_column();
        let packed_col = dictionary::msg_packed_meta_column();
        let image_xml = r#"<msg><img aeskey="raw-key" /></msg>"#;
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_type, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
                 VALUES (3, 1003, ?1, NULL, 7, x'')"
            ),
            params![image_xml],
        )
        .unwrap();
        drop(conn);

        let contains = vec!["raw-key".to_string()];
        let mut bytes = Vec::new();
        let mut out = output::Output::new(&mut bytes);
        let mut options = default_query_options(dir.path(), &contains, &[]);
        options.fields = QueryFields::parse("timestamp,body,raw_body").unwrap();
        let count = run_messages_with_output(options, &mut out).unwrap();
        out.flush().unwrap();
        drop(out);
        let output = String::from_utf8(bytes).unwrap();

        assert_eq!(count, 1);
        assert!(output.contains("[img:raw-key]"));
        assert!(output.contains("<msg><img aeskey=\"raw-key\" /></msg>"));
    }

    #[test]
    fn message_query_separates_raw_and_decoded_contains() {
        let dir = create_query_test_dir();
        let conn = Connection::open(dir.path().join("message/message_0.db")).unwrap();
        let marker_col = dictionary::msg_compression_marker_column();
        let packed_col = dictionary::msg_packed_meta_column();
        let xml = r#"<msg><appmsg><title>visible title</title><type>5</type><secret>raw-only-token</secret></appmsg></msg>"#;
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_type, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
                 VALUES (49, 1003, ?1, NULL, 7, x'')"
            ),
            params![xml],
        )
        .unwrap();
        drop(conn);

        let contains = vec!["raw-only-token".to_string()];
        let (count, output) = run_messages_for_test(dir.path(), &contains, &[]).unwrap();
        assert_eq!(count, 0);
        assert!(output.is_empty());

        let raw_contains = vec!["raw-only-token".to_string()];
        let mut bytes = Vec::new();
        let mut out = output::Output::new(&mut bytes);
        let mut options = default_query_options(dir.path(), &[], &[]);
        options.raw_contains = &raw_contains;
        let count = run_messages_with_output(options, &mut out).unwrap();
        out.flush().unwrap();
        drop(out);
        let output = String::from_utf8(bytes).unwrap();

        assert_eq!(count, 1);
        assert!(output.contains("visible title"));
        assert!(!output.contains("raw-only-token"));
    }

    #[test]
    fn message_query_filters_by_media_type() {
        let dir = create_query_test_dir();
        let conn = Connection::open(dir.path().join("message/message_0.db")).unwrap();
        let marker_col = dictionary::msg_compression_marker_column();
        let packed_col = dictionary::msg_packed_meta_column();
        let file_type = (6_i64 << 32) | 49;
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_type, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
                 VALUES (34, 1003, '', NULL, 7, x'')"
            ),
            [],
        )
        .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_type, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
                 VALUES (?1, 1004, '<msg><appmsg><title>report.pdf</title><type>6</type></appmsg></msg>', NULL, 7, x'')"
            ),
            params![file_type],
        )
        .unwrap();
        drop(conn);

        let has = vec![QueryMediaType::File];
        let mut bytes = Vec::new();
        let mut out = output::Output::new(&mut bytes);
        let mut options = default_query_options(dir.path(), &[], &[]);
        options.has = &has;
        let count = run_messages_with_output(options, &mut out).unwrap();
        out.flush().unwrap();
        drop(out);
        let output = String::from_utf8(bytes).unwrap();

        assert_eq!(count, 1);
        assert!(output.contains("report.pdf"));
        assert!(!output.contains("[语音"));
    }

    #[test]
    fn message_query_displays_voice_local_id() {
        let dir = create_query_test_dir();
        let conn = Connection::open(dir.path().join("message/message_0.db")).unwrap();
        let marker_col = dictionary::msg_compression_marker_column();
        let packed_col = dictionary::msg_packed_meta_column();
        conn.execute("ALTER TABLE Msg_test ADD COLUMN local_id INTEGER", [])
            .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_id, local_type, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
                 VALUES (4242, 34, 1003, '', NULL, 7, x'')"
            ),
            [],
        )
        .unwrap();
        drop(conn);

        let contains = vec!["[voice:4242]".to_string()];
        let (count, output) = run_messages_for_test(dir.path(), &contains, &[]).unwrap();

        assert_eq!(count, 1);
        assert!(output.contains("[voice:4242]"));
    }

    #[test]
    fn message_query_decodes_indexed_xml_body() {
        let dir = create_query_test_dir();
        let conn = Connection::open(dir.path().join("message/message_0.db")).unwrap();
        let marker_col = dictionary::msg_compression_marker_column();
        let packed_col = dictionary::msg_packed_meta_column();
        let create_time = crate::time::default_recent_since() + 1;
        let quote_xml = r#"<msg><appmsg><title>索引回复</title><type>57</type><refermsg><type>1</type><displayname>Bob</displayname><content>索引引用</content></refermsg></appmsg></msg>"#;
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_type, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
                 VALUES (49, ?1, ?2, NULL, 7, x'')"
            ),
            params![create_time, quote_xml],
        )
        .unwrap();
        drop(conn);
        crate::message_index::ensure_recent(dir.path(), 1).unwrap();

        let contains = vec!["索引回复".to_string()];
        let mut bytes = Vec::new();
        let mut out = output::Output::new(&mut bytes);
        let mut options = default_query_options(dir.path(), &contains, &[]);
        options.since = Some(crate::time::default_recent_since());
        let count = run_messages_with_output(options, &mut out).unwrap();
        out.flush().unwrap();
        drop(out);
        let output = String::from_utf8(bytes).unwrap();

        assert_eq!(count, 1);
        assert!(output.contains("> Bob: 索引引用\\n 索引回复"));
        assert!(!output.contains("<appmsg"));
    }

    #[test]
    fn message_query_uses_indexed_decoded_body_for_contains() {
        let dir = create_query_test_dir();
        let conn = Connection::open(dir.path().join("message/message_0.db")).unwrap();
        let marker_col = dictionary::msg_compression_marker_column();
        let packed_col = dictionary::msg_packed_meta_column();
        let create_time = crate::time::default_recent_since() + 1;
        let xml = r#"<msg><appmsg><title>indexed visible title</title><type>5</type><secret>indexed-raw-only</secret></appmsg></msg>"#;
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_type, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
                 VALUES (49, ?1, ?2, NULL, 7, x'')"
            ),
            params![create_time, xml],
        )
        .unwrap();
        drop(conn);
        crate::message_index::ensure_recent(dir.path(), 1).unwrap();

        let contains = vec!["indexed-raw-only".to_string()];
        let mut bytes = Vec::new();
        let mut out = output::Output::new(&mut bytes);
        let mut options = default_query_options(dir.path(), &contains, &[]);
        options.since = Some(crate::time::default_recent_since());
        let count = run_messages_with_output(options, &mut out).unwrap();
        out.flush().unwrap();
        drop(out);
        assert_eq!(count, 0);

        let raw_contains = vec!["indexed-raw-only".to_string()];
        let mut bytes = Vec::new();
        let mut out = output::Output::new(&mut bytes);
        let mut options = default_query_options(dir.path(), &[], &[]);
        options.raw_contains = &raw_contains;
        options.since = Some(crate::time::default_recent_since());
        let count = run_messages_with_output(options, &mut out).unwrap();
        out.flush().unwrap();
        drop(out);
        let output = String::from_utf8(bytes).unwrap();

        assert_eq!(count, 1);
        assert!(output.contains("indexed visible title"));
        assert!(!output.contains("indexed-raw-only"));
    }

    #[test]
    fn message_query_uses_indexed_voice_media_and_local_id() {
        let dir = create_query_test_dir();
        let conn = Connection::open(dir.path().join("message/message_0.db")).unwrap();
        let marker_col = dictionary::msg_compression_marker_column();
        let packed_col = dictionary::msg_packed_meta_column();
        conn.execute("ALTER TABLE Msg_test ADD COLUMN local_id INTEGER", [])
            .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_id, local_type, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
                 VALUES (5151, 34, ?1, '', NULL, 7, x'')"
            ),
            params![crate::time::default_recent_since() + 1],
        )
        .unwrap();
        drop(conn);
        crate::message_index::ensure_recent(dir.path(), 1).unwrap();

        let has = vec![QueryMediaType::Voice];
        let mut bytes = Vec::new();
        let mut out = output::Output::new(&mut bytes);
        let mut options = default_query_options(dir.path(), &[], &[]);
        options.has = &has;
        options.since = Some(crate::time::default_recent_since());
        let count = run_messages_with_output(options, &mut out).unwrap();
        out.flush().unwrap();
        drop(out);
        let output = String::from_utf8(bytes).unwrap();

        assert_eq!(count, 1);
        assert!(output.contains("[voice:5151]"));
    }

    #[test]
    fn message_query_uses_indexed_media_type_column_for_has() {
        let dir = tempdir().unwrap();
        let index_path = dir.path().join(".tg_index.db");
        let conn = Connection::open(&index_path).unwrap();
        conn.execute_batch(
            "PRAGMA user_version = 4;
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
                 media_type TEXT NOT NULL,
                 create_time INTEGER NOT NULL,
                 raw_body TEXT NOT NULL,
                 decoded_body TEXT NOT NULL,
                 body TEXT NOT NULL,
                 marker INTEGER,
                 packed_info BLOB NOT NULL
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (
                source_db, table_name, session_id, session_display,
                sender_account, sender_display, local_id, local_type,
                media_type, create_time, raw_body, decoded_body, body, marker, packed_info
             ) VALUES (
                'message/message_0.db', 'Msg_test', 'tgid_test', 'Test',
                '', '', 1, 49, 'file', ?1, '<msg><appmsg><type>5</type></appmsg></msg>',
                '[文件] report.pdf', '<msg><appmsg><type>5</type></appmsg></msg>', NULL, x''
             )",
            params![crate::time::default_recent_since() + 1],
        )
        .unwrap();
        drop(conn);

        let index = message_index::HotIndex {
            path: index_path,
            since: crate::time::default_recent_since(),
        };
        let has = vec![QueryMediaType::File];
        let mut options = default_query_options(dir.path(), &[], &[]);
        options.has = &has;
        options.since = Some(crate::time::default_recent_since());

        let rows = query_indexed_messages(&index, &options).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body, "[文件] report.pdf");
    }

    #[test]
    fn message_query_treats_input_as_bound_text() {
        let dir = create_query_test_dir();
        let contains = vec!["' OR 1=1 --".to_string()];

        let (count, output) = run_messages_for_test(dir.path(), &contains, &[]).unwrap();

        assert_eq!(count, 0);
        assert!(output.is_empty());
    }

    #[test]
    fn message_query_treats_like_wildcards_as_text() {
        let dir = create_query_test_dir();
        let contains = vec!["%".to_string()];

        let (count, output) = run_messages_for_test(dir.path(), &contains, &[]).unwrap();

        assert_eq!(count, 1);
        assert!(output.contains("literal 100%_match"));
    }

    #[test]
    fn message_query_rejects_empty_keywords() {
        let dir = create_query_test_dir();
        let contains = vec!["   ".to_string()];
        let err = run_messages_for_test(dir.path(), &contains, &[]).unwrap_err();

        assert!(err.contains("--contains cannot be empty"));
    }

    #[test]
    fn message_query_rejects_large_result_windows() {
        let dir = create_query_test_dir();
        let contains = vec!["needle".to_string()];
        let mut bytes = Vec::new();
        let mut out = output::Output::new(&mut bytes);
        let mut options = default_query_options(dir.path(), &contains, &[]);
        options.limit = MAX_QUERY_RESULT_WINDOW;
        options.offset = 1;

        let err = run_messages_with_output(options, &mut out).unwrap_err();

        assert!(err.contains("--limit plus --offset"));
    }

    #[test]
    fn message_query_rejects_large_table_cells() {
        let dir = create_query_test_dir();
        let contains = vec!["needle".to_string()];
        let mut bytes = Vec::new();
        let mut out = output::Output::new(&mut bytes);
        let mut options = default_query_options(dir.path(), &contains, &[]);
        options.max_cell_chars = MAX_TABLE_CELL_CHARS + 1;

        let err = run_messages_with_output(options, &mut out).unwrap_err();

        assert!(err.contains("--max-cell-chars"));
    }

    #[test]
    fn message_query_supports_negative_keyword() {
        let dir = create_query_test_dir();
        let contains = vec!["update".to_string()];
        let not_contains = vec!["ordinary".to_string()];

        let (count, output) = run_messages_for_test(dir.path(), &contains, &not_contains).unwrap();

        assert_eq!(count, 0);
        assert!(output.is_empty());
    }

    #[test]
    fn message_query_requires_a_bound() {
        let dir = create_query_test_dir();
        let err = run_messages_for_test(dir.path(), &[], &[]).unwrap_err();

        assert!(err.contains("unbounded"));
    }

    #[test]
    fn schema_lists_public_query_contract() {
        let dir = create_query_test_dir();
        let mut bytes = Vec::new();
        let mut out = output::Output::new(&mut bytes);

        let count = run_schema_with_output(
            SchemaOptions {
                decrypted_dir: dir.path(),
                db_target: "message_0",
                format: QueryOutputFormat::Table,
                max_cell_chars: 500,
            },
            &mut out,
        )
        .unwrap();

        assert!(count >= 1);
        drop(out);
        let output = String::from_utf8(bytes).unwrap();
        assert!(output.contains("section\tname\tvalue\tdescription"));
        assert!(output.contains("field\tbody\tstring"));
        assert!(!output.contains("CREATE TABLE"));
        assert!(!output.contains("Msg_test"));
    }

    #[test]
    fn message_query_opens_database_read_only() {
        let dir = create_query_test_dir();
        let path = dir.path().join("message/message_0.db");

        let conn = open_readonly(&path).unwrap();

        assert!(conn.is_readonly(rusqlite::MAIN_DB).unwrap());
        assert!(conn
            .execute("CREATE TABLE write_probe (id INTEGER)", [])
            .is_err());
    }

    #[test]
    fn table_cells_escape_terminal_control_chars() {
        let output = escape_table_cell("ok\x1b[31m\tred\nnext\r\x7f", 100);

        assert_eq!(output, "ok\\u{001b}[31m\\tred\\nnext\\r\\u{007f}");
    }
}
