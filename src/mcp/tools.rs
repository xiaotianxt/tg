//! MCP tool definitions and calls. Every tool is read-only.
//!
//! Arguments are validated here, at the protocol boundary; the domain modules
//! receive typed options. In remote mode the owner's policy (session allow/deny
//! lists, lookback window, contact-remark hiding) is applied to every result.

use std::collections::HashSet;
use std::path::PathBuf;

use clap::ValueEnum;
use serde_json::{json, Map, Value};

use super::config::RemotePolicy;
use crate::contact::DisplayNameMode;
use crate::source_store::SourceStore;
use crate::{db, doctor, query, time};

const DAY_SECONDS: i64 = 86_400;

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
    pub value: Value,
    /// Item count for the audit log.
    pub rows: usize,
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

    fn text(&self, value: &str) -> Value {
        let Some(max) = self.policy.max_text_chars else {
            return Value::String(value.to_string());
        };
        if value.chars().count() <= max {
            return Value::String(value.to_string());
        }
        let mut text: String = value.chars().take(max).collect();
        text.push_str("…[truncated]");
        Value::String(text)
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
            "description": "List every chat (direct and group) from the local tg history, most recently active first. Returns each chat's stable `id`, which other tools accept as `session`. Only if the list exceeds the result size cap is it trimmed, with `truncated: true` and `next_offset`.",
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
            "description": "List every chat that has unread messages, with unread counts. Only if the list exceeds the result size cap is it trimmed, with `truncated: true` and `next_offset`.",
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
            "description": "Read every message of one chat in an optional [since, until) time range, in chronological order. There is no page size: one call returns the whole range. Only if the range exceeds the result size cap is the result trimmed, with `truncated: true` and `next_offset`; then repeat the same call with offset=next_offset. order=newest (default) keeps the latest messages when trimming, order=oldest the earliest.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "Chat id from list_sessions (preferred) or a display name"},
                    "order": {"type": "string", "enum": ["newest", "oldest"], "default": "newest"},
                    "offset": {"type": "integer", "minimum": 0, "default": 0, "description": "Only to continue a truncated result: pass its next_offset. Counted from the end of the range for order=newest and from the start for order=oldest."},
                    "since": {"type": "string", "description": format!("Inclusive start; {time_hint}")},
                    "until": {"type": "string", "description": format!("Exclusive end; {time_hint}")},
                    "contains": {"type": "string", "description": "Only messages whose text contains this substring"}
                },
                "required": ["session"],
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "search_messages",
            "title": "Search messages",
            "description": "Full-text search across all chats using the client's own search index. Defaults to the recent window unless `since` or `all_time` is given. Returns every match, newest first. Only if the matches exceed the result size cap is the result trimmed, with `truncated: true` and `next_offset`; then repeat the same call with offset=next_offset.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "minLength": 1},
                    "since": {"type": "string", "description": time_hint},
                    "all_time": {"type": "boolean", "default": false},
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
            "description": "Structured message filter across all chats or one chat: keywords to include/exclude, media type, and a time range. Slower than search_messages but exhaustive. Defaults to the recent window unless `since` or `all_time` is given. Returns every match. Only if the matches exceed the result size cap is the result trimmed, with `truncated: true` and `next_offset`; then repeat the same call with offset=next_offset. To read every message of one chat in a period, use read_messages instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "Optional chat id or display name"},
                    "contains": {"type": "array", "items": {"type": "string"}, "maxItems": 16},
                    "not_contains": {"type": "array", "items": {"type": "string"}, "maxItems": 16},
                    "match": {"type": "string", "enum": ["all", "any"], "default": "all"},
                    "has": {"type": "array", "items": {"type": "string", "enum": ["voice", "image", "sticker", "file", "video"]}},
                    "since": {"type": "string", "description": time_hint},
                    "until": {"type": "string", "description": time_hint},
                    "all_time": {"type": "boolean", "default": false},
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
            "description": "List a group's current members with locally recorded speaking activity, or only members who are your local contacts (friends_only).",
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
    Ok(continue_after_trim(
        fit(ctx, value, "sessions", Trim::End),
        offset,
    ))
}

fn read_messages(ctx: &ToolContext, args: Args<'_>) -> Result<CallOutcome, String> {
    args.only(&["session", "order", "offset", "since", "until", "contains"])?;
    let session = args.required_str("session")?;
    let newest = args.choice("order", &["newest", "oldest"], "newest")? == "newest";
    let offset = args.offset()?;
    let since = ctx.clamp_since(args.time("since")?);
    let until = args.time("until")?;
    let contains = args.str("contains")?;
    let store = ctx.open_store()?;
    let options = db::ReadMessagesOptions {
        session_query: session,
        limit: None,
        offset,
        search_query: contains,
        since,
        until,
        tail: newest,
        time_bucket: time::MessageTimeBucket::PerMessage,
        name_mode: ctx.name_mode(),
        jobs: ctx.jobs,
    };
    let page = db::read_messages(&store, &options)?;
    if !ctx.session_visible(&page.username) {
        return Err(no_session(session));
    }
    let messages: Vec<Value> = page
        .messages
        .iter()
        .map(|message| {
            json!({
                "time": time_value(message.create_time),
                "sender": message.sender,
                "type": message_kind(message.local_type),
                "text": ctx.text(&message.content),
            })
        })
        .collect();
    let value = json!({
        "session": {"id": page.username, "name": page.display_name},
        "order": if newest { "newest" } else { "oldest" },
        "messages": messages,
    });
    // Trim the messages farthest from the paging origin, so next_offset
    // continues exactly where the kept ones end.
    let trim = if newest { Trim::Start } else { Trim::End };
    Ok(continue_after_trim(
        fit(ctx, value, "messages", trim),
        offset,
    ))
}

fn search_messages(ctx: &ToolContext, args: Args<'_>) -> Result<CallOutcome, String> {
    args.only(&["query", "since", "all_time", "offset"])?;
    let query = args.required_str("query")?;
    let offset = args.offset()?;
    let since = args.time("since")?;
    let since = if args.bool("all_time")? {
        since
    } else {
        Some(since.unwrap_or_else(time::default_recent_since))
    };
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
    Ok(continue_after_trim(
        fit(ctx, value, "hits", Trim::End),
        offset,
    ))
}

fn query_messages(ctx: &ToolContext, args: Args<'_>) -> Result<CallOutcome, String> {
    args.only(&[
        "session",
        "contains",
        "not_contains",
        "match",
        "has",
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
    let since = args.time("since")?;
    let since = if args.bool("all_time")? {
        since
    } else {
        Some(since.unwrap_or_else(time::default_recent_since))
    };
    let until = args.time("until")?;
    let sort =
        query::QuerySort::from_str(args.choice("order", &["newest", "oldest"], "newest")?, true)?;
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
                "type": message_kind(row.local_type),
                "text": ctx.text(&row.body),
            })
        })
        .collect();
    let value = json!({ "messages": messages });
    Ok(continue_after_trim(
        fit(ctx, value, "messages", Trim::End),
        offset,
    ))
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
        return Ok(fit(ctx, value, "friends", Trim::End));
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
    Ok(fit(ctx, value, "members", Trim::End))
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
    Ok(CallOutcome { value, rows })
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

#[derive(Clone, Copy)]
enum Trim {
    Start,
    End,
}

/// Drop items from `key` until the serialized result fits the byte cap, and
/// mark the result `truncated` if anything was dropped.
fn fit(ctx: &ToolContext, mut value: Value, key: &str, trim: Trim) -> CallOutcome {
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
        match trim {
            Trim::Start => {
                items.drain(..remove);
            }
            Trim::End => items.truncate(total - remove),
        }
        dropped += remove;
    }
    if dropped > 0 {
        value["truncated"] = json!(true);
    }
    let rows = value.get(key).and_then(Value::as_array).map_or(0, Vec::len);
    CallOutcome { value, rows }
}

/// A trimmed result continues right after the items it kept; a whole result
/// carries no continuation.
fn continue_after_trim(mut outcome: CallOutcome, offset: usize) -> CallOutcome {
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
        assert_eq!(tools.len(), 7);
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
    fn oversized_results_are_trimmed_from_the_requested_end() {
        let mut ctx = ctx(false);
        ctx.policy.max_result_bytes = 2048;
        let items: Vec<Value> = (0..200)
            .map(|i| json!({"n": i, "pad": "x".repeat(40)}))
            .collect();
        let outcome = fit(
            &ctx,
            json!({ "items": items.clone() }),
            "items",
            Trim::Start,
        );
        assert!(serde_json::to_vec(&outcome.value).unwrap().len() <= 2048);
        assert_eq!(outcome.value["truncated"], true);
        let kept = outcome.value["items"].as_array().unwrap();
        assert_eq!(
            kept.last().unwrap()["n"],
            199,
            "Start trimming keeps the newest"
        );
        assert_eq!(outcome.rows, kept.len());

        let outcome = fit(&ctx, json!({ "items": items }), "items", Trim::End);
        assert_eq!(outcome.value["items"][0]["n"], 0);
    }

    #[test]
    fn trimmed_results_continue_after_the_kept_items() {
        let mut ctx = ctx(false);
        ctx.policy.max_result_bytes = 1024;
        let items: Vec<Value> = (0..100)
            .map(|n| json!({"n": n, "pad": "x".repeat(40)}))
            .collect();
        let outcome =
            continue_after_trim(fit(&ctx, json!({ "items": items }), "items", Trim::End), 7);
        assert_eq!(outcome.value["truncated"], true);
        assert_eq!(outcome.value["next_offset"], 7 + outcome.rows);

        let whole =
            continue_after_trim(fit(&ctx, json!({ "items": [1, 2] }), "items", Trim::End), 7);
        assert!(whole.value.get("next_offset").is_none());
    }

    #[test]
    fn long_text_is_capped_by_characters() {
        let mut ctx = ctx(false);
        let long = "x".repeat(10_000);
        assert_eq!(ctx.text(&long), json!(long));
        ctx.policy.max_text_chars = Some(3);
        assert_eq!(ctx.text("你好世界"), json!("你好世…[truncated]"));
        assert_eq!(ctx.text("abc"), json!("abc"));
    }
}
