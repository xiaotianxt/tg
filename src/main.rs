mod cache;
mod db;
mod decrypt;
mod dictionary;
mod doctor;
mod export;
mod logger;
mod media;
mod media_decrypt;
mod media_index;
mod media_key;
mod media_pb;
mod message;
mod output;
mod parallel;
mod scanner;
mod skill;
mod time;

use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

fn print_output(args: std::fmt::Arguments<'_>) {
    if let Err(e) = output::stdout_line(args) {
        log::error!("Error: {}", e);
        std::process::exit(1);
    }
}

fn normalize_args_for_default_messages<I>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args: Vec<OsString> = args.into_iter().collect();
    if should_default_to_messages(&args) {
        args.insert(1, OsString::from("messages"));
    }
    args
}

fn should_default_to_messages(args: &[OsString]) -> bool {
    let Some(first_arg) = args.get(1).and_then(|arg| arg.to_str()) else {
        return false;
    };

    !first_arg.starts_with('-') && !is_known_subcommand(first_arg)
}

fn is_known_subcommand(value: &str) -> bool {
    matches!(
        value,
        "keys"
            | "decrypt"
            | "sessions"
            | "messages"
            | "search"
            | "export"
            | "image"
            | "doctor"
            | "refresh"
            | "skill"
            | "help"
    )
}

fn print_refresh_stats(label: &str, stats: &decrypt::DecryptStats) {
    print_output(format_args!(
        "{}: {} succeeded, {} failed, {} skipped, {} total",
        label, stats.success, stats.failed, stats.skipped, stats.total
    ));
}

fn ensure_message_cache_ready(decrypted_dir: &Path, jobs: usize) {
    let refresh = cache::refresh_message_decrypted(decrypted_dir, jobs);
    let refresh = if cache::needs_message_key_retry(&refresh) {
        log::warn!(
            "Decrypted message cache is incomplete ({}). Refreshing keys and retrying once.",
            cache::retry_reason(&refresh)
        );
        cache::refresh_keys_and_message_decrypted(decrypted_dir, jobs)
    } else {
        refresh
    };

    match refresh {
        Ok(stats) if cache::failures_can_affect_messages(&stats) => {
            log::error!(
                "Cannot read messages because the decrypted cache is still incomplete after refreshing keys. Failed: {}",
                cache::message_failure_summary(&stats)
            );
            std::process::exit(1);
        }
        Ok(_) => {}
        Err(e) if decrypt::is_refresh_lock_busy_error(&e) => {
            log::warn!(
                "Decrypted cache refresh is already running; reading the existing decrypted cache."
            );
        }
        Err(e) => {
            log::error!("Cannot refresh decrypted message cache: {}", e);
            std::process::exit(1);
        }
    }
}

#[derive(Parser)]
#[command(
    name = "tg",
    version,
    about = "Read Telegram messages from local databases"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Extract DB encryption keys from Telegram process memory (requires sudo)
    Keys {
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
        /// Incremental mode is the default; kept for compatibility with old scripts
        #[arg(short, long, hide = true)]
        incremental: bool,
        /// Force decrypting every database even when cached outputs are up to date
        #[arg(long)]
        full: bool,
        /// Only decrypt databases modified after this time (ISO 8601 or relative: 5min, 1h, today)
        #[arg(long)]
        since: Option<String>,
        /// Show decrypt progress and summary
        #[arg(long)]
        verbose: bool,
        /// Number of parallel jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },
    /// List all chat sessions/conversations
    Sessions {
        /// Path to decrypted databases
        #[arg(long, default_value = "decrypted")]
        decrypted_dir: PathBuf,
        /// Number of top sessions to show
        #[arg(long, default_value_t = 30)]
        top: usize,
        /// Number of parallel jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },
    /// Diagnose tg setup and optionally a specific chat
    Doctor {
        /// Optional session username (tgid_xxx) or display name to inspect
        session: Option<String>,
        /// Path to decrypted databases
        #[arg(long, default_value = "decrypted")]
        decrypted_dir: PathBuf,
        /// Number of parallel jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },
    /// Refresh decrypted cache, refreshing keys if needed
    Refresh {
        /// Path to decrypted databases
        #[arg(long, default_value = "decrypted")]
        decrypted_dir: PathBuf,
        /// Extract keys before decrypting
        #[arg(long)]
        keys: bool,
        /// Number of parallel jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },
    /// Read messages from a specific session
    Messages {
        /// Session username (tgid_xxx) or display name to search
        session: String,
        /// Path to decrypted databases
        #[arg(long, default_value = "decrypted")]
        decrypted_dir: PathBuf,
        /// Number of messages to show (defaults to 50 unless --since is used)
        #[arg(long)]
        limit: Option<usize>,
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
        /// Show earliest messages instead of the default latest messages
        #[arg(long, conflicts_with = "tail")]
        head: bool,
        /// Timestamp grouping for output: 1m/1min, 1h, 1d, 1mo, 1y, full, or none
        #[arg(long, default_value = "1m")]
        time_bucket: String,
        /// Number of parallel jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
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
        /// Show matches after this time (ISO 8601 or relative: 5min, 1h, today)
        #[arg(long)]
        since: Option<String>,
        /// Number of parallel jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
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
        /// Number of parallel jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },
    /// Export local cached images from a specific session
    Image {
        /// Session username (tgid_xxx) or display name to search
        session: String,
        /// Path to decrypted databases
        #[arg(long, default_value = "decrypted")]
        decrypted_dir: PathBuf,
        /// Output directory for readable image files
        #[arg(long, default_value = "exported/images")]
        output: PathBuf,
        /// List recent image messages without exporting
        #[arg(long, conflicts_with_all = ["all", "index"])]
        list: bool,
        /// Export every locally cached image in the selected window
        #[arg(long, conflicts_with = "index")]
        all: bool,
        /// Export the Nth image shown by --list (newest first)
        #[arg(long)]
        index: Option<usize>,
        /// Export an image by compact message identifier
        #[arg(long, conflicts_with_all = ["list", "all", "index"])]
        id: Option<String>,
        /// Number of recent image messages to scan
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Only consider images after this time (ISO 8601 or relative: 5min, 1h, today)
        #[arg(long)]
        since: Option<String>,
        /// Number of parallel jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },
    /// Install or manage the local agent skill
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },
}

#[derive(Subcommand)]
enum SkillCommands {
    /// Install the local SKILL.md generated from tg's bundled template
    Install {
        /// Skill directory to write; defaults to $CODEX_HOME/skills/tg or ~/.codex/skills/tg
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

fn main() {
    logger::init();
    scanner::maybe_run_internal_scanner();
    let cli = Cli::parse_from(normalize_args_for_default_messages(std::env::args_os()));

    match cli.command {
        Commands::Keys { timeout } => match scanner::extract_keys(timeout) {
            Ok(path) => print_output(format_args!("Keys saved to: {}", path)),
            Err(e) => {
                log::error!("Error: {}", e);
                std::process::exit(1);
            }
        },
        Commands::Decrypt {
            keys,
            output,
            db_dir,
            incremental: _,
            full,
            since,
            verbose,
            jobs,
        } => {
            let since_ts = match time::parse_since_opt(since.as_deref()) {
                Ok(ts) => ts,
                Err(e) => {
                    log::error!("Error parsing --since: {}", e);
                    std::process::exit(1);
                }
            };
            let config = decrypt::DecryptConfig {
                incremental: !full,
                since: since_ts,
                scope: decrypt::DecryptScope::All,
                recent_output_grace: None,
                quiet: !verbose,
                jobs,
            };
            match decrypt::decrypt_all(&keys, &output, db_dir.as_deref(), &config) {
                Ok(stats) => {
                    if verbose {
                        if stats.skipped > 0 {
                            log::info!("Decryption complete: {} succeeded, {} failed, {} skipped, {} total",
                                stats.success, stats.failed, stats.skipped, stats.total);
                        } else {
                            log::info!(
                                "Decryption complete: {} succeeded, {} failed, {} total",
                                stats.success,
                                stats.failed,
                                stats.total
                            );
                        }
                    }
                }
                Err(e) => {
                    log::error!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Sessions {
            decrypted_dir,
            top,
            jobs,
        } => {
            let _ = cache::refresh_decrypted(&decrypted_dir, jobs);
            match db::list_sessions(&decrypted_dir, top, jobs) {
                Ok(sessions) => {
                    if sessions.is_empty() {
                        print_output(format_args!(
                            "No sessions found. Try running 'decrypt' first."
                        ));
                    }
                }
                Err(e) => {
                    log::error!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Doctor {
            session,
            decrypted_dir,
            jobs,
        } => {
            if let Err(e) = doctor::run(doctor::DoctorOptions {
                session: session.as_deref(),
                decrypted_dir: &decrypted_dir,
                jobs,
            }) {
                log::error!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Refresh {
            decrypted_dir,
            keys,
            jobs,
        } => {
            let refresh = if keys {
                cache::refresh_keys_and_decrypted(&decrypted_dir, jobs)
            } else {
                let refresh = cache::refresh_decrypted(&decrypted_dir, jobs);
                if cache::needs_message_key_retry(&refresh) {
                    log::warn!(
                        "Decrypted cache refresh had issues ({}). Refreshing keys and retrying once.",
                        cache::retry_reason(&refresh)
                    );
                    cache::refresh_keys_and_decrypted(&decrypted_dir, jobs)
                } else {
                    refresh
                }
            };

            match refresh {
                Ok(stats) => print_refresh_stats("Refresh complete", &stats),
                Err(e) => {
                    log::error!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Messages {
            session,
            decrypted_dir,
            limit,
            offset,
            search,
            since,
            tail,
            head,
            time_bucket,
            jobs,
        } => {
            let since_ts = match time::parse_since_opt(since.as_deref()) {
                Ok(ts) => ts,
                Err(e) => {
                    log::error!("Error parsing --since: {}", e);
                    std::process::exit(1);
                }
            };
            let time_bucket = match time::parse_message_time_bucket(&time_bucket) {
                Ok(bucket) => bucket,
                Err(e) => {
                    log::error!("Error parsing --time-bucket: {}", e);
                    std::process::exit(1);
                }
            };
            let limit = limit.or_else(|| since_ts.is_none().then_some(50));
            let use_tail = tail || (!head && offset == 0);
            ensure_message_cache_ready(&decrypted_dir, jobs);
            let read_messages = || {
                db::read_messages(
                    &decrypted_dir,
                    db::ReadMessagesOptions {
                        session_query: &session,
                        limit,
                        offset,
                        search_query: search.as_deref(),
                        since: since_ts,
                        tail: use_tail,
                        time_bucket,
                        jobs,
                    },
                )
            };

            let msg_count = match read_messages() {
                Ok(count) => count,
                Err(e) => {
                    log::error!("Error: {}", e);
                    std::process::exit(1);
                }
            };

            if msg_count == 0 {
                print_output(format_args!(
                    "No messages found for '{}'. Use 'sessions' to list available sessions.",
                    session
                ));
            }
        }
        Commands::Search {
            query,
            decrypted_dir,
            limit,
            since,
            jobs,
        } => {
            let since_ts = match time::parse_since_opt(since.as_deref()) {
                Ok(ts) => ts,
                Err(e) => {
                    log::error!("Error parsing --since: {}", e);
                    std::process::exit(1);
                }
            };
            let refresh = cache::refresh_decrypted(&decrypted_dir, jobs);
            if cache::needs_search_refresh_warning(&refresh) {
                log::warn!(
                    "Decrypted cache refresh had issues ({}). Search results may be stale.",
                    cache::search_refresh_reason(&refresh)
                );
            }
            let use_telegram_fts = match &refresh {
                Ok(stats) => !cache::failures_can_affect_telegram_fts(stats),
                Err(_) => true,
            };
            match db::search_messages(
                &decrypted_dir,
                db::SearchMessagesOptions {
                    query: &query,
                    limit,
                    since: since_ts,
                    use_telegram_fts,
                    jobs,
                },
            ) {
                Ok(count) => {
                    if count == 0 {
                        print_output(format_args!("No messages found for '{}'.", query));
                    }
                }
                Err(e) => {
                    log::error!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Export {
            session,
            decrypted_dir,
            format,
            output,
            media_dir,
            jobs,
        } => {
            let _ = cache::refresh_decrypted(&decrypted_dir, jobs);
            match export::export_messages(
                &decrypted_dir,
                &session,
                &format,
                &output,
                media_dir.as_deref(),
                jobs,
            ) {
                Ok(paths) => {
                    print_output(format_args!("Exported to:"));
                    for (fmt, path) in paths {
                        print_output(format_args!("  [{}] {}", fmt, path.display()));
                    }
                }
                Err(e) => {
                    log::error!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Image {
            session,
            decrypted_dir,
            output,
            list,
            all,
            index,
            id,
            limit,
            since,
            jobs,
        } => {
            let since_ts = match time::parse_since_opt(since.as_deref()) {
                Ok(ts) => ts,
                Err(e) => {
                    log::error!("Error parsing --since: {}", e);
                    std::process::exit(1);
                }
            };
            let _ = cache::refresh_decrypted(&decrypted_dir, jobs);
            let config = export::ImageExportConfig {
                output_dir: &output,
                list,
                all,
                index,
                id: id.as_deref(),
                limit,
                since: since_ts,
                jobs,
            };
            if let Err(e) = export::export_images(&decrypted_dir, &session, config) {
                log::error!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Skill { command } => match command {
            SkillCommands::Install { dir } => {
                match skill::install(skill::InstallOptions { target_dir: dir }) {
                    Ok(path) => {
                        print_output(format_args!("Skill installed to: {}", path.display()))
                    }
                    Err(e) => {
                        log::error!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn unknown_first_arg_defaults_to_messages() {
        assert_eq!(
            normalize_args_for_default_messages(args(&["tg", "张三", "--limit", "20"])),
            args(&["tg", "messages", "张三", "--limit", "20"])
        );
    }

    #[test]
    fn known_subcommands_are_not_rewritten() {
        for command in [
            "keys", "decrypt", "sessions", "messages", "search", "export", "image", "doctor",
            "refresh", "skill",
        ] {
            assert_eq!(
                normalize_args_for_default_messages(args(&["tg", command])),
                args(&["tg", command])
            );
        }
    }

    #[test]
    fn top_level_flags_and_help_subcommand_are_not_rewritten() {
        assert_eq!(
            normalize_args_for_default_messages(args(&["tg", "--help"])),
            args(&["tg", "--help"])
        );
        assert_eq!(
            normalize_args_for_default_messages(args(&["tg", "help", "messages"])),
            args(&["tg", "help", "messages"])
        );
    }
}
