//! MCP tool definitions and calls. Every tool is read-only.
//!
//! Arguments are validated here, at the protocol boundary; the domain modules
//! receive typed options. In remote mode the owner's policy (session allow/deny
//! lists, lookback window, contact-remark hiding) is applied to every result.

use std::collections::HashSet;
use std::path::PathBuf;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use clap::ValueEnum;
use serde_json::{json, Map, Value};

use super::config::RemotePolicy;
use crate::contact::DisplayNameMode;
use crate::media_fetch::{self, MediaCache, MediaSize};
use crate::source_store::SourceStore;
use crate::{db, dictionary, doctor, media, query, time};

const DAY_SECONDS: i64 = 86_400;
/// Images per get_media call; each one costs the model about as much as a
/// page of text.
const MAX_MEDIA_PER_CALL: usize = 10;

const SENDER_HINT: &str = "Only messages from one sender: `me` for the account owner, an account id \
     (the ids that list_members and the message tools show), or a name exactly as those tools show it. \
     A name several accounts share is rejected with their ids.";

pub(crate) struct ToolContext {
    pub db_dir: Option<PathBuf>,
    pub keys: PathBuf,
    pub jobs: usize,
    /// Size caps apply in every mode; session and lookback rules only when
    /// `remote` is set.
    pub policy: RemotePolicy,
    pub remote: bool,
}

pub(crate) struct CallOutcome {
    pub content: Content,
    /// Item count for the audit log.
    pub rows: usize,
}

pub(crate) enum Content {
    /// Sent as `structuredContent` plus the same JSON as text.
    Json(Value),
    /// Sent as text only. Used where JSON field names would outweigh the data
    /// itself; a single copy keeps clients from feeding the model both forms.
    Text(String),
    /// Raw MCP content blocks, for results that carry images.
    Blocks(Vec<Value>),
}

/// A JSON result while it is being trimmed to the size cap.
struct JsonPage {
    value: Value,
    rows: usize,
}

impl From<JsonPage> for CallOutcome {
    fn from(page: JsonPage) -> Self {
        Self {
            content: Content::Json(page.value),
            rows: page.rows,
        }
    }
}

impl ToolContext {
    fn name_mode(&self) -> DisplayNameMode {
        if self.remote && !self.policy.show_contact_remarks {
            DisplayNameMode::Anonymous
        } else {
            DisplayNameMode::PersonalRemark
        }
    }

    fn session_visible(&self, username: &str) -> bool {
        if !self.remote {
            return true;
        }
        let policy = &self.policy;
        (policy.allow_sessions.is_empty() || policy.allow_sessions.iter().any(|s| s == username))
            && !policy.deny_sessions.iter().any(|s| s == username)
    }

    fn lookback_floor(&self) -> Option<i64> {
        self.remote
            .then(|| time::now_timestamp() - self.policy.max_lookback_days * DAY_SECONDS)
    }

    /// Raise `since` to the lookback floor in remote mode.
    fn clamp_since(&self, since: Option<i64>) -> Option<i64> {
        match (since, self.lookback_floor()) {
            (Some(since), Some(floor)) => Some(since.max(floor)),
            (None, floor) => floor,
            (since, None) => since,
        }
    }

    fn text(&self, value: &str) -> String {
        let Some(max) = self.policy.max_text_chars else {
            return value.to_string();
        };
        if value.chars().count() <= max {
            return value.to_string();
        }
        let mut text: String = value.chars().take(max).collect();
        text.push_str("…[truncated]");
        text
    }

    fn open_store(&self) -> Result<SourceStore, String> {
        SourceStore::discover(self.db_dir.as_deref(), &self.keys)
    }
}

pub(crate) fn definitions() -> Value {
    let read_only = json!({
        "readOnlyHint": true,
        "destructiveHint": false,
        "idempotentHint": true,
        "openWorldHint": false
    });
    let time_hint = "ISO 8601 date/time or relative (30min, 2h, 7d, today, yesterday)";
    let tools = vec![
        json!({
            "name": "list_sessions",
            "title": "List chats",
            "description": dictionary::render_app_names("Use this first when the user names a person, group, or chat, or asks about a conversation or relationship with someone, in their Telegram history: it turns the name into the chat `id` that the other tools take as `session`. Lists every direct and group chat, most recently active first. Returns each chat's stable `id`, which other tools accept as `session`. Only if the list exceeds the result size cap is it trimmed, with `truncated: true` and `next_offset`."),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Optional name filter (fuzzy match on names and ids)"},
                    "offset": {"type": "integer", "minimum": 0, "default": 0, "description": "Only to continue a truncated result: pass its next_offset."}
                },
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "list_unread",
            "title": "List unread chats",
            "description": "Use this when the user asks what is new, unread, or waiting for a reply. Lists every chat that has unread messages, with unread counts. Only if the list exceeds the result size cap is it trimmed, with `truncated: true` and `next_offset`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Optional name filter"},
                    "offset": {"type": "integer", "minimum": 0, "default": 0, "description": "Only to continue a truncated result: pass its next_offset."}
                },
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "read_messages",
            "title": "Read a chat",
            "description": "Use this to see what was actually said in one chat: summarize what a group has been discussing, recall a conversation with someone, or read the context around a search hit. Reads every message of one chat in an optional [since, until) time range. Returns a plain-text transcript in chronological order: a `== YYYY-MM-DD ==` line per local day, then one `HH:MM sender: text` line per message, with indented continuation lines. A senders list after the header maps each shown name to its stable account id; the account owner always appears as 我. The header also says when the chat's readable history starts: nothing earlier is available, so an empty early range means missing history, not silence. Non-text messages appear as bracketed placeholders; images show as [img:<id>], which get_media can display. There is no page size: one call returns the whole range. Only if the range exceeds the result size cap is the transcript trimmed and ends with a `[truncated]` line giving the offset to repeat the same call with. order=newest (default) keeps the latest messages when trimming, order=oldest the earliest.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "Chat id from list_sessions (preferred) or a display name"},
                    "order": {"type": "string", "enum": ["newest", "oldest"], "default": "newest"},
                    "offset": {"type": "integer", "minimum": 0, "default": 0, "description": "Only to continue a truncated result: pass its next_offset. Counted from the end of the range for order=newest and from the start for order=oldest."},
                    "since": {"type": "string", "description": format!("Inclusive start; {time_hint}")},
                    "until": {"type": "string", "description": format!("Exclusive end; {time_hint}")},
                    "contains": {"type": "string", "description": "Only messages whose text contains this substring"},
                    "sender": {"type": "string", "description": SENDER_HINT}
                },
                "required": ["session"],
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "search_messages",
            "title": "Search messages",
            "description": dictionary::render_app_names("Use this when answering needs what people previously said or discussed about a topic, across private and group Telegram chats, including indirect questions that never mention Telegram or chat history (why someone decided something, who talked about a subject, what was said about a plan). Fast keyword search on the client's own index, over the full history unless `since` is given; it can miss some card and notice text, so use query_messages when completeness matters. Returns every match, newest first. Only if the matches exceed the result size cap is the result trimmed, with `truncated: true` and `next_offset`; then repeat the same call with offset=next_offset."),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "minLength": 1},
                    "since": {"type": "string", "description": time_hint},
                    "offset": {"type": "integer", "minimum": 0, "default": 0, "description": "Only to continue a truncated result: pass its next_offset."}
                },
                "required": ["query"],
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "query_messages",
            "title": "Filter messages",
            "description": "Use this for exhaustive or precise lookups: everything one person said (`sender`), messages with a media type, keyword combinations with exclusions, or counting messages in a period. Structured message filter across all chats or one chat: keywords to include/exclude, media type, sender, and a time range. Each message carries `sender` (the account owner is 我) and its stable `sender_id`. Slower than search_messages but exhaustive. Covers the full history unless `since` is given; with no session, give at least one filter or `since`. Returns every match. Only if the matches exceed the result size cap is the result trimmed, with `truncated: true` and `next_offset`; then repeat the same call with offset=next_offset. To read every message of one chat in a period, use read_messages instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "Optional chat id or display name"},
                    "contains": {"type": "array", "items": {"type": "string"}, "maxItems": 16},
                    "not_contains": {"type": "array", "items": {"type": "string"}, "maxItems": 16},
                    "match": {"type": "string", "enum": ["all", "any"], "default": "all"},
                    "has": {"type": "array", "items": {"type": "string", "enum": ["voice", "image", "sticker", "file", "video"]}},
                    "sender": {"type": "string", "description": SENDER_HINT},
                    "since": {"type": "string", "description": time_hint},
                    "until": {"type": "string", "description": time_hint},
                    "order": {"type": "string", "enum": ["newest", "oldest"], "default": "newest"},
                    "offset": {"type": "integer", "minimum": 0, "default": 0, "description": "Only to continue a truncated result: pass its next_offset."}
                },
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "list_members",
            "title": "List group members",
            "description": "Use this for questions about who is in a group or how active its members are: who speaks most, who has gone quiet, or which members are your contacts. Lists a group's current members with each one's id, message count, and last message time, or only members who are your local contacts (friends_only).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "Group id or display name"},
                    "inactive_since": {"type": "string", "description": format!("Only members with no recorded messages since this time; {time_hint}")},
                    "friends_only": {"type": "boolean", "default": false}
                },
                "required": ["session"],
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "get_media",
            "title": "View cached images",
            "description": format!("Use this when an image in a transcript matters to the answer. Views images from one chat. Pass the 32-character ids that read_messages and the other message tools show as [img:<id>] (images) or as the name in [视频 WxH <id>] (video cover). Returns each image the local client has cached, up to {MAX_MEDIA_PER_CALL} per call; an id that was never downloaded reports so instead. size=preview (default) returns the client's standard copy, size=original the full-resolution copy when one was downloaded. Each image costs about as much context as a page of text, so fetch only the ones the task needs."),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "Chat id or display name the images were sent in"},
                    "ids": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_MEDIA_PER_CALL},
                    "size": {"type": "string", "enum": ["preview", "original"], "default": "preview"}
                },
                "required": ["session", "ids"],
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "diagnose",
            "title": "Diagnose tg",
            "description": "Check whether tg can read the local history, optionally for one chat. Use when another tool fails.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": {"type": "string"}
                },
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
    ];
    Value::Array(tools)
}

pub(crate) fn call(
    ctx: &ToolContext,
    name: &str,
    arguments: &Value,
) -> Result<CallOutcome, String> {
    let empty = Map::new();
    let map = match arguments {
        Value::Object(map) => map,
        Value::Null => &empty,
        _ => return Err("arguments must be an object".to_string()),
    };
    let args = Args::new(map);
    match name {
        "list_sessions" => list_sessions(ctx, args, db::SessionListKind::All),
        "list_unread" => list_sessions(ctx, args, db::SessionListKind::Unread),
        "read_messages" => read_messages(ctx, args),
        "search_messages" => search_messages(ctx, args),
        "query_messages" => query_messages(ctx, args),
        "list_members" => list_members(ctx, args),
        "get_media" => get_media(ctx, args),
        "diagnose" => diagnose(ctx, args),
        _ => Err(format!("Unknown tool: {name}")),
    }
}

fn list_sessions(
    ctx: &ToolContext,
    args: Args<'_>,
    kind: db::SessionListKind,
) -> Result<CallOutcome, String> {
    args.only(&["query", "offset"])?;
    let query = args.str("query")?;
    let offset = args.offset()?;
    let store = ctx.open_store()?;
    let list = match kind {
        db::SessionListKind::All => db::list_sessions(&store, usize::MAX, query),
        db::SessionListKind::Unread => db::list_unread_sessions(&store, usize::MAX, query),
    }?;
    let sessions: Vec<Value> = list
        .sessions
        .iter()
        .filter(|session| ctx.session_visible(&session.username))
        .skip(offset)
        .map(|session| {
            json!({
                "id": session.username,
                "name": session.display_name,
                "unread": session.unread_count,
                "last_active": time_value(session.last_active),
            })
        })
        .collect();
    let value = json!({ "sessions": sessions });
    Ok(continue_after_trim(fit(ctx, value, "sessions"), offset).into())
}

fn read_messages(ctx: &ToolContext, args: Args<'_>) -> Result<CallOutcome, String> {
    args.only(&[
        "session", "order", "offset", "since", "until", "contains", "sender", "all_time",
    ])?;
    let session = args.required_str("session")?;
    let order =
        db::MessageOrder::from_str(args.choice("order", &["newest", "oldest"], "newest")?, true)?;
    let offset = args.offset()?;
    let since = ctx.clamp_since(args.time("since")?);
    let until = args.time("until")?;
    let contains = args.str("contains")?;
    let sender = args.str("sender")?;
    args.bool("all_time")?;
    let store = ctx.open_store()?;
    let options = db::ReadMessagesOptions {
        session_query: session,
        limit: None,
        offset,
        search_query: contains,
        since,
        until,
        order,
        sender,
        time_bucket: time::MessageTimeBucket::PerMessage,
        name_mode: ctx.name_mode(),
        jobs: ctx.jobs,
    };
    let page = db::read_messages(&store, &options)?;
    if !ctx.session_visible(&page.username) {
        return Err(no_session(session));
    }
    let messages: Vec<TranscriptMessage> = page
        .messages
        .iter()
        .map(|message| {
            let (date, minute) = time::format_local_date_and_minute(message.create_time);
            TranscriptMessage {
                date,
                block: transcript_block(&minute, &message.sender, &ctx.text(&message.content)),
                sender: sender_legend_line(&message.sender, &message.sender_id),
            }
        })
        .collect();
    let mut header = format!("Chat: {} (id: {})\n", page.display_name, page.username);
    // The lookback floor also hides when older history begins.
    let readable_start = page
        .history_start
        .map(|start| ctx.lookback_floor().map_or(start, |floor| start.max(floor)));
    if let Some(start) = readable_start {
        header.push_str(&format!(
            "Readable history starts {}; nothing earlier is available.\n",
            time::format_local_timestamp(start)
        ));
    }
    header.push_str(
        "{count} messages in time order. Times are local; \
         an indented line continues the message above it.\n",
    );
    let transcript = fit_transcript(
        &header,
        &messages,
        order,
        ctx.policy.max_result_bytes,
        offset,
    );
    Ok(CallOutcome {
        content: Content::Text(transcript.text),
        rows: transcript.rows,
    })
}

struct TranscriptMessage {
    date: String,
    /// `HH:MM sender: text` plus indented continuation lines, newline-terminated.
    block: String,
    /// The sender's line in the senders list; empty for system messages.
    sender: String,
}

const SENDER_LEGEND_TITLE: &str = "Senders (name = id):\n";

fn sender_legend_line(name: &str, id: &str) -> String {
    if id.is_empty() {
        String::new()
    } else {
        format!("  {name} = {id}\n")
    }
}

/// One message as transcript lines. Continuation lines are indented so they
/// can never be mistaken for a new message.
fn transcript_block(minute: &str, sender: &str, text: &str) -> String {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("");
    let mut block = if sender.is_empty() {
        format!("{minute} {first}\n")
    } else {
        format!("{minute} {sender}: {first}\n")
    };
    for line in lines {
        block.push_str("  ");
        block.push_str(line);
        block.push('\n');
    }
    block
}

fn day_header(date: &str) -> String {
    format!("== {date} ==\n")
}

struct Transcript {
    text: String,
    rows: usize,
}

/// Render `messages` (chronological) under the byte cap. When they do not all
/// fit, keep those nearest the paging origin (the end `order` puts first), so
/// `offset + rows` continues exactly where the kept ones end. `header` holds a
/// `{count}` placeholder for the number of messages kept.
fn fit_transcript(
    header: &str,
    messages: &[TranscriptMessage],
    order: db::MessageOrder,
    max_bytes: usize,
    offset: usize,
) -> Transcript {
    // Upper bounds: the count only shrinks, and the footer's offset is at most
    // `offset + messages.len()`.
    let header_bytes = header.replace("{count}", &messages.len().to_string()).len();
    let footer_bytes = truncation_footer(order, offset + messages.len()).len();
    let budget = max_bytes.saturating_sub(header_bytes);

    let from_origin: Box<dyn Iterator<Item = &TranscriptMessage>> = match order {
        db::MessageOrder::Newest => Box::new(messages.iter().rev()),
        db::MessageOrder::Oldest => Box::new(messages.iter()),
    };
    let mut used = 0;
    let mut kept = 0;
    let mut last_date: Option<&str> = None;
    let mut senders = std::collections::BTreeSet::new();
    for message in from_origin {
        let mut cost = message.block.len();
        if last_date != Some(message.date.as_str()) {
            cost += day_header(&message.date).len();
        }
        let new_sender = !message.sender.is_empty() && !senders.contains(&message.sender);
        if new_sender {
            cost += message.sender.len();
            if senders.is_empty() {
                cost += SENDER_LEGEND_TITLE.len();
            }
        }
        // Reserve room for the footer whenever something would be left out.
        let reserve = if kept + 1 < messages.len() {
            footer_bytes
        } else {
            0
        };
        if used + cost + reserve > budget {
            break;
        }
        used += cost;
        kept += 1;
        last_date = Some(&message.date);
        if new_sender {
            senders.insert(&message.sender);
        }
    }

    let range = match order {
        db::MessageOrder::Newest => messages.len() - kept..messages.len(),
        db::MessageOrder::Oldest => 0..kept,
    };
    let mut text = header.replace("{count}", &kept.to_string());
    if !senders.is_empty() {
        text.push_str(SENDER_LEGEND_TITLE);
        for line in senders {
            text.push_str(line);
        }
    }
    let mut last_date: Option<&str> = None;
    for message in &messages[range] {
        if last_date != Some(message.date.as_str()) {
            text.push_str(&day_header(&message.date));
            last_date = Some(&message.date);
        }
        text.push_str(&message.block);
    }
    if kept < messages.len() {
        text.push_str(&truncation_footer(order, offset + kept));
    }
    Transcript { text, rows: kept }
}

fn truncation_footer(order: db::MessageOrder, next_offset: usize) -> String {
    let omitted = match order {
        db::MessageOrder::Newest => "Older",
        db::MessageOrder::Oldest => "Newer",
    };
    format!(
        "[truncated] {omitted} messages were left out to fit the size cap. \
         Repeat the same call with offset={next_offset} to read them.\n"
    )
}

fn search_messages(ctx: &ToolContext, args: Args<'_>) -> Result<CallOutcome, String> {
    args.only(&["query", "since", "all_time", "offset"])?;
    let query = args.required_str("query")?;
    let offset = args.offset()?;
    let since = args.time("since")?;
    // Every tool reads the full history by default; `all_time` is only
    // type-checked for callers written against the old recent window.
    args.bool("all_time")?;
    let store = ctx.open_store()?;
    let results = db::search_messages(
        &store,
        db::SearchMessagesOptions {
            query,
            limit: None,
            since: ctx.clamp_since(since),
            name_mode: ctx.name_mode(),
            jobs: ctx.jobs,
        },
    )?;
    // Hits arrive oldest first; return newest first.
    let hits: Vec<Value> = results
        .hits
        .iter()
        .rev()
        .filter(|hit| ctx.session_visible(&hit.session))
        .skip(offset)
        .map(|hit| {
            json!({
                "time": time_value(hit.create_time),
                "session": {"id": hit.session, "name": hit.session_display},
                "type": message_kind(hit.local_type),
                "text": ctx.text(&hit.content),
            })
        })
        .collect();
    let value = json!({ "hits": hits });
    Ok(continue_after_trim(fit(ctx, value, "hits"), offset).into())
}

fn query_messages(ctx: &ToolContext, args: Args<'_>) -> Result<CallOutcome, String> {
    args.only(&[
        "session",
        "contains",
        "not_contains",
        "match",
        "has",
        "sender",
        "since",
        "until",
        "all_time",
        "order",
        "offset",
    ])?;
    let session = args.str("session")?;
    let contains = args.str_list("contains")?;
    let not_contains = args.str_list("not_contains")?;
    let match_mode =
        query::QueryMatchMode::from_str(args.choice("match", &["all", "any"], "all")?, true)?;
    let has = args
        .str_list("has")?
        .iter()
        .map(|value| query::QueryMediaType::from_str(value, true))
        .collect::<Result<Vec<_>, _>>()?;
    let sender = args.str("sender")?;
    let since = args.time("since")?;
    args.bool("all_time")?;
    let until = args.time("until")?;
    let sort =
        db::MessageOrder::from_str(args.choice("order", &["newest", "oldest"], "newest")?, true)?;
    let offset = args.offset()?;
    let store = ctx.open_store()?;
    // Offsets count only rows the caller can see, so hidden sessions are
    // filtered before skipping.
    let rows = query::query_messages(&query::QueryOptions {
        store: &store,
        session,
        contains: &contains,
        not_contains: &not_contains,
        raw_contains: &[],
        has: &has,
        sender,
        since: ctx.clamp_since(since),
        until,
        limit: None,
        offset: 0,
        sort,
        match_mode,
        name_mode: ctx.name_mode(),
        jobs: ctx.jobs,
    })?;
    let messages: Vec<Value> = rows
        .iter()
        .filter(|row| ctx.session_visible(&row.session_id))
        .skip(offset)
        .map(|row| {
            json!({
                "time": time_value(row.create_time),
                "session": {"id": row.session_id, "name": row.session},
                "sender": row.sender,
                "sender_id": row.sender_id,
                "type": message_kind(row.local_type),
                "text": ctx.text(&row.body),
            })
        })
        .collect();
    let value = json!({ "messages": messages });
    Ok(continue_after_trim(fit(ctx, value, "messages"), offset).into())
}

fn list_members(ctx: &ToolContext, args: Args<'_>) -> Result<CallOutcome, String> {
    args.only(&["session", "inactive_since", "friends_only"])?;
    let session = args.required_str("session")?;
    let friends_only = args.bool("friends_only")?;
    let inactive_since = args.time("inactive_since")?;
    if friends_only && inactive_since.is_some() {
        return Err("friends_only cannot be combined with inactive_since".to_string());
    }
    let store = ctx.open_store()?;
    if friends_only {
        let report = db::list_member_friends(&store, session, ctx.name_mode(), ctx.jobs)?;
        if !ctx.session_visible(&report.group_username) {
            return Err(no_session(session));
        }
        let friends: Vec<Value> = report
            .friends
            .iter()
            .map(|friend| json!({"id": friend.username, "name": friend.display_name}))
            .collect();
        let value = json!({
            "group": {"id": report.group_username, "name": report.group_display_name},
            "cached_members": report.cached_members,
            "friends": friends,
        });
        return Ok(fit(ctx, value, "friends").into());
    }
    let report = db::list_members(
        &store,
        db::ListMembersOptions {
            session_query: session,
            inactive_since,
            name_mode: ctx.name_mode(),
            jobs: ctx.jobs,
        },
    )?;
    if !ctx.session_visible(&report.group_username) {
        return Err(no_session(session));
    }
    let members: Vec<Value> = report
        .members
        .iter()
        .map(|member| {
            json!({
                "id": member.username,
                "name": member.display_name,
                "owner": member.is_owner,
                "last_spoke": member.last_spoke.map(time_value),
                "message_count": member.message_count,
            })
        })
        .collect();
    let value = json!({
        "group": {"id": report.group_username, "name": report.group_display_name},
        "total_members": report.total_members,
        "history": {
            "start": report.history_start.map(time_value),
            "end": report.history_end.map(time_value),
        },
        "members": members,
    });
    Ok(fit(ctx, value, "members").into())
}

fn get_media(ctx: &ToolContext, args: Args<'_>) -> Result<CallOutcome, String> {
    args.only(&["session", "ids", "size"])?;
    if ctx.remote && !ctx.policy.allow_media {
        return Err(
            "Images are disabled for remote clients. The owner can enable them \
                    with remote.allow_media in the tg MCP config."
                .to_string(),
        );
    }
    let session = args.required_str("session")?;
    let size = match args.choice("size", &["preview", "original"], "preview")? {
        "original" => MediaSize::Original,
        _ => MediaSize::Preview,
    };
    let ids = args.str_list("ids")?;
    if ids.is_empty() || ids.len() > MAX_MEDIA_PER_CALL {
        return Err(format!("ids must list 1 to {MAX_MEDIA_PER_CALL} media ids"));
    }
    // Models often copy the whole tag; the id inside it is what counts.
    let ids: Vec<&str> = ids
        .iter()
        .map(|id| id.trim().trim_start_matches("[img:").trim_end_matches(']'))
        .collect();
    if let Some(bad) = ids.iter().find(|id| !media_fetch::is_media_id(id)) {
        return Err(format!(
            "'{bad}' is not a media id; pass the 32-character id from [img:<id>]"
        ));
    }

    let store = ctx.open_store()?;
    let username = db::resolve_session(&store, session, ctx.jobs)?;
    if !ctx.session_visible(&username) {
        return Err(no_session(session));
    }
    // The cache is filed by month, so the lookback applies per whole month.
    let min_month = ctx
        .lookback_floor()
        .map(|floor| time::format_message_time_bucket(floor, time::MessageTimeBucket::Month));
    let cache = MediaCache::open(&username, min_month.as_deref())?;

    let text = |text: String| json!({"type": "text", "text": text});
    let mut budget = ctx.policy.max_result_bytes;
    let mut blocks = Vec::new();
    let mut rows = 0;
    for id in ids {
        match cache.load(id, size) {
            Ok(image) => {
                let data = BASE64.encode(&image.data);
                if data.len() > budget {
                    blocks.push(text(format!(
                        "{id}: left out to fit the result size cap; request it in its own call \
                         or with size=preview"
                    )));
                    continue;
                }
                budget -= data.len();
                blocks.push(text(format!(
                    "{id} ({}, {})",
                    image.variant,
                    media::format_file_size(image.data.len() as u64)
                )));
                blocks.push(json!({"type": "image", "data": data, "mimeType": image.mime}));
                rows += 1;
            }
            Err(error) => blocks.push(text(format!("{id}: {error}"))),
        }
    }
    Ok(CallOutcome {
        content: Content::Blocks(blocks),
        rows,
    })
}

fn diagnose(ctx: &ToolContext, args: Args<'_>) -> Result<CallOutcome, String> {
    args.only(&["session"])?;
    let session = args.str("session")?;
    let report = doctor::run(doctor::DoctorOptions {
        session,
        db_dir: ctx.db_dir.as_deref(),
        keys_path: &ctx.keys,
        jobs: ctx.jobs,
    });
    let checks: Vec<Value> = report
        .checks
        .iter()
        .map(|check| {
            // Status details carry local paths and raw errors; facts are counts.
            let detail = if ctx.remote && check.status.is_some() {
                None
            } else {
                check.detail.clone()
            };
            json!({"check": check.label, "status": check.status, "detail": detail})
        })
        .collect();
    let mut value = json!({ "checks": checks });
    if let Some(session) = &report.session {
        value["session"] = match &session.diagnosis {
            doctor::SessionDiagnosis::Resolved(probe) if ctx.session_visible(&probe.username) => {
                json!({
                    "outcome": "resolved",
                    "id": probe.username,
                    "name": probe.display_name,
                    "message_rows": probe.message_count,
                    "databases": probe.matching_dbs,
                })
            }
            doctor::SessionDiagnosis::Resolved(_) => json!({"outcome": "not_found"}),
            doctor::SessionDiagnosis::Failed { error } if !ctx.remote => {
                json!({"outcome": "failed", "error": error})
            }
            doctor::SessionDiagnosis::Failed { .. } => json!({"outcome": "not_found"}),
            doctor::SessionDiagnosis::Unavailable => json!({"outcome": "unavailable"}),
        };
    }
    let rows = report.checks.len();
    Ok(CallOutcome {
        content: Content::Json(value),
        rows,
    })
}

/// Same text for a hidden session as for a missing one, so a remote caller
/// cannot tell that a denied session exists.
fn no_session(query: &str) -> String {
    format!("No chat matched '{query}'. Use list_sessions to find chat ids.")
}

fn time_value(timestamp: i64) -> Value {
    json!({ "unix": timestamp, "local": time::format_local_timestamp(timestamp) })
}

fn message_kind(local_type: i64) -> &'static str {
    match local_type & 0xFFFF_FFFF {
        1 => "text",
        3 => "image",
        34 => "voice",
        42 => "contact_card",
        43 => "video",
        47 => "sticker",
        48 => "location",
        49 => "app",
        50 => "call",
        10000 | 10002 => "system",
        _ => "other",
    }
}

/// Drop items from the end of `key` until the serialized result fits the byte
/// cap, and mark the result `truncated` if anything was dropped.
fn fit(ctx: &ToolContext, mut value: Value, key: &str) -> JsonPage {
    let max = ctx.policy.max_result_bytes;
    let size = |value: &Value| serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len());
    let mut dropped = 0;
    while size(&value) > max {
        let Some(items) = value.get_mut(key).and_then(Value::as_array_mut) else {
            break;
        };
        if items.is_empty() {
            break;
        }
        // Remove roughly the overflow proportion at once to stay linear.
        let total = items.len();
        let over = size(&Value::Array(items.clone()))
            .saturating_sub(max)
            .max(1);
        let per_item = (size(&Value::Array(items.clone())) / total).max(1);
        let remove = over.div_ceil(per_item).clamp(1, total);
        items.truncate(total - remove);
        dropped += remove;
    }
    if dropped > 0 {
        value["truncated"] = json!(true);
    }
    let rows = value.get(key).and_then(Value::as_array).map_or(0, Vec::len);
    JsonPage { value, rows }
}

/// A trimmed result continues right after the items it kept; a whole result
/// carries no continuation.
fn continue_after_trim(mut outcome: JsonPage, offset: usize) -> JsonPage {
    if outcome.value.get("truncated").is_some() {
        outcome.value["next_offset"] = json!(offset + outcome.rows);
    }
    outcome
}

/// Typed accessors over a tool's argument object.
struct Args<'a> {
    map: &'a Map<String, Value>,
}

impl<'a> Args<'a> {
    fn new(map: &'a Map<String, Value>) -> Self {
        Self { map }
    }

    fn only(&self, allowed: &[&str]) -> Result<(), String> {
        let allowed: HashSet<&str> = allowed.iter().copied().collect();
        match self.map.keys().find(|key| !allowed.contains(key.as_str())) {
            Some(key) => Err(format!("Unknown argument: {key}")),
            None => Ok(()),
        }
    }

    fn str(&self, name: &str) -> Result<Option<&'a str>, String> {
        match self.map.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) if value.trim().is_empty() => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.as_str())),
            Some(_) => Err(format!("{name} must be a string")),
        }
    }

    fn required_str(&self, name: &str) -> Result<&'a str, String> {
        self.str(name)?.ok_or_else(|| format!("{name} is required"))
    }

    fn str_list(&self, name: &str) -> Result<Vec<String>, String> {
        match self.map.get(name) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| format!("{name} must be a list of strings"))
                })
                .collect(),
            Some(_) => Err(format!("{name} must be a list of strings")),
        }
    }

    /// Continuation offset from a previous truncated result; 0 when absent.
    fn offset(&self) -> Result<usize, String> {
        match self.map.get("offset") {
            None | Some(Value::Null) => Ok(0),
            Some(value) => value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| "offset must be a non-negative integer".to_string()),
        }
    }

    fn bool(&self, name: &str) -> Result<bool, String> {
        match self.map.get(name) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(value)) => Ok(*value),
            Some(_) => Err(format!("{name} must be a boolean")),
        }
    }

    fn choice(
        &self,
        name: &str,
        allowed: &[&'static str],
        default: &'static str,
    ) -> Result<&'static str, String> {
        match self.str(name)? {
            None => Ok(default),
            Some(value) => allowed
                .iter()
                .find(|choice| **choice == value)
                .copied()
                .ok_or_else(|| format!("{name} must be one of: {}", allowed.join(", "))),
        }
    }

    fn time(&self, name: &str) -> Result<Option<i64>, String> {
        time::parse_since_opt(self.str(name)?).map_err(|error| format!("Invalid {name}: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(remote: bool) -> ToolContext {
        ToolContext {
            db_dir: None,
            keys: PathBuf::from("/nonexistent"),
            jobs: 1,
            policy: RemotePolicy::default(),
            remote,
        }
    }

    #[test]
    fn every_tool_is_read_only_and_closed_world() {
        let tools = definitions();
        let tools = tools.as_array().unwrap();
        assert_eq!(tools.len(), 8);
        for tool in tools {
            assert_eq!(
                tool["annotations"]["readOnlyHint"], true,
                "{}",
                tool["name"]
            );
            assert_eq!(tool["annotations"]["destructiveHint"], false);
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        }
    }

    #[test]
    fn unknown_and_mistyped_arguments_are_rejected() {
        let ctx = ctx(false);
        let error = call(&ctx, "list_sessions", &json!({"limt": 3}))
            .err()
            .unwrap();
        assert_eq!(error, "Unknown argument: limt");
        // No tool takes a page size; continuation is offset-only.
        for (tool, args) in [
            ("list_sessions", json!({"limit": 10})),
            ("list_unread", json!({"limit": 10})),
            ("read_messages", json!({"session": "a", "limit": 10})),
            ("search_messages", json!({"query": "a", "limit": 10})),
            ("query_messages", json!({"limit": 10})),
        ] {
            let error = call(&ctx, tool, &args).err().unwrap();
            assert_eq!(error, "Unknown argument: limit", "{tool}");
        }
        assert!(!definitions().to_string().contains("\"limit\""));
        assert!(call(&ctx, "drop_tables", &json!({})).is_err());
    }

    #[test]
    fn remote_policy_hides_denied_sessions_and_clamps_lookback() {
        let mut ctx = ctx(true);
        ctx.policy.deny_sessions = vec!["secret".into()];
        assert!(!ctx.session_visible("secret"));
        assert!(ctx.session_visible("other"));
        ctx.policy.allow_sessions = vec!["only".into()];
        assert!(!ctx.session_visible("other"));
        assert!(ctx.session_visible("only"));

        let floor = ctx.lookback_floor().unwrap();
        assert_eq!(ctx.clamp_since(None), Some(floor));
        assert_eq!(ctx.clamp_since(Some(0)), Some(floor));
        assert_eq!(ctx.clamp_since(Some(floor + 10)), Some(floor + 10));
        assert_eq!(ctx.name_mode(), DisplayNameMode::Anonymous);

        let local = super::tests::ctx(false);
        assert_eq!(local.clamp_since(None), None);
        assert!(local.session_visible("secret"));
    }

    #[test]
    fn oversized_results_are_trimmed_from_the_end() {
        let mut ctx = ctx(false);
        ctx.policy.max_result_bytes = 2048;
        let items: Vec<Value> = (0..200)
            .map(|i| json!({"n": i, "pad": "x".repeat(40)}))
            .collect();
        let outcome = fit(&ctx, json!({ "items": items }), "items");
        assert!(serde_json::to_vec(&outcome.value).unwrap().len() <= 2048);
        assert_eq!(outcome.value["truncated"], true);
        let kept = outcome.value["items"].as_array().unwrap();
        assert_eq!(kept[0]["n"], 0);
        assert_eq!(outcome.rows, kept.len());
    }

    #[test]
    fn trimmed_results_continue_after_the_kept_items() {
        let mut ctx = ctx(false);
        ctx.policy.max_result_bytes = 1024;
        let items: Vec<Value> = (0..100)
            .map(|n| json!({"n": n, "pad": "x".repeat(40)}))
            .collect();
        let outcome = continue_after_trim(fit(&ctx, json!({ "items": items }), "items"), 7);
        assert_eq!(outcome.value["truncated"], true);
        assert_eq!(outcome.value["next_offset"], 7 + outcome.rows);

        let whole = continue_after_trim(fit(&ctx, json!({ "items": [1, 2] }), "items"), 7);
        assert!(whole.value.get("next_offset").is_none());
    }

    fn message(date: &str, minute: &str, sender: &str, text: &str) -> TranscriptMessage {
        TranscriptMessage {
            date: date.to_string(),
            block: transcript_block(minute, sender, text),
            sender: if sender.is_empty() {
                String::new()
            } else {
                sender_legend_line(sender, &format!("id_{sender}"))
            },
        }
    }

    #[test]
    fn transcript_prints_each_day_once_and_indents_continuations() {
        let messages = [
            message("2026-11-06", "23:59", "张三", "晚安"),
            message("2026-11-07", "08:00", "李四", "早\n第二行"),
            message("2026-11-07", "08:01", "", "[系统消息]"),
        ];
        let transcript = fit_transcript(
            "{count} msgs\n",
            &messages,
            db::MessageOrder::Newest,
            usize::MAX,
            0,
        );
        assert_eq!(
            transcript.text,
            "3 msgs\nSenders (name = id):\n  张三 = id_张三\n  李四 = id_李四\n\
             == 2026-11-06 ==\n23:59 张三: 晚安\n\
             == 2026-11-07 ==\n08:00 李四: 早\n  第二行\n08:01 [系统消息]\n"
        );
        assert_eq!(transcript.rows, 3);
    }

    #[test]
    fn oversized_transcripts_keep_the_paging_end_and_point_to_the_rest() {
        let messages: Vec<TranscriptMessage> = (0..100)
            .map(|n| message("2026-11-07", "12:00", "a", &format!("{n:03}")))
            .collect();
        let max = 600;

        let newest = fit_transcript("{count}\n", &messages, db::MessageOrder::Newest, max, 5);
        assert!(newest.text.len() <= max);
        assert!(newest.rows > 0 && newest.rows < 100);
        assert!(newest.text.contains("12:00 a: 099\n"));
        assert!(!newest.text.contains(": 000\n"));
        assert!(newest.text.starts_with(&format!("{}\n", newest.rows)));
        assert!(newest.text.ends_with(&format!(
            "Repeat the same call with offset={} to read them.\n",
            5 + newest.rows
        )));

        let oldest = fit_transcript("{count}\n", &messages, db::MessageOrder::Oldest, max, 0);
        assert!(oldest.text.len() <= max);
        assert!(oldest.text.contains("12:00 a: 000\n"));
        assert!(oldest.text.contains("Newer messages"));
    }

    #[test]
    fn get_media_validates_ids_before_touching_the_cache() {
        let ctx = ctx(false);
        let call_ids = |ids: Value| {
            call(
                &ctx,
                "get_media",
                &json!({"session": "room@chatroom", "ids": ids}),
            )
            .err()
            .unwrap()
        };
        assert!(call_ids(json!([])).contains("1 to 10"));
        assert!(call_ids(json!(vec!["0".repeat(32); 11])).contains("1 to 10"));
        assert!(call_ids(json!(["../../etc/passwd"])).contains("not a media id"));
        // A copied tag is accepted; the store lookup is what fails here.
        let error = call_ids(json!([format!("[img:{}]", "a".repeat(32))]));
        assert!(!error.contains("not a media id"), "{error}");
    }

    #[test]
    fn remote_media_is_off_unless_the_owner_allows_it() {
        let mut ctx = ctx(true);
        let args = json!({"session": "room@chatroom", "ids": ["a".repeat(32)]});
        let error = call(&ctx, "get_media", &args).err().unwrap();
        assert!(error.contains("remote.allow_media"), "{error}");
        ctx.policy.allow_media = true;
        let error = call(&ctx, "get_media", &args).err().unwrap();
        assert!(!error.contains("remote.allow_media"), "{error}");
    }

    #[test]
    fn long_text_is_capped_by_characters() {
        let mut ctx = ctx(false);
        let long = "x".repeat(10_000);
        assert_eq!(ctx.text(&long), long);
        ctx.policy.max_text_chars = Some(3);
        assert_eq!(ctx.text("你好世界"), "你好世…[truncated]");
        assert_eq!(ctx.text("abc"), "abc");
    }
}
