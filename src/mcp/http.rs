//! Streamable HTTP transport (JSON responses only) plus the OAuth endpoints.
//!
//! TLS terminates at the public reverse proxy; the proxy-to-tg hop runs over
//! the tailnet. Requests are served one at a time: there is one owner, and the
//! heavy reads already parallelize internally.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Instant;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use super::config::Config;
use super::oauth::{AuthServer, Owner, Reply};
use super::protocol::{self, SUPPORTED_VERSIONS};
use super::tools::ToolContext;
use crate::time::now_timestamp;

const MAX_BODY_BYTES: u64 = 1024 * 1024;

pub(crate) fn serve(
    config: &Config,
    dir: &Path,
    owner: Owner,
    ctx: ToolContext,
) -> Result<(), String> {
    let server = Server::http(config.listen)
        .map_err(|error| format!("Cannot listen on {}: {error}", config.listen))?;
    log::info!(
        "tg mcp listening on http://{} for {}",
        config.listen,
        config.resource_url()
    );
    let mut app = App {
        config,
        auth: AuthServer::new(config, dir, owner),
        ctx,
        audit_path: dir.join("audit.log"),
    };
    for request in server.incoming_requests() {
        app.handle(request);
    }
    Ok(())
}

struct App<'a> {
    config: &'a Config,
    auth: AuthServer<'a>,
    ctx: ToolContext,
    audit_path: PathBuf,
}

struct Incoming<'a> {
    method: &'a Method,
    path: &'a str,
    query: &'a str,
    headers: &'a HashMap<String, String>,
    body: &'a [u8],
    client_ip: IpAddr,
    started: Instant,
}

struct HttpReply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    headers: Vec<(&'static str, String)>,
}

impl HttpReply {
    fn json(status: u16, value: &Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(value).unwrap_or_default(),
            headers: Vec::new(),
        }
    }

    fn empty(status: u16) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: Vec::new(),
            headers: Vec::new(),
        }
    }

    fn text(status: u16, text: &str) -> Self {
        Self {
            body: text.as_bytes().to_vec(),
            ..Self::empty(status)
        }
    }

    fn from_oauth(reply: Reply) -> Self {
        match reply {
            Reply::Json(status, value) => Self::json(status, &value),
            Reply::Html(status, html) => Self {
                status,
                content_type: "text/html; charset=utf-8",
                body: html.into_bytes(),
                headers: vec![
                    ("X-Frame-Options", "DENY".to_string()),
                    (
                        "Content-Security-Policy",
                        "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'"
                            .to_string(),
                    ),
                    ("Referrer-Policy", "no-referrer".to_string()),
                ],
            },
            Reply::Redirect(location) => Self {
                headers: vec![("Location", location)],
                ..Self::empty(302)
            },
        }
    }
}

impl App<'_> {
    fn handle(&mut self, mut request: Request) {
        let started = Instant::now();
        let method = request.method().clone();
        let url = request.url().to_string();
        let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
        let path = path.to_string();
        let headers = headers_of(&request);
        let client_ip = self.client_ip(&request, &headers);

        let reply = if !self.host_allowed(headers.get("host").map(String::as_str)) {
            HttpReply::text(421, "Misdirected request")
        } else {
            let mut body = Vec::new();
            let read = request
                .as_reader()
                .take(MAX_BODY_BYTES + 1)
                .read_to_end(&mut body);
            if read.is_err() || body.len() as u64 > MAX_BODY_BYTES {
                HttpReply::text(413, "Request body too large")
            } else {
                self.route(&Incoming {
                    method: &method,
                    path: &path,
                    query,
                    headers: &headers,
                    body: &body,
                    client_ip,
                    started,
                })
            }
        };
        // Never log query strings: they carry authorization codes.
        log::info!("{method} {path} -> {} ({client_ip})", reply.status);
        respond(request, reply);
    }

    fn route(&mut self, request: &Incoming<'_>) -> HttpReply {
        let Incoming {
            method,
            path,
            query,
            headers,
            body,
            ..
        } = *request;
        match (method, path) {
            (Method::Get, "/.well-known/oauth-protected-resource")
            | (Method::Get, "/.well-known/oauth-protected-resource/mcp") => {
                HttpReply::json(200, &self.auth.protected_resource_metadata())
            }
            (Method::Get, "/.well-known/oauth-authorization-server")
            | (Method::Get, "/.well-known/openid-configuration") => {
                HttpReply::json(200, &self.auth.authorization_server_metadata())
            }
            (Method::Post, "/oauth/register") => HttpReply::from_oauth(self.auth.register(body)),
            (Method::Get, "/oauth/authorize") => {
                HttpReply::from_oauth(self.auth.authorize_page(&parse_form(query)))
            }
            (Method::Post, "/oauth/authorize") => HttpReply::from_oauth(
                self.auth
                    .authorize_submit(&parse_form(&String::from_utf8_lossy(body))),
            ),
            (Method::Post, "/oauth/token") => {
                let form = parse_form(&String::from_utf8_lossy(body));
                let basic = headers
                    .get("authorization")
                    .and_then(|value| parse_basic(value));
                HttpReply::from_oauth(self.auth.token(&form, basic))
            }
            (Method::Post, "/mcp") => self.mcp(request),
            (Method::Get, "/mcp") | (Method::Delete, "/mcp") => {
                // No server-initiated streams and no sessions to end.
                let mut reply = HttpReply::text(405, "Method not allowed");
                reply.headers.push(("Allow", "POST".to_string()));
                reply
            }
            _ => HttpReply::text(404, "Not found"),
        }
    }

    fn mcp(&mut self, request: &Incoming<'_>) -> HttpReply {
        let headers = request.headers;
        let bearer = headers.get("authorization").and_then(|value| {
            value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
        });
        let client = match bearer.map(|token| self.auth.store().authenticate(token.trim())) {
            Some(Ok(client)) => client,
            Some(Err(_)) => return self.unauthorized(Some("invalid_token")),
            None => return self.unauthorized(None),
        };
        if let Some(version) = headers.get("mcp-protocol-version") {
            if !SUPPORTED_VERSIONS.contains(&version.as_str()) {
                return HttpReply::json(
                    400,
                    &json!({"error": format!("Unsupported MCP-Protocol-Version: {version}")}),
                );
            }
        }
        let handled = protocol::handle_bytes(&self.ctx, request.body);
        if let Some(call) = &handled.call {
            self.audit(&client, request.client_ip, call, request.started);
        }
        match handled.response {
            Some(response) => HttpReply::json(200, &response),
            None => HttpReply::empty(202),
        }
    }

    fn unauthorized(&self, error: Option<&str>) -> HttpReply {
        let mut challenge = format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\", scope=\"{}\"",
            self.config.public_url,
            super::oauth::SCOPE
        );
        if let Some(error) = error {
            challenge.push_str(&format!(", error=\"{error}\""));
        }
        let mut reply = HttpReply::json(401, &json!({"error": error.unwrap_or("unauthorized")}));
        reply.headers.push(("WWW-Authenticate", challenge));
        reply
    }

    /// The public Host, or the listen address for direct local checks. A DNS
    /// rebinding page would carry its own hostname, so neither admits it.
    fn host_allowed(&self, host: Option<&str>) -> bool {
        host.is_some_and(|host| {
            host.eq_ignore_ascii_case(self.config.public_host())
                || host == self.config.listen.to_string()
        })
    }

    fn client_ip(&self, request: &Request, headers: &HashMap<String, String>) -> IpAddr {
        let peer = request
            .remote_addr()
            .map(|addr| addr.ip())
            .unwrap_or(IpAddr::from([0, 0, 0, 0]));
        if Some(peer) != self.config.trusted_proxy {
            return peer;
        }
        headers
            .get("x-forwarded-for")
            .and_then(|value| value.split(',').next())
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(peer)
    }

    /// Argument names only, never values: search terms are private too.
    fn audit(&self, client: &str, ip: IpAddr, call: &protocol::CallRecord, started: Instant) {
        let line = json!({
            "ts": now_timestamp(),
            "client": client,
            "ip": ip.to_string(),
            "tool": call.tool,
            "args": call.argument_keys,
            "rows": call.rows,
            "ok": call.ok,
            "ms": started.elapsed().as_millis() as u64,
        });
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = options
            .open(&self.audit_path)
            .and_then(|mut file| writeln!(file, "{line}"));
        if let Err(error) = result {
            log::error!(
                "Cannot write audit log {}: {error}",
                self.audit_path.display()
            );
        }
    }
}

fn headers_of(request: &Request) -> HashMap<String, String> {
    request
        .headers()
        .iter()
        .map(|header| {
            (
                header.field.as_str().as_str().to_ascii_lowercase(),
                header.value.as_str().to_string(),
            )
        })
        .collect()
}

fn respond(request: Request, reply: HttpReply) {
    let mut response = Response::from_data(reply.body).with_status_code(reply.status);
    let mut headers = vec![
        ("Content-Type", reply.content_type.to_string()),
        ("Cache-Control", "no-store".to_string()),
        ("X-Content-Type-Options", "nosniff".to_string()),
    ];
    headers.extend(reply.headers);
    for (name, value) in headers {
        if let Ok(header) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            response.add_header(header);
        }
    }
    if let Err(error) = request.respond(response) {
        log::warn!("Failed to send response: {error}");
    }
}

pub(crate) fn parse_form(input: &str) -> HashMap<String, String> {
    input
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((percent_decode(key)?, percent_decode(value)?))
        })
        .collect()
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = bytes.get(index + 1..index + 3)?;
                out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
                index += 2;
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8(out).ok()
}

/// RFC 6749 §2.3.1: Basic credentials are form-encoded before base64.
fn parse_basic(value: &str) -> Option<(String, String)> {
    let encoded = value
        .strip_prefix("Basic ")
        .or_else(|| value.strip_prefix("basic "))?;
    let decoded = String::from_utf8(STANDARD.decode(encoded.trim()).ok()?).ok()?;
    let (id, secret) = decoded.split_once(':')?;
    Some((percent_decode(id)?, percent_decode(secret)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_decoding_handles_plus_and_percent() {
        let form = parse_form("a=1+2&b=%E4%BD%A0%3D&c&bad=%zz");
        assert_eq!(form["a"], "1 2");
        assert_eq!(form["b"], "你=");
        assert_eq!(form["c"], "");
        assert!(!form.contains_key("bad"));
    }

    #[test]
    fn basic_credentials_are_decoded() {
        let header = format!("Basic {}", STANDARD.encode("tgc_a%3Ab:s+3"));
        assert_eq!(parse_basic(&header), Some(("tgc_a:b".into(), "s 3".into())));
        assert_eq!(parse_basic("Bearer x"), None);
    }
}
