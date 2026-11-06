//! `tg mcp`: expose tg's read-only commands as Model Context Protocol tools,
//! over stdio for local clients or Streamable HTTP with OAuth for remote ones.
//! See docs/mcp-server-design.md.

mod config;
mod http;
mod oauth;
mod protocol;
mod stdio;
mod tools;

use std::io::BufRead;
use std::net::{IpAddr, SocketAddr};

use clap::Subcommand;

use crate::SourceArgs;
use config::{Config, RemotePolicy};
use oauth::Owner;
use tools::ToolContext;

#[derive(Subcommand)]
pub(crate) enum McpCommand {
    /// Serve MCP over stdin/stdout for a local client
    Stdio {
        /// Apply the remote policy from ~/.tg/mcp/config.json
        #[arg(long)]
        remote_policy: bool,
        #[command(flatten)]
        source: SourceArgs,
    },
    /// Create or update the remote-mode config and owner passphrase
    Init {
        /// Public HTTPS origin clients connect to, e.g. https://mcp.example.com
        #[arg(long)]
        public_url: Option<String>,
        /// Loopback or tailnet address to listen on, e.g. 100.64.0.5:8765
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// Reverse proxy address whose X-Forwarded-For is trusted
        #[arg(long)]
        trusted_proxy: Option<IpAddr>,
        /// Extra OAuth redirect URI to accept (repeatable), e.g. for MCP Inspector
        #[arg(long = "redirect-uri")]
        redirect_uris: Vec<String>,
        /// Read a new owner passphrase from the first line of stdin
        #[arg(long)]
        passphrase_stdin: bool,
    },
    /// Serve MCP over HTTP with OAuth; requires `tg mcp init`
    Serve {
        #[command(flatten)]
        source: SourceArgs,
    },
    /// Show the remote config and connected clients
    Status,
    /// Revoke OAuth clients and their tokens
    Revoke {
        /// Client id to revoke
        #[arg(long, conflicts_with = "all", required_unless_present = "all")]
        client: Option<String>,
        /// Revoke every client
        #[arg(long)]
        all: bool,
    },
    /// Print recent tool calls from the audit log (no message content is logged)
    Audit {
        /// Only entries after this time (ISO 8601 or relative: 1h, 1d, today)
        #[arg(long, default_value = "1d")]
        since: String,
    },
}

pub(crate) fn run(command: McpCommand) -> Result<(), String> {
    let dir = config::state_dir();
    match command {
        McpCommand::Stdio {
            remote_policy,
            source,
        } => {
            let policy = if remote_policy {
                config::load_config(&dir)?.remote
            } else {
                RemotePolicy::default()
            };
            stdio::serve(&tool_context(source, policy, remote_policy))
        }
        McpCommand::Init {
            public_url,
            listen,
            trusted_proxy,
            redirect_uris,
            passphrase_stdin,
        } => {
            let existing: Option<Config> = config::read_private_json(&config::config_path(&dir))?;
            let mut config = match existing {
                Some(config) => config,
                None => Config {
                    public_url: public_url
                        .clone()
                        .ok_or("--public-url is required for the first init")?,
                    listen: listen.ok_or("--listen is required for the first init")?,
                    trusted_proxy: None,
                    remote: RemotePolicy::default(),
                    extra_redirect_uris: Vec::new(),
                },
            };
            if let Some(url) = public_url {
                config.public_url = url.trim_end_matches('/').to_string();
            }
            if let Some(listen) = listen {
                config.listen = listen;
            }
            if trusted_proxy.is_some() {
                config.trusted_proxy = trusted_proxy;
            }
            for uri in redirect_uris {
                if !config.extra_redirect_uris.contains(&uri) {
                    config.extra_redirect_uris.push(uri);
                }
            }
            config.validate()?;
            let owner_path = dir.join("owner.json");
            if passphrase_stdin {
                let mut line = String::new();
                std::io::stdin()
                    .lock()
                    .read_line(&mut line)
                    .map_err(|error| format!("Read passphrase: {error}"))?;
                let owner = Owner::new(line.trim_end_matches(['\n', '\r']))?;
                config::write_private_json(&owner_path, &owner)?;
            } else if config::read_private_json::<Owner>(&owner_path)?.is_none() {
                return Err("no owner passphrase yet; rerun with --passphrase-stdin".to_string());
            }
            config::write_private_json(&config::config_path(&dir), &config)?;
            crate::print_output(format_args!(
                "MCP config saved. Connector URL: {}",
                config.resource_url()
            ))
        }
        McpCommand::Serve { source } => {
            let config = config::load_config(&dir)?;
            let owner: Owner = config::read_private_json(&dir.join("owner.json"))?
                .ok_or("no owner passphrase; run `tg mcp init --passphrase-stdin`")?;
            let ctx = tool_context(source, config.remote.clone(), true);
            // Fail at startup, not on the first remote request.
            crate::source_store::SourceStore::discover(ctx.db_dir.as_deref(), &ctx.keys)
                .map_err(|error| format!("Cannot open the local history: {error}"))?;
            http::serve(&config, &dir, owner, ctx)
        }
        McpCommand::Status => {
            let config = config::load_config(&dir)?;
            let summary = oauth::Store::new(&dir).summary()?;
            crate::print_output(format_args!("Connector URL: {}", config.resource_url()))?;
            crate::print_output(format_args!("Listening on: {}", config.listen))?;
            crate::print_output(format_args!(
                "Active grants: {}; registered clients: {}",
                summary.grants,
                summary.clients.len()
            ))?;
            for (id, name, created) in summary.clients {
                crate::print_output(format_args!(
                    "  {id}  {}  registered {}",
                    name.unwrap_or_else(|| "-".to_string()),
                    crate::time::format_local_timestamp(created)
                ))?;
            }
            Ok(())
        }
        McpCommand::Revoke { client, all: _ } => {
            let removed = oauth::Store::new(&dir).revoke(client.as_deref())?;
            crate::print_output(format_args!(
                "Revoked {removed} client(s) and their tokens."
            ))
        }
        McpCommand::Audit { since } => {
            let since = crate::time::parse_since_opt(Some(&since))?.unwrap_or(0);
            let path = dir.join("audit.log");
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => return Err(format!("Cannot read {}: {error}", path.display())),
            };
            for line in text.lines() {
                let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                if entry["ts"].as_i64().unwrap_or(0) >= since {
                    crate::print_output(format_args!(
                        "{} {} {} args={} rows={} ok={} {}ms",
                        crate::time::format_local_timestamp(entry["ts"].as_i64().unwrap_or(0)),
                        entry["ip"].as_str().unwrap_or("-"),
                        entry["tool"].as_str().unwrap_or("-"),
                        entry["args"],
                        entry["rows"],
                        entry["ok"],
                        entry["ms"],
                    ))?;
                }
            }
            Ok(())
        }
    }
}

fn tool_context(source: SourceArgs, policy: RemotePolicy, remote: bool) -> ToolContext {
    ToolContext {
        db_dir: source.db_dir,
        keys: source.keys,
        jobs: source.jobs,
        policy,
        remote,
    }
}
