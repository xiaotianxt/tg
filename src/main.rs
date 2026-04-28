mod scanner;
mod decrypt;
mod db;
mod message;
mod media;
mod export;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "tgreader", version, about = "Read Telegram messages from local databases")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Extract DB encryption keys from Telegram process memory (requires sudo)
    Keys {
        /// Path to the key scanner binary (auto-detected if not provided)
        #[arg(long)]
        scanner: Option<PathBuf>,
        /// Timeout in seconds
        #[arg(long, default_value = "30")]
        timeout: u64,
    },
    /// Decrypt all encrypted databases using extracted keys
    Decrypt {
        /// Path to all_keys.json
        #[arg(long, default_value = "all_keys.json")]
        keys: PathBuf,
        /// Output directory for decrypted databases
        #[arg(long, default_value = "decrypted")]
        output: PathBuf,
        /// Path to Telegram db_storage directory (auto-detected if not provided)
        #[arg(long)]
        db_dir: Option<PathBuf>,
        /// Incremental mode: only decrypt files that have changed since last decrypt
        #[arg(short, long)]
        incremental: bool,
        /// Only decrypt databases modified after this time (ISO 8601 or relative: 5min, 1h, today)
        #[arg(long)]
        since: Option<String>,
    },
    /// List all chat sessions/conversations
    Sessions {
        /// Path to decrypted databases
        #[arg(long, default_value = "decrypted")]
        decrypted_dir: PathBuf,
        /// Number of top sessions to show
        #[arg(long, default_value_t = 30)]
        top: usize,
    },
    /// Read messages from a specific session
    Messages {
        /// Session username (tgid_xxx) or display name to search
        session: String,
        /// Path to decrypted databases
        #[arg(long, default_value = "decrypted")]
        decrypted_dir: PathBuf,
        /// Number of messages to show
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Offset for pagination
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Search within messages
        #[arg(long)]
        search: Option<String>,
        /// Show messages after this time (ISO 8601 or relative: 5min, 1h, today)
        #[arg(long)]
        since: Option<String>,
        /// Show the latest N messages (newest appears last; uses --limit for count)
        #[arg(long)]
        tail: bool,
    },
    /// Search across all sessions
    Search {
        /// Search query
        query: String,
        /// Path to decrypted databases
        #[arg(long, default_value = "decrypted")]
        decrypted_dir: PathBuf,
        /// Number of results
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Export messages to file
    Export {
        /// Session username (tgid_xxx)
        session: String,
        /// Path to decrypted databases
        #[arg(long, default_value = "decrypted")]
        decrypted_dir: PathBuf,
        /// Output format: txt, csv, or json
        #[arg(long, default_value = "txt")]
        format: String,
        /// Output directory
        #[arg(long, default_value = "exported")]
        output: PathBuf,
        /// Directory to save decoded media files (images, stickers, videos)
        #[arg(long)]
        media_dir: Option<PathBuf>,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Keys { scanner, timeout } => {
            let scanner_path = scanner.unwrap_or_else(|| PathBuf::from("./scanner_macos"));
            match scanner::extract_keys(&scanner_path, timeout) {
                Ok(path) => println!("Keys saved to: {}", path),
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Decrypt { keys, output, db_dir, incremental, since } => {
            let since_ts = match since.as_deref().map(parse_relative_time) {
                Some(Ok(ts)) => Some(ts),
                Some(Err(e)) => {
                    eprintln!("Error parsing --since: {}", e);
                    std::process::exit(1);
                }
                None => None,
            };
            let config = decrypt::DecryptConfig {
                incremental,
                since: since_ts,
            };
            match decrypt::decrypt_all(&keys, &output, db_dir.as_deref(), &config) {
                Ok(stats) => {
                    if stats.skipped > 0 {
                        println!("Decryption complete: {} succeeded, {} failed, {} skipped, {} total",
                            stats.success, stats.failed, stats.skipped, stats.total);
                    } else {
                        println!("Decryption complete: {} succeeded, {} failed, {} total",
                            stats.success, stats.failed, stats.total);
                    }
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Sessions { decrypted_dir, top } => {
            match db::list_sessions(&decrypted_dir, top) {
                Ok(sessions) => {
                    if sessions.is_empty() {
                        println!("No sessions found. Try running 'decrypt' first.");
                    }
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Messages { session, decrypted_dir, limit, offset, search, since, tail } => {
            let since_ts = match since.as_deref().map(parse_relative_time) {
                Some(Ok(ts)) => Some(ts),
                Some(Err(e)) => {
                    eprintln!("Error parsing --since: {}", e);
                    std::process::exit(1);
                }
                None => None,
            };
            match db::read_messages(&decrypted_dir, &session, limit, offset, search.as_deref(), since_ts, tail) {
                Ok(msg_count) => {
                    if msg_count == 0 {
                        println!("No messages found for '{}'. Use 'sessions' to list available sessions.", session);
                    }
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Search { query, decrypted_dir, limit } => {
            match db::search_messages(&decrypted_dir, &query, limit) {
                Ok(count) => {
                    if count == 0 {
                        println!("No messages found for '{}'.", query);
                    }
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Export { session, decrypted_dir, format, output, media_dir } => {
            match export::export_messages(&decrypted_dir, &session, &format, &output, media_dir.as_deref()) {
                Ok(paths) => {
                    println!("Exported to:");
                    for (fmt, path) in paths {
                        println!("  [{}] {}", fmt, path.display());
                    }
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}

/// Parse a time expression into a Unix timestamp.
///
/// Accepts ISO 8601 formats (e.g. "2024-01-01T00:00:00", "2024-01-01 00:00:00")
/// and relative expressions (e.g. "5min", "1h", "30s", "2d", "1w", "today").
fn parse_relative_time(s: &str) -> Result<i64, String> {
    use chrono::{DateTime, NaiveDateTime, Utc, Duration};

    // Try ISO 8601 / RFC 3339
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp());
    }

    // Try common date-time formats
    for fmt in &["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(dt.and_utc().timestamp());
        }
    }

    // Date-only: assume start of day
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(dt) = d.and_hms_opt(0, 0, 0) {
            return Ok(dt.and_utc().timestamp());
        }
    }

    // Named expressions
    let now = Utc::now();
    match s {
        "today" => {
            let today = now.date_naive();
            let start = today.and_hms_opt(0, 0, 0).unwrap();
            return Ok(start.and_utc().timestamp());
        }
        "yesterday" => {
            let yesterday = (now - Duration::try_days(1).unwrap_or(Duration::hours(24))).date_naive();
            let start = yesterday.and_hms_opt(0, 0, 0).unwrap();
            return Ok(start.and_utc().timestamp());
        }
        _ => {}
    }

    // Try "min" suffix (e.g. "5min")
    if let Some(num_str) = s.strip_suffix("min") {
        let minutes: i64 = num_str.parse()
            .map_err(|_| format!("Invalid number in '{}'", s))?;
        return Ok(now.timestamp() - minutes * 60);
    }

    // Try single-char suffixes: s, h, d, w
    if s.len() >= 2 {
        let (num_part, unit) = s.split_at(s.len() - 1);
        if let Ok(num) = num_part.parse::<i64>() {
            return match unit {
                "s" => Ok(now.timestamp() - num),
                "h" => Ok(now.timestamp() - num * 3600),
                "d" => Ok(now.timestamp() - num * 86400),
                "w" => Ok(now.timestamp() - num * 604800),
                _ => Err(format!("Unknown time unit '{}' in '{}'", unit, s)),
            };
        }
    }

    Err(format!(
        "Cannot parse time expression '{}'. Use ISO 8601 (e.g. '2024-01-01T00:00:00') \
         or relative (e.g. '5min', '1h', 'today').",
        s
    ))
}
