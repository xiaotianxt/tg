mod scanner;
mod decrypt;
mod db;
mod message;
mod media;
mod media_index;
mod media_pb;
mod media_decrypt;
mod media_key;
mod export;
mod logger;
mod output;
mod time;
mod parallel;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

fn print_output(args: std::fmt::Arguments<'_>) {
    if let Err(e) = output::stdout_line(args) {
        log::error!("Error: {}", e);
        std::process::exit(1);
    }
}

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
}

fn refresh_decrypted_cache(decrypted_dir: &std::path::Path, jobs: usize) {
    let config = decrypt::DecryptConfig {
        incremental: true,
        since: None,
        quiet: true,
        jobs,
    };
    let _ = decrypt::decrypt_all(
        std::path::Path::new("all_keys.json"),
        decrypted_dir,
        None,
        &config,
    );
}

fn main() {
    logger::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Keys { scanner, timeout } => {
            let scanner_path = scanner.unwrap_or_else(scanner::default_scanner_path);
            match scanner::extract_keys(&scanner_path, timeout) {
                Ok(path) => print_output(format_args!("Keys saved to: {}", path)),
                Err(e) => {
                    log::error!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Decrypt { keys, output, db_dir, incremental: _, full, since, verbose, jobs } => {
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
                            log::info!("Decryption complete: {} succeeded, {} failed, {} total",
                                stats.success, stats.failed, stats.total);
                        }
                    }
                }
                Err(e) => {
                    log::error!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Sessions { decrypted_dir, top, jobs } => {
            refresh_decrypted_cache(&decrypted_dir, jobs);
            match db::list_sessions(&decrypted_dir, top, jobs) {
                Ok(sessions) => {
                    if sessions.is_empty() {
                        print_output(format_args!("No sessions found. Try running 'decrypt' first."));
                    }
                }
                Err(e) => {
                    log::error!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Messages { session, decrypted_dir, limit, offset, search, since, tail, head, jobs } => {
            let since_ts = match time::parse_since_opt(since.as_deref()) {
                Ok(ts) => ts,
                Err(e) => {
                    log::error!("Error parsing --since: {}", e);
                    std::process::exit(1);
                }
            };
            let limit = limit.or_else(|| since_ts.is_none().then_some(50));
            let use_tail = tail || (!head && offset == 0);
            refresh_decrypted_cache(&decrypted_dir, jobs);
            match db::read_messages(&decrypted_dir, &session, limit, offset, search.as_deref(), since_ts, use_tail, jobs) {
                Ok(msg_count) => {
                    if msg_count == 0 {
                        print_output(format_args!(
                            "No messages found for '{}'. Use 'sessions' to list available sessions.",
                            session
                        ));
                    }
                }
                Err(e) => {
                    log::error!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Search { query, decrypted_dir, limit, jobs } => {
            refresh_decrypted_cache(&decrypted_dir, jobs);
            match db::search_messages(&decrypted_dir, &query, limit, jobs) {
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
        Commands::Export { session, decrypted_dir, format, output, media_dir, jobs } => {
            refresh_decrypted_cache(&decrypted_dir, jobs);
            match export::export_messages(&decrypted_dir, &session, &format, &output, media_dir.as_deref(), jobs) {
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
        Commands::Image { session, decrypted_dir, output, list, all, index, limit, since, jobs } => {
            let since_ts = match time::parse_since_opt(since.as_deref()) {
                Ok(ts) => ts,
                Err(e) => {
                    log::error!("Error parsing --since: {}", e);
                    std::process::exit(1);
                }
            };
            refresh_decrypted_cache(&decrypted_dir, jobs);
            let config = export::ImageExportConfig {
                output_dir: &output,
                list,
                all,
                index,
                limit,
                since: since_ts,
                jobs,
            };
            if let Err(e) = export::export_images(&decrypted_dir, &session, config) {
                log::error!("Error: {}", e);
                std::process::exit(1);
            }
        }
    }
}
