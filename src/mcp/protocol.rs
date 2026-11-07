//! JSON-RPC 2.0 message handling for MCP. Transport-independent and stateless:
//! tg keeps no per-connection state, so every request stands alone.

use serde_json::{json, Value};

use super::tools::{self, Content, ToolContext};

/// Newest first. The answer to `initialize` echoes the client's version when
/// supported, otherwise offers the newest.
pub(crate) const SUPPORTED_VERSIONS: [&str; 3] = ["2026-07-28", "2025-11-25", "2025-06-18"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

const INSTRUCTIONS: &str = "Read-only access to the owner's local tg chat history. \
Start with list_sessions or list_unread to get chat ids, then read_messages with an id. \
Use search_messages for keywords across chats and query_messages for exhaustive filters. \
Results contain private message text; quote only what the user asked for.";

/// What a tool call did, for the audit log.
pub(crate) struct CallRecord {
    pub tool: String,
    pub argument_keys: Vec<String>,
    pub rows: usize,
    pub ok: bool,
}

pub(crate) struct Handled {
    /// None for notifications and responses, which get no reply.
    pub response: Option<Value>,
    pub call: Option<CallRecord>,
}

pub(crate) fn handle_bytes(ctx: &ToolContext, bytes: &[u8]) -> Handled {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(message) => handle(ctx, &message),
        Err(error) => Handled {
            response: Some(error_response(
                Value::Null,
                PARSE_ERROR,
                &format!("Parse error: {error}"),
            )),
            call: None,
        },
    }
}

pub(crate) fn handle(ctx: &ToolContext, message: &Value) -> Handled {
    let no_reply = Handled {
        response: None,
        call: None,
    };
    let Some(object) = message.as_object() else {
        // JSON-RPC batches were removed from MCP in 2025-06-18.
        return reply(error_response(
            Value::Null,
            INVALID_REQUEST,
            "Expected a single JSON-RPC object",
        ));
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return reply(error_response(
            object.get("id").cloned().unwrap_or(Value::Null),
            INVALID_REQUEST,
            "jsonrpc must be \"2.0\"",
        ));
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        // A response to something we never send; ignore it.
        return no_reply;
    };
    let Some(id) = object.get("id").cloned() else {
        // Notifications (initialized, cancelled, ...) need no reply.
        return no_reply;
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => reply(success(id, initialize(&params))),
        "ping" => reply(success(id, json!({}))),
        "tools/list" => reply(success(id, json!({ "tools": tools::definitions() }))),
        "tools/call" => call_tool(ctx, id, &params),
        _ => reply(error_response(
            id,
            METHOD_NOT_FOUND,
            &format!("Method not found: {method}"),
        )),
    }
}

fn reply(response: Value) -> Handled {
    Handled {
        response: Some(response),
        call: None,
    }
}

pub(crate) fn negotiate_version(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|version| SUPPORTED_VERSIONS.iter().find(|v| **v == version))
        .copied()
        .unwrap_or(SUPPORTED_VERSIONS[0])
}

fn initialize(params: &Value) -> Value {
    let version = negotiate_version(params.get("protocolVersion").and_then(Value::as_str));
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "tg",
            "title": "tg chat history",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

fn call_tool(ctx: &ToolContext, id: Value, params: &Value) -> Handled {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return reply(error_response(
            id,
            INVALID_PARAMS,
            "tools/call needs a name",
        ));
    };
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    let argument_keys = arguments
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    let (result, rows, ok) = match tools::call(ctx, name, &arguments) {
        Ok(outcome) => {
            let result = match outcome.content {
                Content::Json(value) => json!({
                    "content": [{ "type": "text", "text": value.to_string() }],
                    "structuredContent": value,
                    "isError": false,
                }),
                Content::Text(text) => json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": false,
                }),
            };
            (result, outcome.rows, true)
        }
        // Tool failures are results, not protocol errors, so the model can read
        // them and correct its arguments.
        Err(error) => (
            json!({
                "content": [{ "type": "text", "text": error }],
                "isError": true,
            }),
            0,
            false,
        ),
    };
    Handled {
        response: Some(success(id, result)),
        call: Some(CallRecord {
            tool: name.to_string(),
            argument_keys,
            rows,
            ok,
        }),
    }
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::config::RemotePolicy;
    use std::path::PathBuf;

    fn ctx() -> ToolContext {
        ToolContext {
            db_dir: None,
            keys: PathBuf::from("/nonexistent"),
            jobs: 1,
            policy: RemotePolicy::default(),
            remote: false,
        }
    }

    fn respond(message: Value) -> Option<Value> {
        handle(&ctx(), &message).response
    }

    #[test]
    fn initialize_negotiates_protocol_version() {
        let response = respond(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}
        }))
        .unwrap();
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(response["result"]["serverInfo"]["name"], "tg");

        let response = respond(json!({
            "jsonrpc": "2.0", "id": 2, "method": "initialize",
            "params": {"protocolVersion": "1999-01-01"}
        }))
        .unwrap();
        assert_eq!(response["result"]["protocolVersion"], SUPPORTED_VERSIONS[0]);
    }

    #[test]
    fn notifications_and_responses_get_no_reply() {
        assert!(
            respond(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none()
        );
        assert!(respond(json!({"jsonrpc": "2.0", "id": 5, "result": {}})).is_none());
    }

    #[test]
    fn malformed_messages_get_json_rpc_errors() {
        let handled = handle_bytes(&ctx(), b"{not json");
        assert_eq!(handled.response.unwrap()["error"]["code"], PARSE_ERROR);
        let batch = respond(json!([{"jsonrpc": "2.0", "id": 1, "method": "ping"}])).unwrap();
        assert_eq!(batch["error"]["code"], INVALID_REQUEST);
        let unknown =
            respond(json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"})).unwrap();
        assert_eq!(unknown["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn tool_failures_are_reported_as_error_results() {
        let handled = handle(
            &ctx(),
            &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
                    "params": {"name": "list_sessions", "arguments": {"bogus": 1}}}),
        );
        let response = handled.response.unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert_eq!(
            response["result"]["content"][0]["text"],
            "Unknown argument: bogus"
        );
        let call = handled.call.unwrap();
        assert_eq!(call.tool, "list_sessions");
        assert_eq!(call.argument_keys, ["bogus"]);
        assert!(!call.ok);
    }

    #[test]
    fn tools_list_returns_definitions() {
        let response =
            respond(json!({"jsonrpc": "2.0", "id": "a", "method": "tools/list"})).unwrap();
        assert_eq!(response["id"], "a");
        assert_eq!(response["result"]["tools"].as_array().unwrap().len(), 7);
    }
}
