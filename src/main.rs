mod cache;
mod completion;
mod completion_values;
mod contact;
mod database_keys;
mod db;
mod decrypt;
mod dictionary;
mod doctor;
mod export;
mod fts;
mod key_material;
mod logger;
mod media;
mod media_decrypt;
mod media_index;
mod media_key;
mod media_pb;
mod message;
mod output;
mod parallel;
mod paths;
mod query;
mod scanner;
mod sqlite;
mod time;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

fn print_output(args: std::fmt::Arguments<'_>) -> Result<(), String> {
    output::stdout_line(args).map_err(|e| format!("Error: {e}"))
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
    if matches!(
        value,
        "help" | "sql" | "image" | "file" | "sticker" | "emoji" | "voice" | "__complete"
    ) {
        return true;
    }
    Cli::command()
        .get_subcommands()
        .any(|command| command.get_name() == value)
}

fn print_refresh_stats(label: &str, stats: &decrypt::DecryptStats) -> Result<(), String> {
    print_output(format_args!(
        "{}: {} succeeded, {} failed, {} skipped, {} total",
        label, stats.success, stats.failed, stats.skipped, stats.total
    ))
}

fn ensure_message_cache_ready(decrypted_dir: &Path, jobs: usize) -> Result<(), String> {
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
            return Err(format!(
                "Cannot read messages because the decrypted cache is still incomplete after refreshing keys. Failed: {}",
                cache::message_failure_summary(&stats)
            ));
        }
        Ok(_) => {}
        Err(e) if decrypt::is_refresh_lock_busy_error(&e) => {
            log::warn!(
                "Decrypted cache refresh is already running; reading the existing decrypted cache."
            );
        }
        Err(e) => {
            return Err(format!("Cannot refresh decrypted message cache: {e}"));
        }
    }

    Ok(())
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

#[derive(Args)]
struct DecryptedArgs {
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
}

#[derive(Subcommand)]
enum Commands {
    /// Refresh DB encryption keys from cached or captured account material
    Keys {
        /// Timeout in seconds
        #[arg(
            long,
            default_value = "30",
            default_value_if("method", "login", "180"),
            default_value_if("method", "lldb-login", "180"),
            default_value_if("method", "lldb-cold", "180"),
            default_value_if("method", "gdb-login", "180")
        )]
        timeout: u64,
        /// Key extraction method
        #[arg(
            long,
            value_enum,
            default_value_t = scanner::KeyExtractionMethod::Auto
        )]
        method: scanner::KeyExtractionMethod,
        /// Number of parallel key-derivation jobs (0 = auto)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
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
        /// Number of top sessions to show
        #[arg(long, default_value_t = 30)]
        top: usize,
        #[command(flatten)]
        cache: DecryptedArgs,
    },
    /// List sessions with unread messages
    Unread {
        /// Optional display name or username query to filter unread sessions
        query: Option<String>,
        /// Number of top unread sessions to show
        #[arg(long, default_value_t = 30)]
        top: usize,
        #[command(flatten)]
        cache: DecryptedArgs,
    },
    /// Diagnose tg setup and optionally a specific chat
    Doctor {
        /// Optional session username (tgid_xxx) or display name to inspect
        session: Option<String>,
        #[command(flatten)]
        cache: DecryptedArgs,
    },
    /// Refresh decrypted cache, refreshing keys if needed
    Refresh {
        /// Extract keys before decrypting
        #[arg(long)]
        keys: bool,
        #[command(flatten)]
        cache: DecryptedArgs,
    },
    /// Read messages from a specific session
    Messages {
        /// Session username (tgid_xxx) or display name to search
        session: String,
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
        #[command(flatten)]
        cache: DecryptedArgs,
    },
    /// Search across all sessions
    Search {
        /// Search query
        query: String,
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
        #[command(flatten)]
        cache: DecryptedArgs,
    },
    /// Search message tables with fixed, parameterized query templates
    Query {
        /// Optional session username (tgid_xxx) or display name to search within
        #[arg(long)]
        session: Option<String>,
        /// Keyword that must appear in message text; repeat for multiple keywords
        #[arg(long = "contains")]
        contains: Vec<String>,
        /// Keyword that must not appear in message text; repeat for multiple keywords
        #[arg(long = "not")]
        not_contains: Vec<String>,
        /// Keyword that must appear in the raw database message body; repeat for multiple keywords
        #[arg(long = "raw-contains")]
        raw_contains: Vec<String>,
        /// Require a structured media type: voice, image, sticker, file, or video
        #[arg(long = "has", value_enum)]
        has: Vec<query::QueryMediaType>,
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
        #[arg(long, value_enum, default_value = "newest")]
        order: query::QuerySort,
        /// Keyword matching mode when --contains is repeated: all or any
        #[arg(long, value_enum, default_value = "all")]
        match_mode: query::QueryMatchMode,
        /// Output fields: time,session,sender,type,body,raw_body,timestamp
        #[arg(long, default_value = "time,session,sender,body")]
        fields: String,
        /// Output format: table or json
        #[arg(long, value_enum, default_value = "table")]
        format: query::QueryOutputFormat,
        /// Maximum displayed characters per text cell
        #[arg(long, default_value_t = 500)]
        max_cell_chars: usize,
        /// Use public names instead of your contact remarks in output
        #[arg(long)]
        anonymous: bool,
        #[command(flatten)]
        cache: DecryptedArgs,
    },
    /// Export a complete chat archive
    Export {
        /// Session username (tgid_xxx) or display name to search
        session: String,
        /// Output directory
        #[arg(
            long,
            default_value = "exported",
            value_hint = clap::ValueHint::DirPath
        )]
        output: PathBuf,
        #[command(flatten)]
        cache: DecryptedArgs,
    },
    /// Generate shell completion scripts
    Completions {
        /// Shell to generate completions for
        shell: CompletionShell,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum CompletionShell {
    Fish,
    Zsh,
    Bash,
}

fn main() {
    logger::init();
    let cli = Cli::parse_from(normalize_args_for_default_messages(std::env::args_os()));

    if let Err(error) = run(cli.command) {
        log::error!("{error}");
        std::process::exit(1);
    }
}

fn run(command: Commands) -> Result<(), String> {
    match command {
        Commands::Keys {
            timeout,
            method,
            jobs,
        } => {
            let path = scanner::extract_keys_with_method_and_jobs(timeout, method, jobs)
                .map_err(|e| format!("Error: {e}"))?;
            print_output(format_args!("Keys saved to: {path}"))
        }
        Commands::Decrypt {
            keys,
            output,
            db_dir,
            full,
            since,
            verbose,
            jobs,
        } => {
            let since_ts = time::parse_since_opt(since.as_deref())
                .map_err(|e| format!("Error parsing --since: {e}"))?;
            let config = decrypt::DecryptConfig {
                incremental: !full,
                since: since_ts,
                scope: decrypt::DecryptScope::All,
                recent_output_grace: None,
                quiet: !verbose,
                jobs,
            };
            let stats = decrypt::decrypt_all(&keys, &output, db_dir.as_deref(), &config)
                .map_err(|e| format!("Error: {e}"))?;
            if verbose {
                if stats.skipped > 0 {
                    log::info!(
                        "Decryption complete: {} succeeded, {} failed, {} skipped, {} total",
                        stats.success,
                        stats.failed,
                        stats.skipped,
                        stats.total
                    );
                } else {
                    log::info!(
                        "Decryption complete: {} succeeded, {} failed, {} total",
                        stats.success,
                        stats.failed,
                        stats.total
                    );
                }
            }
            Ok(())
        }
        Commands::Sessions { query, top, cache } => {
            let DecryptedArgs {
                decrypted_dir,
                jobs,
            } = cache;
            let refresh = cache::refresh_session_decrypted(&decrypted_dir, jobs);
            if let Some(error) = cache::session_refresh_error(&refresh) {
                log::warn!(
                    "Session cache refresh had issues ({}). Using existing session cache.",
                    error
                );
            }
            let sessions = db::list_sessions(&decrypted_dir, top, query.as_deref())
                .map_err(|e| format!("Error: {e}"))?;
            if sessions.is_empty() {
                if let Some(query) = query.as_deref().filter(|value| !value.trim().is_empty()) {
                    print_output(format_args!("No sessions matched '{}'.", query))?;
                } else {
                    print_output(format_args!(
                        "No sessions found. Try running 'decrypt' first."
                    ))?;
                }
            }
            Ok(())
        }
        Commands::Unread { query, top, cache } => {
            let DecryptedArgs {
                decrypted_dir,
                jobs,
            } = cache;
            let refresh = cache::refresh_session_decrypted(&decrypted_dir, jobs);
            if let Some(error) = cache::session_refresh_error(&refresh) {
                log::warn!(
                    "Session cache refresh had issues ({}). Using existing session cache.",
                    error
                );
            }
            let sessions = db::list_unread_sessions(&decrypted_dir, top, query.as_deref())
                .map_err(|e| format!("Error: {e}"))?;
            if sessions.is_empty() {
                if let Some(query) = query.as_deref().filter(|value| !value.trim().is_empty()) {
                    print_output(format_args!("No unread sessions matched '{}'.", query))?;
                } else {
                    print_output(format_args!("No unread sessions."))?;
                }
            }
            Ok(())
        }
        Commands::Doctor { session, cache } => {
            let DecryptedArgs {
                decrypted_dir,
                jobs,
            } = cache;
            doctor::run(doctor::DoctorOptions {
                session: session.as_deref(),
                decrypted_dir: &decrypted_dir,
                jobs,
            })
            .map_err(|e| format!("Error: {e}"))
        }
        Commands::Refresh { keys, cache } => {
            let DecryptedArgs {
                decrypted_dir,
                jobs,
            } = cache;
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
            let stats = refresh.map_err(|e| format!("Error: {e}"))?;
            print_refresh_stats("Refresh complete", &stats)
        }
        Commands::Messages {
            session,
            limit,
            offset,
            search,
            since,
            all_time,
            tail,
            head,
            time_bucket,
            anonymous,
            cache,
        } => {
            let DecryptedArgs {
                decrypted_dir,
                jobs,
            } = cache;
            let since_ts = time::parse_since_opt(since.as_deref())
                .map_err(|e| format!("Error parsing --since: {e}"))?;
            let time_bucket = time::parse_message_time_bucket(&time_bucket)
                .map_err(|e| format!("Error parsing --time-bucket: {e}"))?;
            let limit = messages_limit_or_default(limit, since_ts, all_time);
            let use_tail = tail || (!head && offset == 0);
            ensure_message_cache_ready(&decrypted_dir, jobs)?;
            let msg_count = db::read_messages(
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
            .map_err(|e| format!("Error: {e}"))?;
            if msg_count == 0 {
                print_output(format_args!(
                    "No messages found for '{}'. Use 'sessions' to list available sessions.",
                    session
                ))?;
            }
            Ok(())
        }
        Commands::Search {
            query,
            limit,
            since,
            all_time,
            anonymous,
            cache,
        } => {
            let DecryptedArgs {
                decrypted_dir,
                jobs,
            } = cache;
            if limit == 0 {
                return Err("Error: --limit must be greater than 0".to_string());
            }
            let since_ts = time::parse_since_opt(since.as_deref())
                .map_err(|e| format!("Error parsing --since: {e}"))?;
            let since_ts = if all_time {
                since_ts
            } else {
                Some(since_ts.unwrap_or_else(time::default_recent_since))
            };
            let refresh = cache::refresh_search_decrypted(&decrypted_dir, jobs);
            if cache::needs_search_refresh_warning(&refresh) {
                log::warn!(
                    "Decrypted cache refresh had issues ({}). Search results may be stale.",
                    cache::search_refresh_reason(&refresh)
                );
            }
            let count = db::search_messages(
                &decrypted_dir,
                db::SearchMessagesOptions {
                    query: &query,
                    limit,
                    since: since_ts,
                    name_mode: if anonymous {
                        contact::DisplayNameMode::Anonymous
                    } else {
                        contact::DisplayNameMode::PersonalRemark
                    },
                },
            )
            .map_err(|e| format!("Error: {e}"))?;
            if count == 0 {
                print_output(format_args!("No messages found for '{}'.", query))?;
            }
            Ok(())
        }
        Commands::Query {
            session,
            contains,
            not_contains,
            raw_contains,
            has,
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
            cache,
        } => {
            let DecryptedArgs {
                decrypted_dir,
                jobs,
            } = cache;
            let since_ts = time::parse_since_opt(since.as_deref())
                .map_err(|e| format!("Error parsing --since: {e}"))?;
            let since_ts = if all_time {
                since_ts
            } else {
                Some(since_ts.unwrap_or_else(time::default_recent_since))
            };
            let until_ts = time::parse_since_opt(until.as_deref())
                .map_err(|e| format!("Error parsing --until: {e}"))?;
            let fields = query::QueryFields::parse(&fields)
                .map_err(|e| format!("Error parsing --fields: {e}"))?;
            let query_options = query::QueryOptions {
                decrypted_dir: &decrypted_dir,
                session: session.as_deref(),
                contains: &contains,
                not_contains: &not_contains,
                raw_contains: &raw_contains,
                has: &has,
                since: since_ts,
                until: until_ts,
                limit,
                offset,
                sort: order,
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
            };
            let refresh = cache::refresh_message_decrypted(&decrypted_dir, jobs);
            if cache::needs_message_key_retry(&refresh) {
                log::warn!(
                    "Decrypted message cache refresh had issues ({}). Query results may be stale.",
                    cache::retry_reason(&refresh)
                );
            }
            if query::run(query_options).map_err(|e| format!("Error: {e}"))? == 0 {
                print_output(format_args!("No rows returned."))?;
            }
            Ok(())
        }
        Commands::Export {
            session,
            output,
            cache,
        } => {
            let DecryptedArgs {
                decrypted_dir,
                jobs,
            } = cache;
            let mut refresh = cache::refresh_decrypted(&decrypted_dir, jobs);
            if cache::needs_export_key_retry(&refresh) {
                log::warn!(
                    "Decrypted cache refresh had issues ({}). Refreshing keys and retrying export refresh.",
                    cache::export_refresh_error(&refresh).unwrap_or_else(|| "unknown".to_string())
                );
                refresh = cache::refresh_keys_and_decrypted(&decrypted_dir, jobs);
            }
            if let Some(error) = cache::export_refresh_error(&refresh) {
                return Err(format!(
                    "Error: export needs a complete decrypted cache, but refresh is incomplete: {error}"
                ));
            }
            let paths = export::export_messages(export::MessageExportConfig {
                decrypted_dir: &decrypted_dir,
                session_query: &session,
                output_dir: &output,
                jobs,
            })
            .map_err(|e| format!("Error: {e}"))?;
            print_output(format_args!("Exported to:"))?;
            for (format, path) in paths {
                print_output(format_args!("  [{}] {}", format, path.display()))?;
            }
            Ok(())
        }
        Commands::Completions { shell } => {
            completion::print_script(shell).map_err(|e| format!("Error: {e}"))
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
            "unread",
            "messages",
            "search",
            "query",
            "sql",
            "export",
            "doctor",
            "refresh",
            "completions",
        ] {
            assert_eq!(
                normalize_args_for_default_messages(args(&["tg", command])),
                args(&["tg", command])
            );
        }
    }

    #[test]
    fn retired_media_commands_are_not_rewritten_as_session_names() {
        for command in ["image", "file", "sticker", "emoji", "voice"] {
            let argv = args(&["tg", command]);
            assert_eq!(normalize_args_for_default_messages(argv.clone()), argv);
            assert_eq!(
                Cli::try_parse_from(argv).err().unwrap().kind(),
                clap::error::ErrorKind::InvalidSubcommand
            );
        }
    }

    #[test]
    fn removed_dynamic_completion_command_is_unknown() {
        let argv = args(&["tg", "__complete"]);
        assert_eq!(normalize_args_for_default_messages(argv.clone()), argv);
        assert_eq!(
            Cli::try_parse_from(argv).err().unwrap().kind(),
            clap::error::ErrorKind::InvalidSubcommand
        );
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
    fn keys_defaults_to_auto_material_reuse_and_accepts_job_count() {
        let cli = Cli::parse_from(args(&["tg", "keys", "--jobs", "3"]));
        match cli.command {
            Commands::Keys {
                timeout,
                method,
                jobs,
            } => {
                assert_eq!(timeout, 30);
                assert!(matches!(method, scanner::KeyExtractionMethod::Auto));
                assert_eq!(jobs, 3);
            }
            _ => panic!("expected keys command"),
        }

        let cli = Cli::parse_from(args(&["tg", "keys", "--method", "lldb-login"]));
        match cli.command {
            Commands::Keys { timeout, .. } => assert_eq!(timeout, 180),
            _ => panic!("expected keys command"),
        }

        let cli = Cli::parse_from(args(&["tg", "keys", "--method", "lldb-cold"]));
        match cli.command {
            Commands::Keys { timeout, .. } => assert_eq!(timeout, 180),
            _ => panic!("expected keys command"),
        }

        let cli = Cli::parse_from(args(&["tg", "keys", "--method", "login"]));
        match cli.command {
            Commands::Keys {
                timeout, method, ..
            } => {
                assert_eq!(timeout, 180);
                assert!(matches!(method, scanner::KeyExtractionMethod::Login));
            }
            _ => panic!("expected keys command"),
        }

        let cli = Cli::parse_from(args(&["tg", "keys", "--method", "gdb-login"]));
        match cli.command {
            Commands::Keys { timeout, .. } => assert_eq!(timeout, 180),
            _ => panic!("expected keys command"),
        }
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
    }

    #[test]
    fn export_rejects_filter_and_format_options() {
        for option in [
            "--anonymous",
            "--format",
            "--media-dir",
            "--since",
            "--limit",
            "--all-time",
        ] {
            let mut argv = args(&["tg", "export", "room", option]);
            if matches!(option, "--format" | "--media-dir" | "--since" | "--limit") {
                argv.push("value".into());
            }
            assert!(Cli::try_parse_from(argv).is_err());
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
    fn unread_accepts_optional_query() {
        let cli = Cli::parse_from(args(&["tg", "unread", "alice"]));
        match cli.command {
            Commands::Unread { query, .. } => assert_eq!(query.as_deref(), Some("alice")),
            _ => panic!("expected unread command"),
        }
    }

    #[test]
    fn query_accepts_structured_filters() {
        let cli = Cli::parse_from(args(&[
            "tg",
            "query",
            "--contains",
            "needle",
            "--raw-contains",
            "<appmsg",
            "--has",
            "voice",
            "--fields",
            "time,body",
        ]));
        match cli.command {
            Commands::Query {
                contains,
                raw_contains,
                has,
                fields,
                ..
            } => {
                assert_eq!(contains, vec!["needle"]);
                assert_eq!(raw_contains, vec!["<appmsg"]);
                assert_eq!(has, vec![query::QueryMediaType::Voice]);
                assert_eq!(fields, "time,body");
            }
            _ => panic!("expected query command"),
        }
    }
}
