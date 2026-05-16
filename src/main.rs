mod cache;
mod completion;
mod completion_values;
mod contact;
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
mod message_index;
mod output;
mod parallel;
mod paths;
mod query;
mod scanner;
mod skill;
mod time;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
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

fn messages_limit_or_default(
    limit: Option<usize>,
    since: Option<i64>,
    all_time: bool,
) -> Option<usize> {
    limit.or_else(|| (since.is_none() && !all_time).then_some(50))
}

fn is_known_subcommand(value: &str) -> bool {
    if matches!(value, "help" | "sql") {
        return true;
    }
    Cli::command()
        .get_subcommands()
        .any(|command| command.get_name() == value)
}

fn print_refresh_stats(label: &str, stats: &decrypt::DecryptStats) {
    print_output(format_args!(
        "{}: {} succeeded, {} failed, {} skipped, {} total",
        label, stats.success, stats.failed, stats.skipped, stats.total
    ));
}

fn ensure_message_cache_ready(decrypted_dir: &Path, jobs: usize) {
    log::info!("Refreshing local message cache");
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
        #[arg(
            long,
            default_value_os_t = paths::default_keys_path(),
            value_hint = clap::ValueHint::FilePath
        )]
        keys: PathBuf,
        /// Output directory for decrypted databases
        #[arg(
            long,
            default_value_os_t = paths::default_decrypted_dir(),
            value_hint = clap::ValueHint::DirPath
        )]
        output: PathBuf,
        /// Path to Telegram db_storage directory (auto-detected if not provided)
        #[arg(long, value_hint = clap::ValueHint::DirPath)]
        db_dir: Option<PathBuf>,
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
        /// Optional display name or username query to filter sessions
        query: Option<String>,
        /// Path to decrypted databases
        #[arg(
            long,
            default_value_os_t = paths::default_decrypted_dir(),
            value_hint = clap::ValueHint::DirPath
        )]
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
        #[arg(
            long,
            default_value_os_t = paths::default_decrypted_dir(),
            value_hint = clap::ValueHint::DirPath
        )]
        decrypted_dir: PathBuf,
        /// Number of parallel jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },
    /// Refresh decrypted cache, refreshing keys if needed
    Refresh {
        /// Path to decrypted databases
        #[arg(
            long,
            default_value_os_t = paths::default_decrypted_dir(),
            value_hint = clap::ValueHint::DirPath
        )]
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
        #[arg(
            long,
            default_value_os_t = paths::default_decrypted_dir(),
            value_hint = clap::ValueHint::DirPath
        )]
        decrypted_dir: PathBuf,
        /// Number of messages to show (defaults to 50 unless --since or --all-time is used)
        #[arg(long)]
        limit: Option<usize>,
        /// Offset for pagination
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Search within messages
        #[arg(long)]
        search: Option<String>,
        /// Show messages after this time (ISO 8601 or relative: 5min, 1h, today)
        #[arg(long, conflicts_with = "all_time")]
        since: Option<String>,
        /// Show the full history instead of the default latest 50 messages
        #[arg(long)]
        all_time: bool,
        /// Show the latest N messages (newest appears last; uses --limit for count)
        #[arg(long)]
        tail: bool,
        /// Show earliest messages instead of the default latest messages
        #[arg(long, conflicts_with = "tail")]
        head: bool,
        /// Timestamp grouping for output: 1m/1min, 1h, 1d, 1mo, 1y, full, or none
        #[arg(
            long,
            default_value = "1m",
            value_parser = completion_values::time_buckets()
        )]
        time_bucket: String,
        /// Use group/member public names instead of your contact remarks
        #[arg(long)]
        anonymous: bool,
        /// Number of parallel jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },
    /// Search across all sessions
    Search {
        /// Search query
        query: String,
        /// Path to decrypted databases
        #[arg(
            long,
            default_value_os_t = paths::default_decrypted_dir(),
            value_hint = clap::ValueHint::DirPath
        )]
        decrypted_dir: PathBuf,
        /// Number of results
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Show matches after this time (ISO 8601 or relative: 5min, 1h, today)
        #[arg(long, conflicts_with = "all_time")]
        since: Option<String>,
        /// Search the full history instead of the default recent window
        #[arg(long)]
        all_time: bool,
        /// Use public names instead of your contact remarks in output
        #[arg(long)]
        anonymous: bool,
        /// Number of parallel jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },
    /// Search message tables with fixed, parameterized query templates
    Query {
        /// Optional session username (tgid_xxx) or display name to search within
        #[arg(long)]
        session: Option<String>,
        /// Path to decrypted databases
        #[arg(
            long,
            default_value_os_t = paths::default_decrypted_dir(),
            value_hint = clap::ValueHint::DirPath
        )]
        decrypted_dir: PathBuf,
        /// Keyword that must appear in message text; repeat for multiple keywords
        #[arg(long = "contains")]
        contains: Vec<String>,
        /// Keyword that must not appear in message text; repeat for multiple keywords
        #[arg(long = "not")]
        not_contains: Vec<String>,
        /// Show messages after this time (ISO 8601 or relative: 5min, 1h, today)
        #[arg(long, conflicts_with = "all_time")]
        since: Option<String>,
        /// Search the full history instead of the default recent window
        #[arg(long)]
        all_time: bool,
        /// Show messages before this time (ISO 8601 or relative: 5min, 1h, today)
        #[arg(long)]
        until: Option<String>,
        /// Maximum rows to print
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Offset for pagination after global sorting
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Sort order: newest or oldest
        #[arg(
            long,
            default_value = "newest",
            value_parser = completion_values::orders()
        )]
        order: String,
        /// Keyword matching mode when --contains is repeated: all or any
        #[arg(
            long,
            default_value = "all",
            value_parser = completion_values::match_modes()
        )]
        match_mode: String,
        /// Output fields: time,session,sender,type,body,timestamp
        #[arg(long, default_value = "time,session,sender,body")]
        fields: String,
        /// Output format: table or json
        #[arg(
            long,
            default_value = "table",
            value_parser = completion_values::query_formats()
        )]
        format: String,
        /// Maximum displayed characters per text cell
        #[arg(long, default_value_t = 500)]
        max_cell_chars: usize,
        /// Use public names instead of your contact remarks in output
        #[arg(long)]
        anonymous: bool,
        /// Number of parallel jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },
    /// Show public query fields and filters without dumping raw database schema
    Schema {
        /// Cache target to check: messages, contact, fts, or message_N
        #[arg(
            long,
            default_value = "messages",
            value_parser = completion_values::db_targets()
        )]
        db: String,
        /// Path to decrypted databases
        #[arg(
            long,
            default_value_os_t = paths::default_decrypted_dir(),
            value_hint = clap::ValueHint::DirPath
        )]
        decrypted_dir: PathBuf,
        /// Output format: table or json
        #[arg(
            long,
            default_value = "table",
            value_parser = completion_values::query_formats()
        )]
        format: String,
        /// Maximum displayed characters per text cell
        #[arg(long, default_value_t = 500)]
        max_cell_chars: usize,
        /// Number of parallel jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },
    /// Export messages to file
    Export {
        /// Session username (tgid_xxx)
        session: String,
        /// Path to decrypted databases
        #[arg(
            long,
            default_value_os_t = paths::default_decrypted_dir(),
            value_hint = clap::ValueHint::DirPath
        )]
        decrypted_dir: PathBuf,
        /// Output format: txt, csv, or json
        #[arg(
            long,
            default_value = "txt",
            value_parser = completion_values::export_formats()
        )]
        format: String,
        /// Output directory
        #[arg(
            long,
            default_value = "exported",
            value_hint = clap::ValueHint::DirPath
        )]
        output: PathBuf,
        /// Directory to save decoded media files (images, stickers, videos, files)
        #[arg(long, value_hint = clap::ValueHint::DirPath)]
        media_dir: Option<PathBuf>,
        /// Export messages after this time (defaults to the recent window)
        #[arg(long, conflicts_with = "all_time")]
        since: Option<String>,
        /// Export at most this many latest messages from the selected window
        #[arg(long)]
        limit: Option<usize>,
        /// Export the full history instead of the default recent window
        #[arg(long)]
        all_time: bool,
        /// Use public names instead of your contact remarks in exported messages
        #[arg(long)]
        anonymous: bool,
        /// Number of parallel jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },
    /// Export local cached images from a specific session
    Image {
        /// Session username (tgid_xxx) or display name to search
        session: String,
        /// Path to decrypted databases
        #[arg(
            long,
            default_value_os_t = paths::default_decrypted_dir(),
            value_hint = clap::ValueHint::DirPath
        )]
        decrypted_dir: PathBuf,
        /// Output directory for readable image files
        #[arg(
            long,
            default_value = "exported/images",
            value_hint = clap::ValueHint::DirPath
        )]
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
    /// Export local cached file attachments from a specific session
    File {
        /// Session username (tgid_xxx) or display name to search
        session: String,
        /// Path to decrypted databases
        #[arg(
            long,
            default_value_os_t = paths::default_decrypted_dir(),
            value_hint = clap::ValueHint::DirPath
        )]
        decrypted_dir: PathBuf,
        /// Output directory for cached file attachments
        #[arg(
            long,
            default_value = "exported/files",
            value_hint = clap::ValueHint::DirPath
        )]
        output: PathBuf,
        /// List recent file messages without exporting
        #[arg(long, conflicts_with_all = ["all", "index"])]
        list: bool,
        /// Export every locally cached file in the selected window
        #[arg(long, conflicts_with = "index")]
        all: bool,
        /// Export the Nth file shown by --list (newest first)
        #[arg(long)]
        index: Option<usize>,
        /// Export a file by compact message identifier or filename
        #[arg(long, conflicts_with_all = ["list", "all", "index"])]
        id: Option<String>,
        /// Number of recent file messages to scan
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Only consider files after this time (ISO 8601 or relative: 5min, 1h, today)
        #[arg(long)]
        since: Option<String>,
        /// Number of parallel jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },
    /// Export local cached voice messages from a specific session
    Voice {
        /// Session username (tgid_xxx) or display name to search
        session: String,
        /// Path to decrypted databases
        #[arg(
            long,
            default_value_os_t = paths::default_decrypted_dir(),
            value_hint = clap::ValueHint::DirPath
        )]
        decrypted_dir: PathBuf,
        /// Output directory for normalized voice files
        #[arg(
            long,
            default_value = "exported/voices",
            value_hint = clap::ValueHint::DirPath
        )]
        output: PathBuf,
        /// Output format: native, wav, or pcm
        #[arg(
            long,
            default_value = "native",
            value_parser = completion_values::voice_formats()
        )]
        format: String,
        /// Path to native voice decoder command (defaults to TG_VOICE_DECODER or PATH lookup)
        #[arg(long, value_hint = clap::ValueHint::FilePath)]
        decoder: Option<PathBuf>,
        /// List recent voice messages without exporting
        #[arg(long, conflicts_with_all = ["all", "index", "id"])]
        list: bool,
        /// Export every local voice message in the selected window
        #[arg(long, conflicts_with_all = ["index", "id"])]
        all: bool,
        /// Export the Nth voice shown by --list (newest first)
        #[arg(long, conflicts_with = "id")]
        index: Option<usize>,
        /// Export a voice by the ID shown in --list
        #[arg(long, conflicts_with_all = ["list", "all", "index"])]
        id: Option<i64>,
        /// Number of recent voice messages to scan
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Only consider voices after this time (ISO 8601 or relative: 5min, 1h, today)
        #[arg(long)]
        since: Option<String>,
        /// Output sample rate for decoded pcm/wav
        #[arg(long, default_value_t = 24000)]
        sample_rate: u32,
        /// Number of parallel jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },
    /// Install or manage the local agent skill
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },
    /// Generate shell completion scripts
    Completions {
        /// Shell to generate completions for
        shell: CompletionShell,
    },
    /// Internal dynamic completion helper
    #[command(name = "__complete", hide = true)]
    Complete {
        /// Dynamic candidate kind
        kind: CompleteKind,
        /// Path to decrypted databases
        #[arg(
            long,
            default_value_os_t = paths::default_decrypted_dir(),
            value_hint = clap::ValueHint::DirPath
        )]
        decrypted_dir: PathBuf,
        /// Maximum candidates to return
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Shell requesting word completion
        #[arg(long, value_enum)]
        shell: Option<CompletionShell>,
        /// Active word index reported by the shell
        #[arg(long)]
        cursor: Option<usize>,
        /// Active token prefix reported by the shell
        #[arg(long, allow_hyphen_values = true)]
        current: Option<String>,
        /// Shell words for runtime completion
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        words: Vec<String>,
    },
}

#[derive(Subcommand)]
enum SkillCommands {
    /// Install the local SKILL.md generated from tg's bundled template
    Install {
        /// Skill directory to write; defaults to $CODEX_HOME/skills/tg or ~/.codex/skills/tg
        #[arg(long, value_hint = clap::ValueHint::DirPath)]
        dir: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum CompletionShell {
    Fish,
    Zsh,
    Bash,
}

#[derive(Clone, Copy, ValueEnum)]
enum CompleteKind {
    Sessions,
    Words,
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
            query,
            decrypted_dir,
            top,
            jobs,
        } => {
            let _ = cache::refresh_message_decrypted(&decrypted_dir, jobs);
            match db::list_sessions(&decrypted_dir, top, query.as_deref(), jobs) {
                Ok(sessions) => {
                    if sessions.is_empty() {
                        if let Some(query) =
                            query.as_deref().filter(|value| !value.trim().is_empty())
                        {
                            print_output(format_args!("No sessions matched '{}'.", query));
                        } else {
                            print_output(format_args!(
                                "No sessions found. Try running 'decrypt' first."
                            ));
                        }
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
                Ok(stats) => {
                    log::info!("Refreshing local message index");
                    if let Err(e) = message_index::ensure_recent(&decrypted_dir, jobs) {
                        log::warn!("Message index refresh failed: {}", e);
                    }
                    print_refresh_stats("Refresh complete", &stats);
                }
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
            all_time,
            tail,
            head,
            time_bucket,
            anonymous,
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
            let limit = messages_limit_or_default(limit, since_ts, all_time);
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
                        name_mode: if anonymous {
                            contact::DisplayNameMode::Anonymous
                        } else {
                            contact::DisplayNameMode::PersonalRemark
                        },
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
            all_time,
            anonymous,
            jobs,
        } => {
            if limit == 0 {
                log::error!("Error: --limit must be greater than 0");
                std::process::exit(1);
            }
            let since_ts = match time::parse_since_opt(since.as_deref()) {
                Ok(ts) => ts,
                Err(e) => {
                    log::error!("Error parsing --since: {}", e);
                    std::process::exit(1);
                }
            };
            let since_ts = if all_time {
                since_ts
            } else {
                Some(since_ts.unwrap_or_else(time::default_recent_since))
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
                    name_mode: if anonymous {
                        contact::DisplayNameMode::Anonymous
                    } else {
                        contact::DisplayNameMode::PersonalRemark
                    },
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
        Commands::Query {
            session,
            decrypted_dir,
            contains,
            not_contains,
            since,
            all_time,
            until,
            limit,
            offset,
            order,
            match_mode,
            fields,
            format,
            max_cell_chars,
            anonymous,
            jobs,
        } => {
            let since_ts = match time::parse_since_opt(since.as_deref()) {
                Ok(ts) => ts,
                Err(e) => {
                    log::error!("Error parsing --since: {}", e);
                    std::process::exit(1);
                }
            };
            let since_ts = if all_time {
                since_ts
            } else {
                Some(since_ts.unwrap_or_else(time::default_recent_since))
            };
            let until_ts = match time::parse_since_opt(until.as_deref()) {
                Ok(ts) => ts,
                Err(e) => {
                    log::error!("Error parsing --until: {}", e);
                    std::process::exit(1);
                }
            };
            let sort = match query::QuerySort::parse(&order) {
                Ok(sort) => sort,
                Err(e) => {
                    log::error!("Error parsing --order: {}", e);
                    std::process::exit(1);
                }
            };
            let match_mode = match query::QueryMatchMode::parse(&match_mode) {
                Ok(mode) => mode,
                Err(e) => {
                    log::error!("Error parsing --match-mode: {}", e);
                    std::process::exit(1);
                }
            };
            let fields = match query::QueryFields::parse(&fields) {
                Ok(fields) => fields,
                Err(e) => {
                    log::error!("Error parsing --fields: {}", e);
                    std::process::exit(1);
                }
            };
            let format = match query::QueryOutputFormat::parse(&format) {
                Ok(format) => format,
                Err(e) => {
                    log::error!("Error parsing --format: {}", e);
                    std::process::exit(1);
                }
            };
            let refresh = cache::refresh_message_decrypted(&decrypted_dir, jobs);
            if cache::needs_message_key_retry(&refresh) {
                log::warn!(
                    "Decrypted message cache refresh had issues ({}). Query results may be stale.",
                    cache::retry_reason(&refresh)
                );
            }
            match query::run(query::QueryOptions {
                decrypted_dir: &decrypted_dir,
                session: session.as_deref(),
                contains: &contains,
                not_contains: &not_contains,
                since: since_ts,
                until: until_ts,
                limit,
                offset,
                sort,
                match_mode,
                fields,
                format,
                max_cell_chars,
                name_mode: if anonymous {
                    contact::DisplayNameMode::Anonymous
                } else {
                    contact::DisplayNameMode::PersonalRemark
                },
                jobs,
            }) {
                Ok(0) => print_output(format_args!("No rows returned.")),
                Ok(_) => {}
                Err(e) => {
                    log::error!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Schema {
            db,
            decrypted_dir,
            format,
            max_cell_chars,
            jobs,
        } => {
            let format = match query::QueryOutputFormat::parse(&format) {
                Ok(format) => format,
                Err(e) => {
                    log::error!("Error parsing --format: {}", e);
                    std::process::exit(1);
                }
            };
            let refresh = cache::refresh_decrypted(&decrypted_dir, jobs);
            if cache::needs_search_refresh_warning(&refresh) {
                log::warn!(
                    "Decrypted cache refresh had issues ({}). Schema may be stale.",
                    cache::search_refresh_reason(&refresh)
                );
            }
            match query::run_schema(query::SchemaOptions {
                decrypted_dir: &decrypted_dir,
                db_target: &db,
                format,
                max_cell_chars,
            }) {
                Ok(0) => print_output(format_args!("No rows returned.")),
                Ok(_) => {}
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
            since,
            limit,
            all_time,
            anonymous,
            jobs,
        } => {
            let since_ts = match time::parse_since_opt(since.as_deref()) {
                Ok(ts) => ts,
                Err(e) => {
                    log::error!("Error parsing --since: {}", e);
                    std::process::exit(1);
                }
            };
            let since_ts = if all_time {
                since_ts
            } else {
                Some(since_ts.unwrap_or_else(time::default_recent_since))
            };
            if limit == Some(0) {
                log::error!("Error: --limit must be greater than 0");
                std::process::exit(1);
            }
            let refresh = cache::refresh_message_decrypted(&decrypted_dir, jobs);
            if cache::needs_message_key_retry(&refresh) {
                log::warn!(
                    "Decrypted message cache refresh had issues ({}). Export may be stale.",
                    cache::retry_reason(&refresh)
                );
            }
            match export::export_messages(export::MessageExportConfig {
                decrypted_dir: &decrypted_dir,
                session_query: &session,
                format: &format,
                output_dir: &output,
                media_dir: media_dir.as_deref(),
                since: since_ts,
                limit,
                name_mode: if anonymous {
                    contact::DisplayNameMode::Anonymous
                } else {
                    contact::DisplayNameMode::PersonalRemark
                },
                jobs,
            }) {
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
        Commands::File {
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
            let config = export::FileExportConfig {
                output_dir: &output,
                list,
                all,
                index,
                id: id.as_deref(),
                limit,
                since: since_ts,
                jobs,
            };
            if let Err(e) = export::export_files(&decrypted_dir, &session, config) {
                log::error!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Voice {
            session,
            decrypted_dir,
            output,
            format,
            decoder,
            list,
            all,
            index,
            id,
            limit,
            since,
            sample_rate,
            jobs,
        } => {
            let since_ts = match time::parse_since_opt(since.as_deref()) {
                Ok(ts) => ts,
                Err(e) => {
                    log::error!("Error parsing --since: {}", e);
                    std::process::exit(1);
                }
            };
            let output_format = match export::VoiceOutputFormat::parse(&format) {
                Ok(format) => format,
                Err(e) => {
                    log::error!("Error parsing --format: {}", e);
                    std::process::exit(1);
                }
            };
            let _ = cache::refresh_decrypted(&decrypted_dir, jobs);
            let config = export::VoiceExportConfig {
                output_dir: &output,
                format: output_format,
                decoder: decoder.as_deref(),
                list,
                all,
                index,
                id,
                limit,
                since: since_ts,
                jobs,
                sample_rate,
            };
            if let Err(e) = export::export_voices(&decrypted_dir, &session, config) {
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
        Commands::Completions { shell } => {
            if let Err(e) = completion::print_script(shell) {
                log::error!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Complete {
            kind,
            decrypted_dir,
            limit,
            shell,
            cursor,
            current,
            words,
        } => {
            let request = completion::CompletionRequest {
                kind,
                decrypted_dir: &decrypted_dir,
                limit,
                shell,
                cursor,
                current: current.as_deref(),
                words: &words,
            };
            if let Err(e) = completion::print_candidates(request) {
                log::error!("Error: {}", e);
                std::process::exit(1);
            }
        }
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
            "keys",
            "decrypt",
            "sessions",
            "messages",
            "search",
            "query",
            "schema",
            "sql",
            "export",
            "image",
            "file",
            "voice",
            "doctor",
            "refresh",
            "skill",
            "completions",
            "__complete",
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

    #[test]
    fn messages_accepts_anonymous_flag() {
        let cli = Cli::parse_from(args(&["tg", "messages", "room", "--anonymous"]));
        match cli.command {
            Commands::Messages { anonymous, .. } => assert!(anonymous),
            _ => panic!("expected messages command"),
        }
    }

    #[test]
    fn content_commands_accept_anonymous_flag() {
        let cli = Cli::parse_from(args(&["tg", "search", "needle", "--anonymous"]));
        match cli.command {
            Commands::Search { anonymous, .. } => assert!(anonymous),
            _ => panic!("expected search command"),
        }

        let cli = Cli::parse_from(args(&[
            "tg",
            "query",
            "--contains",
            "needle",
            "--anonymous",
        ]));
        match cli.command {
            Commands::Query { anonymous, .. } => assert!(anonymous),
            _ => panic!("expected query command"),
        }

        let cli = Cli::parse_from(args(&["tg", "export", "room", "--anonymous"]));
        match cli.command {
            Commands::Export { anonymous, .. } => assert!(anonymous),
            _ => panic!("expected export command"),
        }
    }

    #[test]
    fn messages_accepts_all_time_flag() {
        let cli = Cli::parse_from(args(&["tg", "messages", "room", "--all-time"]));
        match cli.command {
            Commands::Messages { all_time, .. } => assert!(all_time),
            _ => panic!("expected messages command"),
        }
    }

    #[test]
    fn messages_rejects_all_time_with_since() {
        assert!(Cli::try_parse_from(args(&[
            "tg",
            "messages",
            "room",
            "--all-time",
            "--since",
            "today",
        ]))
        .is_err());
    }

    #[test]
    fn messages_all_time_disables_default_limit() {
        assert_eq!(messages_limit_or_default(None, None, false), Some(50));
        assert_eq!(messages_limit_or_default(None, None, true), None);
        assert_eq!(messages_limit_or_default(None, Some(1), false), None);
        assert_eq!(messages_limit_or_default(Some(20), None, true), Some(20));
    }

    #[test]
    fn default_messages_accepts_anonymous_flag() {
        let cli = Cli::parse_from(normalize_args_for_default_messages(args(&[
            "tg",
            "room",
            "--anonymous",
        ])));
        match cli.command {
            Commands::Messages {
                session, anonymous, ..
            } => {
                assert_eq!(session, "room");
                assert!(anonymous);
            }
            _ => panic!("expected messages command"),
        }
    }

    #[test]
    fn sessions_accepts_optional_query() {
        let cli = Cli::parse_from(args(&["tg", "sessions", "alice"]));
        match cli.command {
            Commands::Sessions { query, .. } => assert_eq!(query.as_deref(), Some("alice")),
            _ => panic!("expected sessions command"),
        }
    }

    #[test]
    fn voice_accepts_index_selection() {
        let cli = Cli::parse_from(args(&[
            "tg", "voice", "alice", "--index", "2", "--format", "wav",
        ]));
        match cli.command {
            Commands::Voice {
                session,
                index,
                format,
                ..
            } => {
                assert_eq!(session, "alice");
                assert_eq!(index, Some(2));
                assert_eq!(format, "wav");
            }
            _ => panic!("expected voice command"),
        }
    }

    #[test]
    fn voice_accepts_id_selection() {
        let cli = Cli::parse_from(args(&["tg", "voice", "alice", "--id", "42"]));
        match cli.command {
            Commands::Voice { id, .. } => assert_eq!(id, Some(42)),
            _ => panic!("expected voice command"),
        }
    }

    #[test]
    fn file_accepts_index_selection() {
        let cli = Cli::parse_from(args(&["tg", "file", "alice", "--index", "2"]));
        match cli.command {
            Commands::File { session, index, .. } => {
                assert_eq!(session, "alice");
                assert_eq!(index, Some(2));
            }
            _ => panic!("expected file command"),
        }
    }

    #[test]
    fn file_accepts_id_selection() {
        let cli = Cli::parse_from(args(&["tg", "file", "alice", "--id", "report.pdf"]));
        match cli.command {
            Commands::File { id, .. } => assert_eq!(id.as_deref(), Some("report.pdf")),
            _ => panic!("expected file command"),
        }
    }

    #[test]
    fn query_accepts_structured_filters() {
        let cli = Cli::parse_from(args(&[
            "tg",
            "query",
            "--contains",
            "needle",
            "--fields",
            "time,body",
        ]));
        match cli.command {
            Commands::Query {
                contains, fields, ..
            } => {
                assert_eq!(contains, vec!["needle"]);
                assert_eq!(fields, "time,body");
            }
            _ => panic!("expected query command"),
        }
    }
}
