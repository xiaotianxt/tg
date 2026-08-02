use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};
use serde_json::{Map, Number, Value as JsonValue};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{contact, db, dictionary, media, message, output, parallel, paths};

const MAX_QUERY_KEYWORDS: usize = 16;
const MAX_QUERY_KEYWORD_CHARS: usize = 256;
const MAX_QUERY_RESULT_WINDOW: usize = 10_000;
const MAX_TABLE_CELL_CHARS: usize = 10_000;

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryOutputFormat {
    #[value(help = "Table output")]
    Table,
    #[value(help = "JSON output")]
    Json,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuerySort {
    #[value(help = "Newest messages first")]
    Newest,
    #[value(help = "Oldest messages first")]
    Oldest,
}

impl QuerySort {
    fn order_dir(self) -> &'static str {
        match self {
            Self::Newest => "DESC",
            Self::Oldest => "ASC",
        }
    }
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryMatchMode {
    #[value(help = "Require every keyword")]
    All,
    #[value(help = "Match any keyword")]
    Any,
}

impl QueryMatchMode {
    fn joiner(self) -> &'static str {
        match self {
            Self::All => " AND ",
            Self::Any => " OR ",
        }
    }
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryMediaType {
    #[value(help = "Voice messages")]
    Voice,
    #[value(help = "Image messages")]
    Image,
    #[value(help = "Sticker messages")]
    Sticker,
    #[value(help = "File attachments")]
    File,
    #[value(help = "Video messages")]
    Video,
}

impl QueryMediaType {
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

pub(crate) fn run(options: QueryOptions<'_>) -> Result<usize, String> {
    let stdout = std::io::stdout();
    let mut out = output::Output::new(stdout.lock());
    run_messages_with_output(options, &mut out)
}

fn run_messages_with_output<W: Write>(
    options: QueryOptions<'_>,
    out: &mut output::Output<W>,
) -> Result<usize, String> {
    validate_message_options(&options)?;
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
        || media_filters_need_post_filtering(options)
}

fn media_filters_need_post_filtering(options: &QueryOptions<'_>) -> bool {
    options
        .has
        .iter()
        .any(|media_type| matches!(media_type, QueryMediaType::File | QueryMediaType::Video))
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
            label: paths::relative_label(options.decrypted_dir, &path),
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
    let conn = crate::sqlite::open_immutable(&target.path)?;
    let tables = match &context.selected_table {
        Some(table) => vec![table.clone()],
        None => db::list_message_tables(&conn)?,
    };

    let name2id = db::load_name2id(&conn);
    let mut rows = Vec::new();
    for table in tables {
        if !db::message_table_exists(&conn, &table) {
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
    let order_col = if db::table_has_column(conn, table, "sort_seq") {
        "sort_seq"
    } else {
        "create_time"
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
                params.push(Value::Text(db::like_contains_pattern(query)));
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
         ORDER BY {order_col} {order_dir}{limit_clause}",
        body_col = body_col,
        marker_col = marker_col,
        sender_col = sender_col,
        packed_col = packed_col,
        local_id_col = local_id_col,
        quoted_table = quoted_table,
        order_col = order_col,
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
            let raw_body = db::read_message_body(row, 2, marker);
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
        .take(result_window)
        .collect())
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
        QueryMediaType::Video => format!(
            "((local_type & 4294967295) = 43 OR ((local_type & 4294967295) = 49 AND (local_type >> 32) = 51) OR ((local_type & 4294967295) = 49 AND {raw_body_col} LIKE '%<finderFeed>%'))"
        ),
        QueryMediaType::File => format!(
            "((local_type & 4294967295) = 62 OR (local_type & 4294967295) = 49 OR {raw_body_col} LIKE '%<type>6</type>%' OR {raw_body_col} LIKE '%<type>62</type>%')"
        ),
    }
}

fn message_has_media(local_type: i64, raw_body: &str, media_type: QueryMediaType) -> bool {
    media::message_media_type(local_type, raw_body) == Some(media_type.as_str())
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

fn message_field_json(row: &MessageRow, field: MessageField) -> JsonValue {
    match field {
        MessageField::Type => JsonValue::Number(Number::from(row.local_type)),
        MessageField::Timestamp => JsonValue::Number(Number::from(row.create_time)),
        _ => JsonValue::String(message_field_text(row, field)),
    }
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
        let short_video_type = (51_i64 << 32) | 49;
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
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_type, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
                 VALUES (?1, 1005, '<msg><appmsg><title>我拍了拍 \"Bob\"</title><type>62</type><patinfo><template>我拍了拍 \"${{tgid_bob}}\"</template></patinfo></appmsg></msg>', NULL, 7, x'')"
            ),
            params![(62_i64 << 32) | 49],
        )
        .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_type, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
                 VALUES (?1, 1006, '<msg><appmsg><title>当前版本不支持展示该内容，请升级至最新版本。</title><type>51</type><finderFeed><feedType>4</feedType><nickname>Alice</nickname><desc>short clip</desc><mediaList><media><mediaType>4</mediaType></media></mediaList></finderFeed></appmsg></msg>', NULL, 7, x'')"
            ),
            params![short_video_type],
        )
        .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO Msg_test (local_type, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
                 VALUES (?1, 1007, '<msg><appmsg><title>Chat History</title><type>51</type></appmsg></msg>', NULL, 7, x'')"
            ),
            params![short_video_type],
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
        assert!(!output.contains("拍了拍"));
        assert!(!output.contains("[语音"));

        let has = vec![QueryMediaType::Video];
        let mut bytes = Vec::new();
        let mut out = output::Output::new(&mut bytes);
        let mut options = default_query_options(dir.path(), &[], &[]);
        options.has = &has;
        let count = run_messages_with_output(options, &mut out).unwrap();
        out.flush().unwrap();
        drop(out);
        let output = String::from_utf8(bytes).unwrap();

        assert_eq!(count, 1);
        assert!(output.contains("short clip"));
        assert!(!output.contains("Chat History"));
        assert!(!output.contains("report.pdf"));
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
    fn message_query_decodes_xml_body() {
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
    fn message_query_uses_decoded_body_for_contains() {
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
    fn message_query_uses_voice_media_and_local_id() {
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
    fn message_query_opens_database_read_only() {
        let dir = create_query_test_dir();
        let path = dir.path().join("message/message_0.db");

        let conn = crate::sqlite::open_immutable(&path).unwrap();

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
