//! Single-user OAuth 2.1 authorization server for remote MCP clients.
//!
//! The owner proves presence with a passphrase on the consent page; clients
//! never see it. Tokens are opaque random strings stored only as SHA-256
//! hashes. Authorization codes and pending consent requests live in memory, so
//! a restart only interrupts a sign-in in progress.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::config::{self, Config};
use crate::time::now_timestamp;

pub(crate) const SCOPE: &str = "tg.read";
const ACCESS_TTL: i64 = 3600;
const REFRESH_TTL: i64 = 30 * 86_400;
const CODE_TTL: i64 = 60;
const CONSENT_TTL: i64 = 600;
const PASSPHRASE_MIN_CHARS: usize = 16;
const PBKDF2_ROUNDS: u32 = 600_000;
const MAX_CLIENTS: usize = 10;
const FAILURE_WINDOW: i64 = 15 * 60;
const MAX_FAILURES: usize = 5;

const CHATGPT_STABLE_REDIRECT: &str = "https://chatgpt.com/connector_platform_oauth_redirect";
const CHATGPT_REDIRECT_PREFIX: &str = "https://chatgpt.com/connector/oauth/";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Owner {
    salt: String,
    rounds: u32,
    hash: String,
}

impl Owner {
    pub(crate) fn new(passphrase: &str) -> Result<Self, String> {
        if passphrase.chars().count() < PASSPHRASE_MIN_CHARS {
            return Err(format!(
                "passphrase must have at least {PASSPHRASE_MIN_CHARS} characters"
            ));
        }
        let salt = random_bytes::<16>()?;
        Ok(Self {
            salt: hex::encode(salt),
            rounds: PBKDF2_ROUNDS,
            hash: hex::encode(derive(passphrase, &salt, PBKDF2_ROUNDS)),
        })
    }

    fn verify(&self, passphrase: &str) -> bool {
        let (Ok(salt), Ok(expected)) = (hex::decode(&self.salt), hex::decode(&self.hash)) else {
            return false;
        };
        constant_time_eq(&derive(passphrase, &salt, self.rounds), &expected)
    }
}

fn derive(passphrase: &str, salt: &[u8], rounds: u32) -> [u8; 32] {
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, rounds, &mut out);
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Client {
    client_id: String,
    /// Present for confidential clients (client_secret_post / _basic).
    secret_hash: Option<String>,
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    created: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TokenKind {
    Access,
    Refresh,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Token {
    hash: String,
    kind: TokenKind,
    client_id: String,
    grant_id: String,
    expires: i64,
    /// Refresh tokens are single use; a used one presented again revokes the grant.
    #[serde(default)]
    used: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    clients: Vec<Client>,
    tokens: Vec<Token>,
}

/// File-backed client and token store, re-read on every operation so that
/// `tg mcp revoke` takes effect immediately on a running server.
pub(crate) struct Store {
    path: PathBuf,
}

pub(crate) struct StoreSummary {
    pub clients: Vec<(String, Option<String>, i64)>,
    pub grants: usize,
}

impl Store {
    pub(crate) fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("oauth.json"),
        }
    }

    fn load(&self) -> Result<State, String> {
        Ok(config::read_private_json(&self.path)?.unwrap_or_default())
    }

    fn save(&self, mut state: State) -> Result<(), String> {
        let now = now_timestamp();
        state.tokens.retain(|token| token.expires > now);
        config::write_private_json(&self.path, &state)
    }

    pub(crate) fn summary(&self) -> Result<StoreSummary, String> {
        let state = self.load()?;
        let now = now_timestamp();
        let mut grants: Vec<&str> = state
            .tokens
            .iter()
            .filter(|token| token.expires > now && !token.used)
            .map(|token| token.grant_id.as_str())
            .collect();
        grants.sort_unstable();
        grants.dedup();
        Ok(StoreSummary {
            clients: state
                .clients
                .iter()
                .map(|client| {
                    (
                        client.client_id.clone(),
                        client.client_name.clone(),
                        client.created,
                    )
                })
                .collect(),
            grants: grants.len(),
        })
    }

    /// Remove tokens (and the client registration) for one client, or all.
    pub(crate) fn revoke(&self, client_id: Option<&str>) -> Result<usize, String> {
        let mut state = self.load()?;
        let before = state.clients.len();
        match client_id {
            Some(id) => {
                state.clients.retain(|client| client.client_id != id);
                state.tokens.retain(|token| token.client_id != id);
            }
            None => {
                state.clients.clear();
                state.tokens.clear();
            }
        }
        let removed = before - state.clients.len();
        self.save(state)?;
        Ok(removed)
    }

    /// Validate a bearer token; returns the client id it was issued to.
    pub(crate) fn authenticate(&self, bearer: &str) -> Result<String, String> {
        let hash = hash_token(bearer);
        let now = now_timestamp();
        self.load()?
            .tokens
            .into_iter()
            .find(|token| {
                token.kind == TokenKind::Access && token.hash == hash && token.expires > now
            })
            .map(|token| token.client_id)
            .ok_or_else(|| "invalid or expired access token".to_string())
    }
}

/// An HTTP-independent OAuth response; the transport renders it.
#[derive(Debug)]
pub(crate) enum Reply {
    Json(u16, Value),
    Html(u16, String),
    Redirect(String),
}

struct ConsentRequest {
    client_id: String,
    client_name: Option<String>,
    redirect_uri: String,
    state: Option<String>,
    code_challenge: String,
    expires: i64,
}

struct CodeGrant {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    expires: i64,
}

pub(crate) struct AuthServer<'a> {
    config: &'a Config,
    store: Store,
    owner: Owner,
    consents: HashMap<String, ConsentRequest>,
    codes: HashMap<String, CodeGrant>,
    failures: Vec<i64>,
}

type Params = HashMap<String, String>;

impl<'a> AuthServer<'a> {
    pub(crate) fn new(config: &'a Config, dir: &Path, owner: Owner) -> Self {
        Self {
            config,
            store: Store::new(dir),
            owner,
            consents: HashMap::new(),
            codes: HashMap::new(),
            failures: Vec::new(),
        }
    }

    pub(crate) fn store(&self) -> &Store {
        &self.store
    }

    fn issuer(&self) -> &str {
        &self.config.public_url
    }

    pub(crate) fn protected_resource_metadata(&self) -> Value {
        json!({
            "resource": self.config.resource_url(),
            "authorization_servers": [self.issuer()],
            "scopes_supported": [SCOPE],
            "bearer_methods_supported": ["header"],
            "resource_name": "tg",
        })
    }

    pub(crate) fn authorization_server_metadata(&self) -> Value {
        let base = self.issuer();
        json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/oauth/authorize"),
            "token_endpoint": format!("{base}/oauth/token"),
            "registration_endpoint": format!("{base}/oauth/register"),
            "response_types_supported": ["code"],
            "response_modes_supported": ["query"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none", "client_secret_post", "client_secret_basic"],
            "scopes_supported": [SCOPE],
            "authorization_response_iss_parameter_supported": true,
        })
    }

    fn redirect_allowed(&self, uri: &str) -> bool {
        if uri == CHATGPT_STABLE_REDIRECT
            || self.config.extra_redirect_uris.iter().any(|u| u == uri)
        {
            return true;
        }
        uri.strip_prefix(CHATGPT_REDIRECT_PREFIX)
            .is_some_and(|segment| {
                !segment.is_empty()
                    && segment
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            })
    }

    /// RFC 7591 dynamic client registration, limited to allowlisted redirects.
    pub(crate) fn register(&self, body: &[u8]) -> Reply {
        let request: Value = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => return oauth_error(400, "invalid_client_metadata", "body must be JSON"),
        };
        let redirect_uris: Vec<String> = request
            .get("redirect_uris")
            .and_then(Value::as_array)
            .map(|uris| {
                uris.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if redirect_uris.is_empty() || !redirect_uris.iter().all(|uri| self.redirect_allowed(uri)) {
            return oauth_error(400, "invalid_redirect_uri", "redirect_uris are not allowed");
        }
        let auth_method = request
            .get("token_endpoint_auth_method")
            .and_then(Value::as_str)
            .unwrap_or("none");
        if !["none", "client_secret_post", "client_secret_basic"].contains(&auth_method) {
            return oauth_error(
                400,
                "invalid_client_metadata",
                "unsupported token_endpoint_auth_method",
            );
        }
        let client_name = request
            .get("client_name")
            .and_then(Value::as_str)
            .map(|name| name.chars().take(80).collect::<String>());
        let mut state = match self.store.load() {
            Ok(state) => state,
            Err(error) => return server_error(&error),
        };
        if state.clients.len() >= MAX_CLIENTS {
            return oauth_error(
                400,
                "invalid_client_metadata",
                "client limit reached; run `tg mcp revoke`",
            );
        }
        let (client_id, secret) = match (random_token("tgc_"), random_token("tgs_")) {
            (Ok(id), Ok(secret)) => (id, secret),
            (Err(error), _) | (_, Err(error)) => return server_error(&error),
        };
        let confidential = auth_method != "none";
        let now = now_timestamp();
        state.clients.push(Client {
            client_id: client_id.clone(),
            secret_hash: confidential.then(|| hash_token(&secret)),
            redirect_uris: redirect_uris.clone(),
            client_name: client_name.clone(),
            created: now,
        });
        if let Err(error) = self.store.save(state) {
            return server_error(&error);
        }
        let mut response = json!({
            "client_id": client_id,
            "client_id_issued_at": now,
            "redirect_uris": redirect_uris,
            "token_endpoint_auth_method": auth_method,
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "scope": SCOPE,
        });
        if let Some(name) = client_name {
            response["client_name"] = json!(name);
        }
        if confidential {
            response["client_secret"] = json!(secret);
            response["client_secret_expires_at"] = json!(0);
        }
        Reply::Json(201, response)
    }

    fn resource_matches(&self, resource: Option<&String>) -> bool {
        let Some(resource) = resource else {
            return true;
        };
        let resource = resource.trim_end_matches('/');
        resource == self.config.public_url || resource == self.config.resource_url()
    }

    /// GET /oauth/authorize: validate the request and show the consent page.
    pub(crate) fn authorize_page(&mut self, params: &Params) -> Reply {
        self.expire();
        let Some(client_id) = params.get("client_id") else {
            return consent_error("Missing client_id.");
        };
        let client = match self.store.load() {
            Ok(state) => state
                .clients
                .into_iter()
                .find(|c| &c.client_id == client_id),
            Err(error) => return server_error(&error),
        };
        let Some(client) = client else {
            return consent_error("Unknown client. Remove and re-add the connector.");
        };
        let Some(redirect_uri) = params
            .get("redirect_uri")
            .filter(|uri| client.redirect_uris.contains(uri))
        else {
            return consent_error("redirect_uri does not match the registered client.");
        };
        // From here on errors go back to the client.
        let state = params.get("state").cloned();
        let back =
            |error: &str| redirect_error(redirect_uri, error, state.as_deref(), self.issuer());
        if params.get("response_type").map(String::as_str) != Some("code") {
            return back("unsupported_response_type");
        }
        let challenge = params
            .get("code_challenge")
            .filter(|c| (43..=128).contains(&c.len()));
        if params.get("code_challenge_method").map(String::as_str) != Some("S256")
            || challenge.is_none()
        {
            return back("invalid_request");
        }
        if !self.resource_matches(params.get("resource")) {
            return back("invalid_target");
        }
        let csrf = match random_token("") {
            Ok(value) => value,
            Err(error) => return server_error(&error),
        };
        self.consents.insert(
            csrf.clone(),
            ConsentRequest {
                client_id: client.client_id,
                client_name: client.client_name.clone(),
                redirect_uri: redirect_uri.clone(),
                state,
                code_challenge: challenge.cloned().unwrap_or_default(),
                expires: now_timestamp() + CONSENT_TTL,
            },
        );
        Reply::Html(
            200,
            consent_page(&csrf, client.client_name.as_deref(), redirect_uri, None),
        )
    }

    /// POST /oauth/authorize: check the passphrase and issue a code.
    pub(crate) fn authorize_submit(&mut self, form: &Params) -> Reply {
        self.expire();
        let Some(csrf) = form.get("request") else {
            return consent_error("Missing request. Start the sign-in again from the client.");
        };
        let Some(request) = self.consents.remove(csrf) else {
            return consent_error("This sign-in request expired. Start it again from the client.");
        };
        if form.get("decision").map(String::as_str) == Some("deny") {
            return redirect_error(
                &request.redirect_uri,
                "access_denied",
                request.state.as_deref(),
                self.issuer(),
            );
        }
        let now = now_timestamp();
        if self.failures.len() >= MAX_FAILURES {
            self.consents.insert(csrf.clone(), request);
            return Reply::Html(
                429,
                simple_page("Too many attempts", "Wait 15 minutes, then try again."),
            );
        }
        let passphrase = form.get("passphrase").map(String::as_str).unwrap_or("");
        if !self.owner.verify(passphrase) {
            self.failures.push(now);
            let page = consent_page(
                csrf,
                request.client_name.as_deref(),
                &request.redirect_uri,
                Some("Wrong passphrase."),
            );
            self.consents.insert(csrf.clone(), request);
            return Reply::Html(401, page);
        }
        let code = match random_token("tgcode_") {
            Ok(code) => code,
            Err(error) => return server_error(&error),
        };
        self.codes.insert(
            hash_token(&code),
            CodeGrant {
                client_id: request.client_id,
                redirect_uri: request.redirect_uri.clone(),
                code_challenge: request.code_challenge,
                expires: now + CODE_TTL,
            },
        );
        let mut location = append_query(&request.redirect_uri, "code", &code);
        if let Some(state) = &request.state {
            location = append_query(&location, "state", state);
        }
        Reply::Redirect(append_query(&location, "iss", self.issuer()))
    }

    /// POST /oauth/token. `basic` is the decoded HTTP Basic credential, if any.
    pub(crate) fn token(&mut self, form: &Params, basic: Option<(String, String)>) -> Reply {
        self.expire();
        let (client_id, secret) = match basic {
            Some((id, secret)) => (Some(id), Some(secret)),
            None => (
                form.get("client_id").cloned(),
                form.get("client_secret").cloned(),
            ),
        };
        let Some(client_id) = client_id else {
            return oauth_error(401, "invalid_client", "client_id is required");
        };
        let mut state = match self.store.load() {
            Ok(state) => state,
            Err(error) => return server_error(&error),
        };
        let Some(client) = state.clients.iter().find(|c| c.client_id == client_id) else {
            return oauth_error(401, "invalid_client", "unknown client");
        };
        if let Some(expected) = &client.secret_hash {
            let presented = secret.as_deref().map(hash_token).unwrap_or_default();
            if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
                return oauth_error(401, "invalid_client", "client authentication failed");
            }
        }
        if !self.resource_matches(form.get("resource")) {
            return oauth_error(400, "invalid_target", "resource does not match this server");
        }
        let grant_id = match form.get("grant_type").map(String::as_str) {
            Some("authorization_code") => {
                let code = form.get("code").map(String::as_str).unwrap_or("");
                let Some(grant) = self.codes.remove(&hash_token(code)) else {
                    return oauth_error(400, "invalid_grant", "code is invalid, used, or expired");
                };
                let verifier = form.get("code_verifier").map(String::as_str).unwrap_or("");
                let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
                if grant.client_id != client_id
                    || form.get("redirect_uri") != Some(&grant.redirect_uri)
                    || !constant_time_eq(challenge.as_bytes(), grant.code_challenge.as_bytes())
                {
                    return oauth_error(400, "invalid_grant", "code does not match this request");
                }
                match random_token("") {
                    Ok(id) => id,
                    Err(error) => return server_error(&error),
                }
            }
            Some("refresh_token") => {
                let hash = hash_token(form.get("refresh_token").map(String::as_str).unwrap_or(""));
                let now = now_timestamp();
                let Some(token) = state.tokens.iter_mut().find(|token| {
                    token.kind == TokenKind::Refresh
                        && token.hash == hash
                        && token.client_id == client_id
                }) else {
                    return oauth_error(400, "invalid_grant", "refresh token is invalid");
                };
                if token.used {
                    // Reuse of a rotated token: assume theft and end the whole grant.
                    let grant = token.grant_id.clone();
                    state.tokens.retain(|token| token.grant_id != grant);
                    if let Err(error) = self.store.save(state) {
                        return server_error(&error);
                    }
                    return oauth_error(
                        400,
                        "invalid_grant",
                        "refresh token was already used; sign in again",
                    );
                }
                if token.expires <= now {
                    return oauth_error(400, "invalid_grant", "refresh token expired");
                }
                token.used = true;
                token.grant_id.clone()
            }
            _ => {
                return oauth_error(
                    400,
                    "unsupported_grant_type",
                    "use authorization_code or refresh_token",
                )
            }
        };
        let (access, refresh) = match (random_token("tgat_"), random_token("tgrt_")) {
            (Ok(access), Ok(refresh)) => (access, refresh),
            (Err(error), _) | (_, Err(error)) => return server_error(&error),
        };
        let now = now_timestamp();
        for (token, kind, ttl) in [
            (&access, TokenKind::Access, ACCESS_TTL),
            (&refresh, TokenKind::Refresh, REFRESH_TTL),
        ] {
            state.tokens.push(Token {
                hash: hash_token(token),
                kind,
                client_id: client_id.clone(),
                grant_id: grant_id.clone(),
                expires: now + ttl,
                used: false,
            });
        }
        if let Err(error) = self.store.save(state) {
            return server_error(&error);
        }
        Reply::Json(
            200,
            json!({
                "access_token": access,
                "token_type": "Bearer",
                "expires_in": ACCESS_TTL,
                "refresh_token": refresh,
                "scope": SCOPE,
            }),
        )
    }

    fn expire(&mut self) {
        let now = now_timestamp();
        self.consents.retain(|_, request| request.expires > now);
        self.codes.retain(|_, code| code.expires > now);
        self.failures.retain(|at| now - at < FAILURE_WINDOW);
    }
}

fn random_bytes<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|error| format!("Random source failed: {error}"))?;
    Ok(bytes)
}

fn random_token(prefix: &str) -> Result<String, String> {
    Ok(format!(
        "{prefix}{}",
        URL_SAFE_NO_PAD.encode(random_bytes::<32>()?)
    ))
}

fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn oauth_error(status: u16, error: &str, description: &str) -> Reply {
    Reply::Json(
        status,
        json!({ "error": error, "error_description": description }),
    )
}

fn server_error(error: &str) -> Reply {
    log::error!("oauth: {error}");
    oauth_error(500, "server_error", "internal error")
}

fn redirect_error(redirect_uri: &str, error: &str, state: Option<&str>, issuer: &str) -> Reply {
    let mut location = append_query(redirect_uri, "error", error);
    if let Some(state) = state {
        location = append_query(&location, "state", state);
    }
    Reply::Redirect(append_query(&location, "iss", issuer))
}

fn consent_error(message: &str) -> Reply {
    Reply::Html(400, simple_page("Cannot sign in", message))
}

pub(crate) fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn append_query(uri: &str, key: &str, value: &str) -> String {
    let separator = if uri.contains('?') { '&' } else { '?' };
    format!("{uri}{separator}{key}={}", percent_encode(value))
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const PAGE_STYLE: &str = "body{font:16px/1.5 -apple-system,system-ui,sans-serif;max-width:28rem;margin:4rem auto;padding:0 1rem;color:#1d1d1f;background:#fff}\
h1{font-size:1.3rem}input[type=password]{width:100%;padding:.6rem;font-size:1rem;box-sizing:border-box;border:1px solid #bbb;border-radius:6px}\
button{margin-top:1rem;margin-right:.5rem;padding:.55rem 1.1rem;font-size:1rem;border-radius:6px;border:1px solid #888;background:#f4f4f4}\
button.primary{background:#1d1d1f;color:#fff;border-color:#1d1d1f}.error{color:#b00020}.muted{color:#666;font-size:.9rem}\
@media(prefers-color-scheme:dark){body{background:#111;color:#eee}input[type=password]{background:#222;color:#eee;border-color:#555}button{background:#222;color:#eee}button.primary{background:#eee;color:#111}}";

fn simple_page(title: &str, message: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>tg</title><style>{PAGE_STYLE}</style></head>\
<body><h1>{}</h1><p>{}</p></body></html>",
        html_escape(title),
        html_escape(message)
    )
}

fn consent_page(
    csrf: &str,
    client_name: Option<&str>,
    redirect_uri: &str,
    error: Option<&str>,
) -> String {
    let redirect_host = redirect_uri
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(redirect_uri);
    let error = error
        .map(|message| format!("<p class=error>{}</p>", html_escape(message)))
        .unwrap_or_default();
    format!(
        "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>Allow access to tg</title><style>{PAGE_STYLE}</style></head>\
<body><h1>Allow read-only access to your chat history?</h1>\
<p><b>{}</b> will be able to list chats and read and search messages, within your tg remote policy. It will return to <b>{}</b>.</p>\
{error}<form method=post action=\"/oauth/authorize\"><input type=hidden name=request value=\"{}\">\
<label for=p>Owner passphrase</label><input id=p type=password name=passphrase autocomplete=current-password autofocus>\
<div><button class=primary name=decision value=allow>Allow</button><button name=decision value=deny>Deny</button></div></form>\
<p class=muted>Revoke later with <code>tg mcp revoke</code>.</p></body></html>",
        html_escape(client_name.unwrap_or("An MCP client")),
        html_escape(redirect_host),
        html_escape(csrf),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::config::RemotePolicy;

    const REDIRECT: &str = "https://chatgpt.com/connector/oauth/abc_123";

    fn config() -> Config {
        Config {
            public_url: "https://mcp.example.com".into(),
            listen: "127.0.0.1:8765".parse().unwrap(),
            trusted_proxy: None,
            remote: RemotePolicy::default(),
            extra_redirect_uris: Vec::new(),
        }
    }

    fn owner() -> Owner {
        // Few rounds keep the test fast; production uses PBKDF2_ROUNDS.
        let salt = [7u8; 16];
        Owner {
            salt: hex::encode(salt),
            rounds: 1000,
            hash: hex::encode(derive("correct horse battery staple", &salt, 1000)),
        }
    }

    fn params(pairs: &[(&str, &str)]) -> Params {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn json_of(reply: Reply) -> (u16, Value) {
        match reply {
            Reply::Json(status, value) => (status, value),
            other => panic!("expected JSON, got {other:?}"),
        }
    }

    fn query_param(location: &str, key: &str) -> Option<String> {
        location
            .split_once('?')?
            .1
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{key}=")).map(str::to_string))
    }

    struct Flow {
        client_id: String,
        verifier: String,
    }

    fn register(server: &AuthServer<'_>) -> String {
        let (status, body) = json_of(
            server.register(
                json!({"redirect_uris": [REDIRECT], "client_name": "ChatGPT"})
                    .to_string()
                    .as_bytes(),
            ),
        );
        assert_eq!(status, 201, "{body}");
        body["client_id"].as_str().unwrap().to_string()
    }

    fn authorize(server: &mut AuthServer<'_>, flow: &Flow, passphrase: &str) -> Reply {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(flow.verifier.as_bytes()));
        let page = server.authorize_page(&params(&[
            ("response_type", "code"),
            ("client_id", &flow.client_id),
            ("redirect_uri", REDIRECT),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", "xyz"),
            ("resource", "https://mcp.example.com/mcp"),
        ]));
        let Reply::Html(200, html) = page else {
            panic!("expected consent page, got {page:?}");
        };
        let csrf = html
            .split("name=request value=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        server.authorize_submit(&params(&[
            ("request", csrf),
            ("passphrase", passphrase),
            ("decision", "allow"),
        ]))
    }

    fn exchange(server: &mut AuthServer<'_>, flow: &Flow, code: &str) -> (u16, Value) {
        json_of(server.token(
            &params(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", REDIRECT),
                ("client_id", &flow.client_id),
                ("code_verifier", &flow.verifier),
                ("resource", "https://mcp.example.com/mcp"),
            ]),
            None,
        ))
    }

    #[test]
    fn full_authorization_code_flow_with_refresh_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let config = config();
        let mut server = AuthServer::new(&config, &dir.path().join("state"), owner());
        let flow = Flow {
            client_id: register(&server),
            verifier: "v".repeat(50),
        };

        let Reply::Redirect(location) =
            authorize(&mut server, &flow, "correct horse battery staple")
        else {
            panic!("expected redirect");
        };
        assert!(location.starts_with(REDIRECT));
        assert_eq!(query_param(&location, "state").as_deref(), Some("xyz"));
        assert_eq!(
            query_param(&location, "iss").as_deref(),
            Some("https%3A%2F%2Fmcp.example.com")
        );
        let code = query_param(&location, "code").unwrap();

        let (status, tokens) = exchange(&mut server, &flow, &code);
        assert_eq!(status, 200, "{tokens}");
        let access = tokens["access_token"].as_str().unwrap();
        assert_eq!(server.store().authenticate(access).unwrap(), flow.client_id);
        assert!(server.store().authenticate("tgat_forged").is_err());

        // Codes are single use.
        assert_eq!(
            exchange(&mut server, &flow, &code).1["error"],
            "invalid_grant"
        );

        let refresh = tokens["refresh_token"].as_str().unwrap().to_string();
        let refresh_form = params(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("client_id", &flow.client_id),
        ]);
        let (status, rotated) = json_of(server.token(&refresh_form, None));
        assert_eq!(status, 200);
        let new_access = rotated["access_token"].as_str().unwrap().to_string();
        assert!(server.store().authenticate(&new_access).is_ok());

        // Replaying the old refresh token revokes the whole grant.
        let (status, error) = json_of(server.token(&refresh_form, None));
        assert_eq!(
            (status, error["error"].as_str()),
            (400, Some("invalid_grant"))
        );
        assert!(server.store().authenticate(&new_access).is_err());
        assert!(server.store().authenticate(access).is_err());
    }

    #[test]
    fn wrong_passphrase_is_rejected_and_rate_limited() {
        let dir = tempfile::tempdir().unwrap();
        let config = config();
        let mut server = AuthServer::new(&config, &dir.path().join("state"), owner());
        let flow = Flow {
            client_id: register(&server),
            verifier: "w".repeat(50),
        };
        for _ in 0..MAX_FAILURES {
            assert!(matches!(
                authorize(&mut server, &flow, "wrong"),
                Reply::Html(401, _)
            ));
        }
        assert!(matches!(
            authorize(&mut server, &flow, "correct horse battery staple"),
            Reply::Html(429, _)
        ));
    }

    #[test]
    fn pkce_verifier_and_redirect_must_match() {
        let dir = tempfile::tempdir().unwrap();
        let config = config();
        let mut server = AuthServer::new(&config, &dir.path().join("state"), owner());
        let flow = Flow {
            client_id: register(&server),
            verifier: "x".repeat(50),
        };
        let Reply::Redirect(location) =
            authorize(&mut server, &flow, "correct horse battery staple")
        else {
            panic!("expected redirect");
        };
        let code = query_param(&location, "code").unwrap();
        let wrong = Flow {
            client_id: flow.client_id.clone(),
            verifier: "y".repeat(50),
        };
        assert_eq!(
            exchange(&mut server, &wrong, &code).1["error"],
            "invalid_grant"
        );
    }

    #[test]
    fn registration_only_accepts_allowlisted_redirects() {
        let dir = tempfile::tempdir().unwrap();
        let config = config();
        let server = AuthServer::new(&config, &dir.path().join("state"), owner());
        for bad in [
            "https://evil.example/cb",
            "https://chatgpt.com/connector/oauth/a/b",
            "https://chatgpt.com/connector/oauth/",
            "http://chatgpt.com/connector_platform_oauth_redirect",
        ] {
            let (status, body) =
                json_of(server.register(json!({"redirect_uris": [bad]}).to_string().as_bytes()));
            assert_eq!(
                (status, body["error"].as_str()),
                (400, Some("invalid_redirect_uri")),
                "{bad}"
            );
        }
        let (status, body) = json_of(server.register(
            json!({"redirect_uris": [CHATGPT_STABLE_REDIRECT], "token_endpoint_auth_method": "client_secret_post"})
                .to_string()
                .as_bytes(),
        ));
        assert_eq!(status, 201);
        assert!(body["client_secret"].as_str().unwrap().starts_with("tgs_"));
    }

    #[test]
    fn revoke_removes_clients_and_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let config = config();
        let mut server = AuthServer::new(&config, &dir.path().join("state"), owner());
        let flow = Flow {
            client_id: register(&server),
            verifier: "z".repeat(50),
        };
        let Reply::Redirect(location) =
            authorize(&mut server, &flow, "correct horse battery staple")
        else {
            panic!("expected redirect");
        };
        let (_, tokens) = exchange(&mut server, &flow, &query_param(&location, "code").unwrap());
        let access = tokens["access_token"].as_str().unwrap();
        assert_eq!(server.store().revoke(None).unwrap(), 1);
        assert!(server.store().authenticate(access).is_err());
    }

    #[test]
    fn owner_passphrase_has_a_minimum_length_and_verifies() {
        assert!(Owner::new("short").is_err());
        assert!(owner().verify("correct horse battery staple"));
        assert!(!owner().verify("correct horse battery stapl"));
    }
}
