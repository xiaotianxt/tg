mod scanner;
mod decrypt;
mod db;
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
        Commands::Decrypt { keys, output, db_dir } => {
            match decrypt::decrypt_all(&keys, &output, db_dir.as_deref()) {
                Ok(stats) => println!("Decryption complete: {} succeeded, {} failed, {} total",
                    stats.success, stats.failed, stats.total),
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
        Commands::Messages { session, decrypted_dir, limit, offset, search } => {
            match db::read_messages(&decrypted_dir, &session, limit, offset, search.as_deref()) {
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
        Commands::Export { session, decrypted_dir, format, output } => {
            match export::export_messages(&decrypted_dir, &session, &format, &output) {
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
