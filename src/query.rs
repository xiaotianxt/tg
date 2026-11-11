use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};
use serde_json::{Map, Number, Value as JsonValue};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::io::Write;
#[cfg(test)]
use std::path::Path;
use std::sync::Mutex;

use crate::db::MessageOrder;
use crate::source_store::SourceStore;
use crate::{contact, db, dictionary, media, message, output, parallel, sender};

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
    SenderId,
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
                "sender_id" => MessageField::SenderId,
                "type" | "local_type" => MessageField::Type,
                "body" | "text" => MessageField::Body,
                "raw_body" | "raw" => MessageField::RawBody,
                "timestamp" | "create_time" => MessageField::Timestamp,
                other => {
                    return Err(format!(
                        "unknown field '{}'; expected time, session, sender, sender_id, type, body, raw_body, timestamp",
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
            Self::SenderId => "sender_id",
            Self::Type => "type",
            Self::Body => "body",
            Self::RawBody => "raw_body",
            Self::Timestamp => "timestamp",
        }
    }
}

pub(crate) struct QueryOptions<'a> {
    pub store: &'a SourceStore,
    pub session: Option<&'a str>,
    pub contains: &'a [String],
    pub not_contains: &'a [String],
    pub raw_contains: &'a [String],
    pub has: &'a [QueryMediaType],
    /// Only messages from this sender: `me`, an account id, or a shown name.
    pub sender: Option<&'a str>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// None returns every match (MCP); the CLI always passes a page size.
    pub limit: Option<usize>,
    pub offset: usize,
    pub sort: MessageOrder,
    pub match_mode: QueryMatchMode,
    pub name_mode: contact::DisplayNameMode,
    pub jobs: usize,
}

#[derive(Debug, Clone)]
struct QueryTarget {
    label: String,
}

#[derive(Debug, Clone)]
struct MessageQueryContext {
    targets: Vec<QueryTarget>,
    selected_table: Option<String>,
    table_to_session: HashMap<String, String>,
    table_to_display: HashMap<String, String>,
    sender_display: HashMap<String, String>,
    sender: Option<sender::SenderFilter>,
    account_username: Option<String>,
}

/// How query results are printed; separate from the filters so non-CLI callers
/// can run a query without choosing a text layout.
pub(crate) struct QueryOutput {
    pub fields: QueryFields,
    pub format: QueryOutputFormat,
    pub max_cell_chars: usize,
}

impl QueryOutput {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.max_cell_chars > MAX_TABLE_CELL_CHARS {
            return Err(format!(
                "--max-cell-chars must be <= {}",
                MAX_TABLE_CELL_CHARS
            ));
        }
        Ok(())
    }

    pub(crate) fn write<W: Write>(
        &self,
        out: &mut output::Output<W>,
        rows: &[MessageRow],
    ) -> Result<(), String> {
        write_message_rows(out, rows, &self.fields, self.format, self.max_cell_chars)?;
        out.flush()
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct MessageRow {
    /// Display name of the session.
    pub session: String,
    /// Session username, or the table name when the session is unknown.
    pub session_id: String,
    pub sender: String,
    /// Sender account username; empty for system messages.
    pub sender_id: String,
    pub local_type: i64,
    pub create_time: i64,
    pub body: String,
    pub raw_body: String,
}

/// Run a validated query and return the requested window, globally sorted.
pub(crate) fn query_messages(options: &QueryOptions<'_>) -> Result<Vec<MessageRow>, String> {
    validate_message_options(options)?;
    let context = build_message_context(options)?;
    if context.targets.is_empty() {
        return Err("No message databases found in the source directory.".to_string());
    }

    let jobs = parallel::job_count(options.jobs, parallel::QUERY_SCAN_AUTO_CAP);
    let mut rows = scan_message_tables(options, &context, jobs)?;

    sort_message_rows(&mut rows, options.sort);
    Ok(rows
        .into_iter()
        .skip(options.offset)
        .take(options.limit.unwrap_or(usize::MAX))
        .map(|candidate| candidate.finish(&context))
        .collect())
}

/// Decode inputs of a row whose body no filter reads. Ordering uses only
/// create_time, so decoding can wait until the final window is cut.
struct DeferredBody {
    marker: Option<i64>,
    packed_info: Vec<u8>,
    local_id: i64,
}

struct CandidateRow {
    row: MessageRow,
    deferred: Option<DeferredBody>,
}

impl CandidateRow {
    fn finish(self, context: &MessageQueryContext) -> MessageRow {
        let mut row = self.row;
        if let Some(deferred) = self.deferred {
            row.body = decode_query_body(
                row.local_type,
                &row.raw_body,
                deferred.marker,
                &deferred.packed_info,
                Some(deferred.local_id),
                &row.session,
                &context.sender_display,
            );
        }
        row
    }
}

fn validate_message_options(options: &QueryOptions<'_>) -> Result<(), String> {
    if options.limit == Some(0) {
        return Err("--limit must be greater than 0".to_string());
    }
    validate_query_terms("--contains", options.contains)?;
    validate_query_terms("--not", options.not_contains)?;
    validate_query_terms("--raw-contains", options.raw_contains)?;
    message_query_window(options)?;
    if options.contains.is_empty()
        && options.raw_contains.is_empty()
        && options.has.is_empty()
        && options.sender.is_none()
        && options.session.is_none()
        && options.since.is_none()
    {
        return Err(
            "refusing to return every message of every chat; pass --session, --contains, --raw-contains, --has, --sender, or --since"
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

/// The CLI's guard against huge pages; an unlimited query has no window.
fn message_query_window(options: &QueryOptions<'_>) -> Result<Option<usize>, String> {
    let Some(limit) = options.limit else {
        return Ok(None);
    };
    let window = limit
        .checked_add(options.offset)
        .ok_or_else(|| "--limit plus --offset is too large".to_string())?;
    if window > MAX_QUERY_RESULT_WINDOW {
        return Err(format!(
            "--limit plus --offset must be <= {}",
            MAX_QUERY_RESULT_WINDOW
        ));
    }
    Ok(Some(window))
}

fn query_filters_body(options: &QueryOptions<'_>) -> bool {
    !options.contains.is_empty() || !options.not_contains.is_empty()
}

fn query_needs_post_filtering(options: &QueryOptions<'_>) -> bool {
    query_filters_body(options) || media_filters_need_post_filtering(options)
}

fn media_filters_need_post_filtering(options: &QueryOptions<'_>) -> bool {
    options
        .has
        .iter()
        .any(|media_type| matches!(media_type, QueryMediaType::File | QueryMediaType::Video))
}

fn build_message_context(options: &QueryOptions<'_>) -> Result<MessageQueryContext, String> {
    let message_dbs = db::message_databases(options.store);
    let contact_conn = if db::has_database(options.store, "contact/contact.db") {
        Some(options.store.open("contact/contact.db")?)
    } else {
        None
    };
    let contacts = match &contact_conn {
        Some(conn) => contact::load_contacts(conn)?,
        None => HashMap::new(),
    };

    let mut table_to_session = HashMap::new();
    let mut table_to_display = HashMap::new();
    let mut sender_display = HashMap::new();
    for (username, contact) in &contacts {
        let display = contact.display_name(options.name_mode).to_string();
        table_to_session.insert(db::msg_table_name(username), username.clone());
        table_to_display.insert(db::msg_table_name(username), display.clone());
        sender_display.insert(username.clone(), display);
    }

    let mut session_username = None;
    let selected_table = match options.session {
        Some(session) => {
            let username = db::resolve_username_for_messages(
                session,
                &contacts,
                options.store,
                &message_dbs,
                options.jobs,
            )?;
            let table = db::msg_table_name(&username);
            table_to_session
                .entry(table.clone())
                .or_insert_with(|| username.clone());
            table_to_display
                .entry(table.clone())
                .or_insert_with(|| username.clone());
            session_username = Some(username);
            Some(table)
        }
        None => None,
    };
    let sender = match options.sender {
        Some(value) => {
            let group = match &session_username {
                Some(username) => sender::load_group_names(contact_conn.as_ref(), username)?,
                None => sender::GroupNames::default(),
            };
            Some(sender::resolve_sender(
                value,
                options.store,
                &message_dbs,
                &contacts,
                &group,
                options.name_mode,
            )?)
        }
        None => None,
    };

    let targets = message_dbs
        .into_iter()
        .map(|label| QueryTarget { label })
        .collect();

    Ok(MessageQueryContext {
        targets,
        selected_table,
        table_to_session,
        table_to_display,
        sender_display,
        sender,
        account_username: options.store.account_username().map(str::to_string),
    })
}

/// A message table and what is needed to read it, found once per shard.
struct TablePlan {
    name: String,
    schema: db::MessageTableSchema,
    session: String,
    session_id: String,
    /// Inclusive rowid bounds, or `None` for an empty table.
    rowids: Option<(i64, i64)>,
}

struct ShardPlan {
    label: String,
    tables: Vec<TablePlan>,
    name2id: HashMap<i64, String>,
    /// `Name2Id` rowids of the filtered senders in this shard.
    sender_rowids: Option<Vec<i64>>,
}

/// Half-open rowid range `[start, end)`; a missing bound is unbounded, so the
/// first and last chunk of a table also cover rows outside the planned bounds.
#[derive(Clone, Copy)]
struct RowidRange {
    start: Option<i64>,
    end: Option<i64>,
}

/// A table, or a rowid range of one.
struct ScanPart {
    table: usize,
    range: RowidRange,
}

/// One unit of reader work: consecutive parts of one shard, read in order on
/// one connection.
struct ScanTask {
    shard: usize,
    parts: Vec<ScanPart>,
    weight: i128,
}

type ScannedRows = Vec<(db::MessageOrderKey, CandidateRow)>;

/// Smallest rowid range worth its own reader; smaller tables are read whole.
const MIN_SCAN_CHUNK_ROWS: i128 = 4096;

/// Ranges per reader at most, so readers that finish early find more work.
const SCAN_CHUNKS_PER_READER: i128 = 4;

/// Reads every selected table with `jobs` readers and returns the rows in
/// shard, table, message order, each table cut to the result window.
///
/// Tables read in full are split into rowid ranges so one large shard or chat
/// does not leave the other readers idle. Ranges are exact: each row belongs to
/// exactly one range, and a table's ranges are merged and sorted by their
/// unique (order column, rowid) key before the window is cut, as one read of
/// the whole table would be. Each reader holds its own connection, so each has
/// its own read snapshot of the shard, as separate shards already do.
fn scan_message_tables(
    options: &QueryOptions<'_>,
    context: &MessageQueryContext,
    jobs: usize,
) -> Result<Vec<CandidateRow>, String> {
    let planned = parallel::map_ordered(context.targets.clone(), jobs, |target| {
        plan_message_shard(options.store, &target, context)
    });
    let mut plans = Vec::with_capacity(planned.len());
    let mut pools = Vec::with_capacity(planned.len());
    for result in planned {
        match result {
            Ok((plan, conn)) => {
                plans.push(Ok(plan));
                pools.push(Mutex::new(vec![conn]));
            }
            Err(error) => {
                plans.push(Err(error));
                pools.push(Mutex::new(Vec::new()));
            }
        }
    }

    let tasks = plan_scan_tasks(&plans, jobs, query_reads_whole_tables(options)?);
    let mut schedule: Vec<usize> = (0..tasks.len()).collect();
    // Largest first, so the long reads start early and short ones fill the gaps.
    schedule.sort_by_key(|&index| Reverse(tasks[index].weight));

    let scanned = parallel::map_ordered(schedule.clone(), jobs, |index| {
        let task = &tasks[index];
        let plan = plans[task.shard]
            .as_ref()
            .expect("scan tasks come only from planned shards");
        let pooled = pools[task.shard]
            .lock()
            .expect("connection pool lock is never poisoned")
            .pop();
        let conn = match pooled {
            Some(conn) => conn,
            None => match options.store.open(&plan.label) {
                Ok(conn) => conn,
                Err(error) => return vec![Err(error)],
            },
        };
        let mut results = Vec::with_capacity(task.parts.len());
        for part in &task.parts {
            let result = query_message_table(
                &conn,
                plan,
                &plan.tables[part.table],
                part.range,
                context,
                options,
            );
            let failed = result.is_err();
            results.push(result);
            // Later parts are never reached: the merge stops at this error.
            if failed {
                break;
            }
        }
        pools[task.shard]
            .lock()
            .expect("connection pool lock is never poisoned")
            .push(conn);
        results
    });
    let mut by_task: Vec<Vec<Result<ScannedRows, String>>> =
        (0..tasks.len()).map(|_| Vec::new()).collect();
    for (index, results) in schedule.into_iter().zip(scanned) {
        by_task[index] = results;
    }

    // Tasks and their parts are in shard, table, range order, so errors
    // surface in the order a sequential read would hit them.
    let result_window = message_query_window(options)?;
    let mut parts = tasks
        .iter()
        .zip(by_task)
        .flat_map(|(task, results)| {
            let shard = task.shard;
            let mut results = results.into_iter();
            task.parts
                .iter()
                .map(move |part| (shard, part.table, results.next()))
        })
        .peekable();
    let mut rows = Vec::new();
    let mut table_rows: ScannedRows = Vec::new();
    for (shard, plan) in plans.iter().enumerate() {
        plan.as_ref().map_err(String::clone)?;
        while let Some((_, table, result)) = parts.next_if(|(part_shard, ..)| *part_shard == shard)
        {
            table_rows.append(&mut result.expect("a task stops only after a failed part")?);
            let table_done = parts.peek().is_none_or(|(next_shard, next_table, _)| {
                (*next_shard, *next_table) != (shard, table)
            });
            if table_done {
                db::sort_in_message_order(&mut table_rows, options.sort);
                rows.extend(
                    table_rows
                        .drain(..)
                        .take(result_window.unwrap_or(usize::MAX))
                        .map(|(_, row)| row),
                );
            }
        }
    }
    Ok(rows)
}

fn plan_message_shard(
    store: &SourceStore,
    target: &QueryTarget,
    context: &MessageQueryContext,
) -> Result<(ShardPlan, Connection), String> {
    let conn = store.open(&target.label)?;
    let sender_rowids = match &context.sender {
        Some(filter) => {
            let rowids = filter.rowids(&conn, &target.label)?;
            if rowids.is_empty() {
                // No message here is from the sender, so plan no tables.
                let plan = ShardPlan {
                    label: target.label.clone(),
                    tables: Vec::new(),
                    name2id: HashMap::new(),
                    sender_rowids: Some(rowids),
                };
                return Ok((plan, conn));
            }
            Some(rowids)
        }
        None => None,
    };
    let tables = match &context.selected_table {
        Some(table) => db::message_table_schema(&conn, table)?
            .map(|schema| (table.clone(), schema))
            .into_iter()
            .collect(),
        None => db::message_table_schemas(&conn)?,
    };
    let name2id = db::load_name2id(&conn)?;

    let mut planned = Vec::with_capacity(tables.len());
    for (name, schema) in tables {
        let quoted = db::quote_identifier(&name);
        // Two subqueries, because SQLite answers a lone min() or max() of the
        // rowid from the b-tree edge but scans the table for both together.
        let rowids = conn
            .query_row(
                &format!(
                    "SELECT (SELECT min(rowid) FROM {quoted}), (SELECT max(rowid) FROM {quoted})"
                ),
                [],
                |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .map_err(|e| format!("Message rowid bounds in {}: {}", target.label, e))?;
        let session_id = context
            .table_to_session
            .get(&name)
            .cloned()
            .unwrap_or_else(|| name.clone());
        let session = context
            .table_to_display
            .get(&name)
            .cloned()
            .unwrap_or_else(|| session_id.clone());
        planned.push(TablePlan {
            name,
            schema,
            session,
            session_id,
            rowids: match rowids {
                (Some(min), Some(max)) => Some((min, max)),
                _ => None,
            },
        });
    }
    let plan = ShardPlan {
        label: target.label.clone(),
        tables: planned,
        name2id,
        sender_rowids,
    };
    Ok((plan, conn))
}

/// True when tables are read in full and sorted afterwards; false when a
/// pushed-down LIMIT lets the index walk stop early.
fn query_reads_whole_tables(options: &QueryOptions<'_>) -> Result<bool, String> {
    Ok(message_query_window(options)?.is_none() || query_needs_post_filtering(options))
}

/// Scan tasks in shard, table, range order. With `splits`, tables larger than
/// one chunk become contiguous rowid ranges sized to give each reader several,
/// and smaller tables of a shard are packed together up to one chunk, so a
/// small shard is not opened by several readers.
fn plan_scan_tasks(
    plans: &[Result<ShardPlan, String>],
    jobs: usize,
    splits: bool,
) -> Vec<ScanTask> {
    // i128 holds any rowid span; rowids may be sparse, so a span only
    // estimates the row count and the parts per table are capped.
    let span = |table: &TablePlan| {
        table
            .rowids
            .map_or(0, |(min, max)| i128::from(max) - i128::from(min) + 1)
    };
    let total: i128 = plans
        .iter()
        .flatten()
        .flat_map(|plan| &plan.tables)
        .map(span)
        .sum();
    let max_parts =
        i128::try_from(jobs.max(1)).expect("job count fits in i128") * SCAN_CHUNKS_PER_READER;
    let chunk = (total / max_parts).max(MIN_SCAN_CHUNK_ROWS);

    let mut tasks = Vec::new();
    for (shard, plan) in plans.iter().enumerate() {
        let Ok(plan) = plan else { continue };
        let mut packed = Vec::new();
        let mut packed_rows = 0;
        for (table, table_plan) in plan.tables.iter().enumerate() {
            let rows = span(table_plan);
            let split = splits && rows > chunk;
            if !packed.is_empty() && (split || packed_rows + rows > chunk) {
                tasks.push(ScanTask {
                    shard,
                    parts: std::mem::take(&mut packed),
                    weight: std::mem::take(&mut packed_rows),
                });
            }
            if !split {
                packed.push(ScanPart {
                    table,
                    range: RowidRange {
                        start: None,
                        end: None,
                    },
                });
                packed_rows += rows;
                continue;
            }
            let (min, max) = table_plan
                .rowids
                .expect("a table with rows has rowid bounds");
            let parts = ((rows + chunk - 1) / chunk).min(max_parts);
            let part_rows = (rows + parts - 1) / parts;
            // Bounds never decrease, so the ranges stay disjoint and together
            // cover every rowid; clamping to `max` keeps each one a rowid.
            let bound = |part: i128| {
                let bound = (i128::from(min) + part * part_rows).min(i128::from(max));
                i64::try_from(bound).expect("a bound clamped to max is a rowid")
            };
            for part in 0..parts {
                tasks.push(ScanTask {
                    shard,
                    parts: vec![ScanPart {
                        table,
                        range: RowidRange {
                            start: (part > 0).then(|| bound(part)),
                            end: (part + 1 < parts).then(|| bound(part + 1)),
                        },
                    }],
                    weight: part_rows.min(rows - part * part_rows),
                });
            }
        }
        if !packed.is_empty() {
            tasks.push(ScanTask {
                shard,
                parts: packed,
                weight: packed_rows,
            });
        }
    }
    tasks
}

fn query_message_table(
    conn: &Connection,
    shard: &ShardPlan,
    table: &TablePlan,
    range: RowidRange,
    context: &MessageQueryContext,
    options: &QueryOptions<'_>,
) -> Result<ScannedRows, String> {
    let schema = table.schema;
    let name2id = &shard.name2id;
    let body_col = db::quote_identifier(dictionary::msg_body_column());
    let marker_col = db::quote_identifier(dictionary::msg_compression_marker_column());
    let sender_col = db::quote_identifier(dictionary::msg_sender_column());
    let packed_col = if schema.has_packed_meta {
        db::quote_identifier(dictionary::msg_packed_meta_column())
    } else {
        "x''".to_string()
    };
    let local_id_col = if schema.has_local_id {
        db::quote_identifier("local_id")
    } else {
        "rowid".to_string()
    };
    let order_col = if schema.has_sort_seq {
        "sort_seq"
    } else {
        "create_time"
    };
    let quoted_table = db::quote_identifier(&table.name);
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
    if let Some(start) = range.start {
        clauses.push("rowid >= ?".to_string());
        params.push(Value::Integer(start));
    }
    if let Some(end) = range.end {
        clauses.push("rowid < ?".to_string());
        params.push(Value::Integer(end));
    }
    if let Some(rowids) = &shard.sender_rowids {
        clauses.push(
            sender::sender_rowid_clause(&sender_col, rowids)
                .expect("shards without the sender plan no tables"),
        );
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

    // A pushed-down LIMIT can stop the index walk early. Without one the whole
    // table is read, so read it in rowid order and sort afterwards.
    let order_clause = match result_window {
        Some(window) if !query_needs_post_filtering(options) => {
            format!(
                " ORDER BY {order_col} {} LIMIT {window}",
                options.sort.sql_direction()
            )
        }
        _ => String::new(),
    };

    let sql = format!(
        "SELECT local_type, create_time, {body_col}, {marker_col}, {sender_col}, {packed_col}, {local_id_col}, {order_key} \
         FROM {quoted_table} \
         WHERE {where_clause}{order_clause}",
        body_col = body_col,
        marker_col = marker_col,
        sender_col = sender_col,
        packed_col = packed_col,
        local_id_col = local_id_col,
        order_key = db::message_order_key_columns(order_col),
        quoted_table = quoted_table,
        where_clause = clauses.join(" AND "),
        order_clause = order_clause,
    );

    let session = &table.session;
    let session_id = &table.session_id;
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("Message query prepare error in {}: {}", shard.label, e))?;
    let mut result = stmt
        .query(params_from_iter(params.iter()))
        .map_err(|e| format!("Message query error in {}: {}", shard.label, e))?;
    let read_error = |error| format!("Read message rows in {}: {error}", shard.label);
    // Filter while streaming so rejected rows are never materialized, and test
    // the media filter, which needs only the raw row, before decoding the body.
    // Without a body filter, decoding waits for the final window.
    let mut rows = Vec::new();
    while let Some(row) = result.next().map_err(read_error)? {
        let local_type = row
            .get::<_, Option<i64>>(0)
            .map_err(read_error)?
            .unwrap_or(-1);
        let marker: Option<i64> = row.get::<_, Option<i64>>(3).map_err(read_error)?;
        let raw_body = db::read_message_body(row, 2, marker).map_err(read_error)?;
        if !message_matches_media_filters(local_type, &raw_body, options) {
            continue;
        }
        let packed_info: Vec<u8> = row
            .get::<_, Option<Vec<u8>>>(5)
            .map_err(read_error)?
            .unwrap_or_default();
        let local_id = row
            .get::<_, Option<i64>>(6)
            .map_err(read_error)?
            .unwrap_or(0);
        let (body, deferred) = if query_filters_body(options) {
            let body = decode_query_body(
                local_type,
                &raw_body,
                marker,
                &packed_info,
                Some(local_id),
                session,
                &context.sender_display,
            );
            if !message_body_matches_filters(&body, options) {
                continue;
            }
            (body, None)
        } else {
            let deferred = DeferredBody {
                marker,
                packed_info,
                local_id,
            };
            (String::new(), Some(deferred))
        };
        let sender_id: i64 = row
            .get::<_, Option<i64>>(4)
            .map_err(read_error)?
            .unwrap_or(0);
        let sender_account = name2id.get(&sender_id).map_or("", String::as_str);
        let is_system =
            message::MessageType::from(media::local_type_low32(local_type) as i32).is_system();
        rows.push((
            db::read_message_order_key(row, 7).map_err(read_error)?,
            CandidateRow {
                row: MessageRow {
                    session: session.clone(),
                    session_id: session_id.clone(),
                    sender: display_sender(sender_account, context),
                    sender_id: if is_system {
                        String::new()
                    } else {
                        sender_account.to_string()
                    },
                    local_type,
                    create_time: row
                        .get::<_, Option<i64>>(1)
                        .map_err(read_error)?
                        .unwrap_or(0),
                    body,
                    raw_body,
                },
                deferred,
            },
        ));
    }
    Ok(rows)
}

fn display_sender(sender_account: &str, context: &MessageQueryContext) -> String {
    if sender_account.is_empty() {
        return String::new();
    }
    if sender::is_self(sender_account, context.account_username.as_deref()) {
        return sender::SELF_DISPLAY_NAME.to_string();
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

fn message_body_matches_filters(body: &str, options: &QueryOptions<'_>) -> bool {
    if !options.contains.is_empty() {
        let matched = match options.match_mode {
            QueryMatchMode::All => options.contains.iter().all(|term| body.contains(term)),
            QueryMatchMode::Any => options.contains.iter().any(|term| body.contains(term)),
        };
        if !matched {
            return false;
        }
    }

    !options.not_contains.iter().any(|term| body.contains(term))
}

fn message_matches_media_filters(
    local_type: i64,
    raw_body: &str,
    options: &QueryOptions<'_>,
) -> bool {
    options.has.is_empty()
        || options
            .has
            .iter()
            .any(|media_type| message_has_media(local_type, raw_body, *media_type))
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

fn sort_message_rows(rows: &mut [CandidateRow], sort: MessageOrder) {
    match sort {
        MessageOrder::Newest => rows.sort_by_key(|candidate| Reverse(candidate.row.create_time)),
        MessageOrder::Oldest => rows.sort_by_key(|candidate| candidate.row.create_time),
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
        MessageField::SenderId => row.sender_id.clone(),
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
        dir: &Path,
        contains: &'a [String],
        not_contains: &'a [String],
    ) -> QueryOptions<'a> {
        let store = Box::leak(Box::new(SourceStore::from_plaintext(dir).unwrap()));
        QueryOptions {
            store,
            session: None,
            contains,
            not_contains,
            raw_contains: &[],
            has: &[],
            sender: None,
            since: None,
            until: None,
            limit: Some(100),
            offset: 0,
            sort: MessageOrder::Newest,
            match_mode: QueryMatchMode::All,
            name_mode: contact::DisplayNameMode::PersonalRemark,
            jobs: 1,
        }
    }

    fn default_query_output() -> QueryOutput {
        QueryOutput {
            fields: QueryFields::parse("timestamp,body").unwrap(),
            format: QueryOutputFormat::Table,
            max_cell_chars: 500,
        }
    }

    fn run_messages_with_output<W: Write>(
        options: QueryOptions<'_>,
        query_output: &QueryOutput,
        out: &mut output::Output<W>,
    ) -> Result<usize, String> {
        query_output.validate()?;
        let rows = query_messages(&options)?;
        query_output.write(out, &rows)?;
        Ok(rows.len())
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
                &default_query_output(),
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
        let options = default_query_options(dir.path(), &contains, &[]);
        let mut query_output = default_query_output();
        query_output.fields = QueryFields::parse("timestamp,body,raw_body").unwrap();
        let count = run_messages_with_output(options, &query_output, &mut out).unwrap();
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
        let count = run_messages_with_output(options, &default_query_output(), &mut out).unwrap();
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
        let count = run_messages_with_output(options, &default_query_output(), &mut out).unwrap();
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
        let count = run_messages_with_output(options, &default_query_output(), &mut out).unwrap();
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
        let create_time = 1_700_000_000 + 1;
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
        options.since = Some(1_700_000_000);
        let count = run_messages_with_output(options, &default_query_output(), &mut out).unwrap();
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
        let create_time = 1_700_000_000 + 1;
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
        options.since = Some(1_700_000_000);
        let count = run_messages_with_output(options, &default_query_output(), &mut out).unwrap();
        out.flush().unwrap();
        drop(out);
        assert_eq!(count, 0);

        let raw_contains = vec!["indexed-raw-only".to_string()];
        let mut bytes = Vec::new();
        let mut out = output::Output::new(&mut bytes);
        let mut options = default_query_options(dir.path(), &[], &[]);
        options.raw_contains = &raw_contains;
        options.since = Some(1_700_000_000);
        let count = run_messages_with_output(options, &default_query_output(), &mut out).unwrap();
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
            params![1_700_000_000 + 1],
        )
        .unwrap();
        drop(conn);
        let has = vec![QueryMediaType::Voice];
        let mut bytes = Vec::new();
        let mut out = output::Output::new(&mut bytes);
        let mut options = default_query_options(dir.path(), &[], &[]);
        options.has = &has;
        options.since = Some(1_700_000_000);
        let count = run_messages_with_output(options, &default_query_output(), &mut out).unwrap();
        out.flush().unwrap();
        drop(out);
        let output = String::from_utf8(bytes).unwrap();

        assert_eq!(count, 1);
        assert!(output.contains("[voice:5151]"));
    }

    #[test]
    fn message_query_keeps_sort_seq_index_order_for_ties() {
        let dir = tempdir().unwrap();
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();
        let conn = Connection::open(message_dir.join("message_0.db")).unwrap();
        let marker_col = dictionary::msg_compression_marker_column();
        let packed_col = dictionary::msg_packed_meta_column();
        conn.execute_batch(&format!(
            "CREATE TABLE Msg_test (
                local_id INTEGER PRIMARY KEY AUTOINCREMENT, local_type INTEGER,
                sort_seq INTEGER, create_time INTEGER, message_content TEXT,
                {marker_col} INTEGER, real_sender_id INTEGER, {packed_col} BLOB);
             CREATE INDEX Msg_test_SORTSEQ ON Msg_test(sort_seq);"
        ))
        .unwrap();
        // Equal create_time keeps the per-table order through the global
        // stable sort, so sort_seq ties and NULLs must follow the index walk.
        let sort_seqs = [
            Some(5),
            Some(2),
            Some(5),
            None,
            Some(2),
            Some(9),
            None,
            Some(5),
        ];
        for (index, sort_seq) in sort_seqs.into_iter().enumerate() {
            conn.execute(
                &format!(
                    "INSERT INTO Msg_test (local_id, local_type, sort_seq, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
                     VALUES (?1, 1, ?2, 1000, ?3, NULL, 0, x'')"
                ),
                params![index as i64 + 1, sort_seq, format!("tie {}", index + 1)],
            )
            .unwrap();
        }

        let contains = ["tie".to_string()];
        for sort in [MessageOrder::Newest, MessageOrder::Oldest] {
            let expected: Vec<String> = conn
                .prepare(&format!(
                    "SELECT message_content FROM Msg_test ORDER BY sort_seq {}",
                    sort.sql_direction()
                ))
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            // --contains reads the whole table; --since pushes the LIMIT down.
            for (contains, since) in [(&contains[..], None), (&[][..], Some(1))] {
                let mut options = default_query_options(dir.path(), contains, &[]);
                options.sort = sort;
                options.since = since;
                let got: Vec<String> = query_messages(&options)
                    .unwrap()
                    .into_iter()
                    .map(|row| row.body)
                    .collect();
                assert_eq!(got, expected, "{sort:?} since={since:?}");
            }
        }
    }

    #[test]
    fn message_query_split_tables_match_whole_table_reads() {
        let dir = tempdir().unwrap();
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();
        let conn = Connection::open(message_dir.join("message_0.db")).unwrap();
        let marker_col = dictionary::msg_compression_marker_column();
        let packed_col = dictionary::msg_packed_meta_column();
        // Small tables before and after a large one, so packed and split
        // tasks interleave in the table order.
        let tables = [
            ("Msg_a", 300),
            ("Msg_b", 3 * MIN_SCAN_CHUNK_ROWS as i64 + 123),
            ("Msg_c", 200),
        ];
        let mut seed = 0x2545_f491_u64;
        let mut next = move |modulo: u64| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) % modulo
        };
        for (table, rows) in tables {
            conn.execute_batch(&format!(
                "CREATE TABLE {table} (
                    local_id INTEGER PRIMARY KEY AUTOINCREMENT, local_type INTEGER,
                    sort_seq INTEGER, create_time INTEGER, message_content TEXT,
                    {marker_col} INTEGER, real_sender_id INTEGER, {packed_col} BLOB);
                 CREATE INDEX {table}_SORTSEQ ON {table}(sort_seq);"
            ))
            .unwrap();
            let tx = conn.unchecked_transaction().unwrap();
            for id in 1..=rows {
                // Few distinct create_times and repeated or NULL sort_seqs, so
                // ties decide much of the order.
                let sort_seq = (next(10) != 0).then(|| next(rows as u64 / 4) as i64);
                let body = if next(3) == 0 { "miss" } else { "hit" };
                tx.execute(
                    &format!(
                        "INSERT INTO {table} (local_id, local_type, sort_seq, create_time, message_content, {marker_col}, real_sender_id, {packed_col})
                         VALUES (?1, 1, ?2, ?3, ?4, NULL, 0, x'')"
                    ),
                    params![id, sort_seq, 1000 + next(40) as i64, format!("{body} {table} {id}")],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }

        let contains = ["hit".to_string()];
        // Unlimited compares every row; the window also cuts each table early.
        let windows = [(None, 0), (Some(700), 150)];
        for (sort, (limit, offset)) in [MessageOrder::Newest, MessageOrder::Oldest]
            .into_iter()
            .flat_map(|sort| windows.map(|window| (sort, window)))
        {
            let window = limit.map_or(String::new(), |limit: usize| {
                format!(" LIMIT {}", limit + offset)
            });
            // Each table cut to the window in sort_seq order, then a stable
            // global sort on create_time, as a sequential whole-table read does.
            let mut expected: Vec<(i64, String)> = Vec::new();
            for (table, _) in tables {
                let dir = sort.sql_direction();
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT create_time, message_content FROM {table} \
                         WHERE message_content LIKE 'hit%' \
                         ORDER BY sort_seq {dir}, rowid {dir}{window}"
                    ))
                    .unwrap();
                let rows = stmt
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                    .unwrap();
                expected.extend(rows.map(Result::unwrap));
            }
            match sort {
                MessageOrder::Newest => expected.sort_by_key(|(time, _)| Reverse(*time)),
                MessageOrder::Oldest => expected.sort_by_key(|(time, _)| *time),
            }
            let expected: Vec<String> = expected
                .into_iter()
                .skip(offset)
                .take(limit.unwrap_or(usize::MAX))
                .map(|(_, body)| body)
                .collect();

            for jobs in [1, 3, 8] {
                let mut options = default_query_options(dir.path(), &contains, &[]);
                options.sort = sort;
                options.limit = limit;
                options.offset = offset;
                options.jobs = jobs;
                let got: Vec<String> = query_messages(&options)
                    .unwrap()
                    .into_iter()
                    .map(|row| row.body)
                    .collect();
                assert_eq!(got, expected, "{sort:?} {limit:?} jobs={jobs}");
            }
        }
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
        options.limit = Some(MAX_QUERY_RESULT_WINDOW);
        options.offset = 1;

        let err = run_messages_with_output(options, &default_query_output(), &mut out).unwrap_err();

        assert!(err.contains("--limit plus --offset"));
    }

    #[test]
    fn message_query_rejects_large_table_cells() {
        let dir = create_query_test_dir();
        let contains = vec!["needle".to_string()];
        let mut bytes = Vec::new();
        let mut out = output::Output::new(&mut bytes);
        let options = default_query_options(dir.path(), &contains, &[]);
        let mut query_output = default_query_output();
        query_output.max_cell_chars = MAX_TABLE_CELL_CHARS + 1;

        let err = run_messages_with_output(options, &query_output, &mut out).unwrap_err();

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
        assert!(err.contains("every message of every chat"), "{err}");

        // One chat is bound enough, as reading it with `messages` would be.
        let mut options = default_query_options(dir.path(), &[], &[]);
        options.session = Some("tgid_one");
        assert_eq!(validate_message_options(&options), Ok(()));
    }

    #[test]
    fn message_query_opens_database_read_only() {
        let dir = create_query_test_dir();
        let store = SourceStore::from_plaintext(dir.path()).unwrap();
        let conn = store.open("message/message_0.db").unwrap();

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
