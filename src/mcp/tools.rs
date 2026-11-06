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

const MAX_SESSIONS: usize = 200;
const MAX_MESSAGES: usize = 500;
const MAX_SEARCH_HITS: usize = 100;
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
        let max = self.policy.max_text_chars;
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
            "description": "List recent chats (direct and group) from the local tg history, most recently active first. Returns each chat's stable `id`, which other tools accept as `session`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Optional name filter (fuzzy match on names and ids)"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_SESSIONS, "default": 30}
                },
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "list_unread",
            "title": "List unread chats",
            "description": "List chats that have unread messages, with unread counts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Optional name filter"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_SESSIONS, "default": 30}
                },
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "read_messages",
            "title": "Read a chat",
            "description": "Read messages from one chat in chronological order. order=newest (default) returns the latest `limit` messages; order=oldest pages forward from the start with `offset`. For messages before a given time, use query_messages with `session` and `until`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "Chat id from list_sessions (preferred) or a display name"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_MESSAGES, "default": 50},
                    "order": {"type": "string", "enum": ["newest", "oldest"], "default": "newest"},
                    "offset": {"type": "integer", "minimum": 0, "default": 0, "description": "Only with order=oldest"},
                    "since": {"type": "string", "description": time_hint},
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
            "description": "Full-text search across all chats using the client's own search index. Defaults to the recent window unless `since` or `all_time` is given. Returns the newest matches.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "minLength": 1},
                    "since": {"type": "string", "description": time_hint},
                    "all_time": {"type": "boolean", "default": false},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_SEARCH_HITS, "default": 20}
                },
                "required": ["query"],
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "query_messages",
            "title": "Filter messages",
            "description": "Structured message filter across all chats or one chat: keywords to include/exclude, media type, and a time range. Slower than search_messages but exhaustive. Defaults to the recent window unless `since` or `all_time` is given.",
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
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_MESSAGES, "default": 100},
                    "offset": {"type": "integer", "minimum": 0, "default": 0}
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
    args.only(&["query", "limit"])?;
    let query = args.str("query")?;
    let limit = args.usize("limit", 30, 1, MAX_SESSIONS)?;
    let store = ctx.open_store()?;
    // Ask for every match so hidden sessions don't shrink the page.
    let list = match kind {
        db::SessionListKind::All => db::list_sessions(&store, usize::MAX, query),
        db::SessionListKind::Unread => db::list_unread_sessions(&store, usize::MAX, query),
    }?;
    let sessions: Vec<Value> = list
        .sessions
        .iter()
        .filter(|session| ctx.session_visible(&session.username))
        .take(limit)
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
    Ok(fit(ctx, value, "sessions", Trim::End))
}

fn read_messages(ctx: &ToolContext, args: Args<'_>) -> Result<CallOutcome, String> {
    args.only(&["session", "limit", "order", "offset", "since", "contains"])?;
    let session = args.required_str("session")?;
    let limit = args.usize("limit", 50, 1, MAX_MESSAGES)?;
    let newest = args.choice("order", &["newest", "oldest"], "newest")? == "newest";
    let offset = args.usize("offset", 0, 0, usize::MAX)?;
    if newest && offset > 0 {
        return Err("offset requires order=oldest".to_string());
    }
    let since = ctx.clamp_since(args.time("since")?);
    let contains = args.str("contains")?;
    let store = ctx.open_store()?;
    let options = db::ReadMessagesOptions {
        session_query: session,
        limit: Some(limit),
        offset,
        search_query: contains,
        since,
        tail: newest,
        time_bucket: time::MessageTimeBucket::PerMessage,
        name_mode: ctx.name_mode(),
        jobs: ctx.jobs,
    };
    let page = db::read_messages(&store, &options)?;
    if !ctx.session_visible(&page.username) {
        return Err(no_session(session));
    }
    let count = page.messages.len();
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
    let mut value = json!({
        "session": {"id": page.username, "name": page.display_name},
        "order": if newest { "newest" } else { "oldest" },
        "messages": messages,
    });
    if !newest && count == limit {
        value["next_offset"] = json!(offset + count);
    }
    // Newest-first pages keep the latest messages when trimmed.
    let trim = if newest { Trim::Start } else { Trim::End };
    let mut outcome = fit(ctx, value, "messages", trim);
    if !newest {
        if let Some(kept) = outcome.value["messages"].as_array().map(Vec::len) {
            if kept < count {
                outcome.value["next_offset"] = json!(offset + kept);
            }
        }
    }
    Ok(outcome)
}

fn search_messages(ctx: &ToolContext, args: Args<'_>) -> Result<CallOutcome, String> {
    args.only(&["query", "since", "all_time", "limit"])?;
    let query = args.required_str("query")?;
    let limit = args.usize("limit", 20, 1, MAX_SEARCH_HITS)?;
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
            limit,
            since: ctx.clamp_since(since),
            name_mode: ctx.name_mode(),
        },
    )?;
    // Hits arrive oldest first; return newest first.
    let hits: Vec<Value> = results
        .hits
        .iter()
        .rev()
        .filter(|hit| ctx.session_visible(&hit.session))
        .map(|hit| {
            json!({
                "time": time_value(hit.create_time),
                "session": {"id": hit.session, "name": hit.session_display},
                "type": message_kind(hit.local_type),
                "text": ctx.text(&hit.content),
            })
        })
        .collect();
    let value = json!({ "hits": hits, "has_more": results.has_more });
    Ok(fit(ctx, value, "hits", Trim::End))
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
        "limit",
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
    let limit = args.usize("limit", 100, 1, MAX_MESSAGES)?;
    let offset = args.usize("offset", 0, 0, usize::MAX)?;
    let store = ctx.open_store()?;
    let rows = query::query_messages(&query::QueryOptions {
        store: &store,
        session,
        contains: &contains,
        not_contains: &not_contains,
        raw_contains: &[],
        has: &has,
        since: ctx.clamp_since(since),
        until,
        limit,
        offset,
        sort,
        match_mode,
        name_mode: ctx.name_mode(),
        jobs: ctx.jobs,
    })?;
    let fetched = rows.len();
    let messages: Vec<Value> = rows
        .iter()
        .filter(|row| ctx.session_visible(&row.session_id))
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
    let mut value = json!({ "messages": messages });
    if fetched == limit {
        value["next_offset"] = json!(offset + fetched);
    }
    let mut outcome = fit(ctx, value, "messages", Trim::End);
    if outcome.value.get("truncated").is_some() {
        // Trimmed rows were never seen; let the caller continue after the kept ones.
        let kept = outcome.value["messages"].as_array().map_or(0, Vec::len);
        outcome.value["next_offset"] = json!(offset + kept);
    }
    Ok(outcome)
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

    fn usize(&self, name: &str, default: usize, min: usize, max: usize) -> Result<usize, String> {
        let value = match self.map.get(name) {
            None | Some(Value::Null) => return Ok(default),
            Some(value) => value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| format!("{name} must be a non-negative integer"))?,
        };
        if value < min || value > max {
            return Err(format!("{name} must be between {min} and {max}"));
        }
        Ok(value)
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
        let error = call(&ctx, "list_sessions", &json!({"limit": 0}))
            .err()
            .unwrap();
        assert!(error.contains("between 1 and 200"), "{error}");
        let error = call(&ctx, "read_messages", &json!({"session": "a", "offset": 3}))
            .err()
            .unwrap();
        assert_eq!(error, "offset requires order=oldest");
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
    fn long_text_is_capped_by_characters() {
        let mut ctx = ctx(false);
        ctx.policy.max_text_chars = 3;
        assert_eq!(ctx.text("你好世界"), json!("你好世…[truncated]"));
        assert_eq!(ctx.text("abc"), json!("abc"));
    }
}
