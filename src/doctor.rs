use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::{db, decrypt, output::Output, paths, scanner};

const PATH_LIST_LIMIT: usize = 20;
const HOT_WAL_LIST_LIMIT: usize = 8;

pub(crate) struct DoctorOptions<'a> {
    pub session: Option<&'a str>,
    pub decrypted_dir: &'a Path,
    pub jobs: usize,
}

pub(crate) fn run(options: DoctorOptions<'_>) -> Result<(), String> {
    let stdout = std::io::stdout();
    let mut out = Output::new(stdout.lock());

    out.line(format_args!("tg doctor"))?;
    out.blank_line()?;

    write_process_status(&mut out)?;
    out.line(format_args!("scanner: OK (embedded in tg)"))?;
    let keys = write_keys_status(&mut out)?;
    write_cache_status(&mut out, options.decrypted_dir, keys.as_ref())?;

    if let Some(session) = options.session {
        out.blank_line()?;
        write_session_status(&mut out, options.decrypted_dir, session, options.jobs)?;
    }

    out.flush()
}

fn write_process_status<W: std::io::Write>(out: &mut Output<W>) -> Result<(), String> {
    match scanner::telegram_pid() {
        Ok(pid) => out.line(format_args!("Telegram process: OK (pid {})", pid)),
        Err(e) => out.line(format_args!("Telegram process: MISSING ({})", e)),
    }
}

fn write_keys_status<W: std::io::Write>(
    out: &mut Output<W>,
) -> Result<Option<decrypt::DatabaseKeys>, String> {
    let path = paths::default_keys_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let keys = match serde_json::from_str::<decrypt::DatabaseKeys>(&content) {
                Ok(keys) => keys,
                Err(e) => {
                    out.line(format_args!("keys: ERROR ({}: {})", path.display(), e))?;
                    return Ok(None);
                }
            };
            let key_count = usable_key_count(&keys);
            if key_count == 0 {
                out.line(format_args!(
                    "keys: ERROR (0 usable keys in {}; rerun key extraction)",
                    path.display()
                ))?;
            } else {
                out.line(format_args!(
                    "keys: OK ({} keys in {})",
                    key_count,
                    path.display()
                ))?;
            }
            Ok(Some(keys))
        }
        Err(_) => {
            out.line(format_args!("keys: MISSING ({})", path.display()))?;
            Ok(None)
        }
    }
}

fn write_cache_status<W: std::io::Write>(
    out: &mut Output<W>,
    decrypted_dir: &Path,
    keys: Option<&decrypt::DatabaseKeys>,
) -> Result<(), String> {
    match decrypt::auto_detect_db_dir() {
        Some(source_dir) => {
            out.line(format_args!("source db dir: OK ({})", source_dir.display()))?;
            let source_files = decrypt::collect_db_files(&source_dir);
            let source_message_dbs = numbered_message_sources(&source_files);
            out.line(format_args!(
                "source numbered message dbs: {}",
                source_message_dbs.len()
            ))?;

            match keys {
                Some(keys) => {
                    let missing = missing_message_key_paths(&source_message_dbs, keys);
                    let covered = source_message_dbs.len().saturating_sub(missing.len());
                    out.line(format_args!(
                        "message key coverage: {}/{} numbered dbs",
                        covered,
                        source_message_dbs.len()
                    ))?;
                    if missing.is_empty() {
                        out.line(format_args!("missing message keys: none"))?;
                    } else {
                        out.line(format_args!(
                            "missing message keys: {}",
                            format_path_list(&missing)
                        ))?;
                    }
                }
                None => {
                    out.line(format_args!(
                        "message key coverage: unknown (keys unavailable)"
                    ))?;
                    out.line(format_args!("missing message keys: unknown"))?;
                }
            }

            write_hot_wal_status(out, &source_message_dbs)?;
        }
        None => {
            out.line(format_args!("source db dir: MISSING (auto-detect failed)"))?;
            out.line(format_args!("source numbered message dbs: unknown"))?;
            out.line(format_args!("message key coverage: unknown"))?;
            out.line(format_args!("missing message keys: unknown"))?;
            out.line(format_args!("hot WAL files: unknown"))?;
        }
    }

    if decrypted_dir.is_dir() {
        out.line(format_args!(
            "decrypted cache: OK ({})",
            decrypted_dir.display()
        ))?;
    } else {
        out.line(format_args!(
            "decrypted cache: MISSING ({})",
            decrypted_dir.display()
        ))?;
    }

    let (contact_db, message_dbs) = db::find_decrypted_dbs(decrypted_dir);
    match contact_db {
        Some(path) => out.line(format_args!("contact db: OK ({})", path.display()))?,
        None => out.line(format_args!("contact db: MISSING"))?,
    }
    out.line(format_args!(
        "decrypted numbered message dbs: {}",
        message_dbs.len()
    ))
}

fn numbered_message_sources(files: &[decrypt::SourceDbFile]) -> Vec<&decrypt::SourceDbFile> {
    let mut message_dbs: Vec<&decrypt::SourceDbFile> = files
        .iter()
        .filter(|file| is_numbered_message_rel_path(&file.rel_path))
        .collect();
    message_dbs.sort_by(|a, b| {
        message_db_index(&a.rel_path)
            .cmp(&message_db_index(&b.rel_path))
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
    message_dbs
}

fn is_numbered_message_rel_path(rel_path: &str) -> bool {
    let path = Path::new(rel_path);
    let in_message_dir = path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        == Some("message");
    let is_numbered = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(db::is_message_db_name);
    in_message_dir && is_numbered
}

fn message_db_index(rel_path: &str) -> Option<u32> {
    Path::new(rel_path)
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("message_"))
        .and_then(|name| name.strip_suffix(".db"))
        .and_then(|index| index.parse::<u32>().ok())
}

fn missing_message_key_paths(
    source_message_dbs: &[&decrypt::SourceDbFile],
    keys: &decrypt::DatabaseKeys,
) -> Vec<String> {
    source_message_dbs
        .iter()
        .filter(
            |file| match decrypt::database_key_entry(keys, &file.rel_path) {
                Some(entry) => !has_usable_enc_key(entry),
                None => true,
            },
        )
        .map(|file| file.rel_path.clone())
        .collect()
}

fn usable_key_count(keys: &decrypt::DatabaseKeys) -> usize {
    keys.values()
        .filter(|entry| has_usable_enc_key(entry))
        .count()
}

fn has_usable_enc_key(entry: &std::collections::HashMap<String, String>) -> bool {
    entry.get("enc_key").is_some_and(|key| !key.is_empty())
}

fn format_path_list(paths: &[String]) -> String {
    let mut summary = paths
        .iter()
        .take(PATH_LIST_LIMIT)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > PATH_LIST_LIMIT {
        summary.push_str(&format!(" ... {} total", paths.len()));
    }
    summary
}

struct HotWalFile {
    rel_path: String,
    size: u64,
    modified: Option<SystemTime>,
}

fn write_hot_wal_status<W: std::io::Write>(
    out: &mut Output<W>,
    source_message_dbs: &[&decrypt::SourceDbFile],
) -> Result<(), String> {
    let hot_wals = collect_hot_wal_files(source_message_dbs);

    out.line(format_args!(
        "hot WAL files: {} (not replayed by tg decrypt)",
        hot_wals.len()
    ))?;
    for wal in hot_wals.iter().take(HOT_WAL_LIST_LIMIT) {
        out.line(format_args!(
            "  {} ({} bytes, {})",
            wal.rel_path,
            wal.size,
            format_mtime_and_age(wal.modified)
        ))?;
    }
    if hot_wals.len() > HOT_WAL_LIST_LIMIT {
        out.line(format_args!("  ... {} total", hot_wals.len()))?;
    }
    Ok(())
}

fn collect_hot_wal_files(source_message_dbs: &[&decrypt::SourceDbFile]) -> Vec<HotWalFile> {
    let mut hot_wals = Vec::new();
    for source in source_message_dbs {
        let wal_path = decrypt::sqlite_sidecar_path(&source.full_path, "-wal");
        let Ok(meta) = std::fs::metadata(&wal_path) else {
            continue;
        };
        if meta.len() == 0 {
            continue;
        }
        hot_wals.push(HotWalFile {
            rel_path: format!("{}-wal", source.rel_path),
            size: meta.len(),
            modified: meta.modified().ok(),
        });
    }

    hot_wals.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
    hot_wals
}

fn format_mtime_and_age(modified: Option<SystemTime>) -> String {
    let Some(modified) = modified else {
        return "mtime unavailable".to_string();
    };
    let datetime: chrono::DateTime<chrono::Local> = modified.into();
    format!(
        "mtime {}, age {}",
        datetime.format("%Y-%m-%d %H:%M:%S %z"),
        format_age(modified)
    )
}

fn format_age(modified: SystemTime) -> String {
    match SystemTime::now().duration_since(modified) {
        Ok(age) => format_duration(age),
        Err(_) => "in future".to_string(),
    }
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 60 * 60 {
        format!("{}m", secs / 60)
    } else if secs < 24 * 60 * 60 {
        format!("{}h", secs / (60 * 60))
    } else {
        format!("{}d", secs / (24 * 60 * 60))
    }
}

fn write_session_status<W: std::io::Write>(
    out: &mut Output<W>,
    decrypted_dir: &Path,
    session: &str,
    jobs: usize,
) -> Result<(), String> {
    out.line(format_args!("session query: {}", session))?;
    match db::probe_session(decrypted_dir, session, jobs) {
        Ok(probe) => {
            out.line(format_args!(
                "resolved session: {} ({})",
                probe.display_name, probe.username
            ))?;
            out.line(format_args!("message table: {}", probe.table_name))?;
            out.line(format_args!("databases with table: {}", probe.matching_dbs))?;
            out.line(format_args!("messages: {}", probe.message_count))
        }
        Err(e) => out.line(format_args!("session probe: ERROR ({})", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn source(rel_path: &str, full_path: PathBuf) -> decrypt::SourceDbFile {
        decrypt::SourceDbFile {
            rel_path: rel_path.to_string(),
            full_path,
            size: 0,
            freshness_mtime: None,
        }
    }

    fn key_entry(value: &str) -> HashMap<String, String> {
        let mut entry = HashMap::new();
        entry.insert("enc_key".to_string(), value.to_string());
        entry
    }

    #[test]
    fn numbered_message_sources_filter_and_sort_by_index() {
        let files = vec![
            source("message/message_10.db", PathBuf::from("message_10.db")),
            source("message/message_2.db", PathBuf::from("message_2.db")),
            source("message/message_0.db", PathBuf::from("message_0.db")),
            source("message/message_fts.db", PathBuf::from("message_fts.db")),
            source("favorite/message_1.db", PathBuf::from("message_1.db")),
        ];

        let rels: Vec<&str> = numbered_message_sources(&files)
            .iter()
            .map(|file| file.rel_path.as_str())
            .collect();

        assert_eq!(
            rels,
            vec![
                "message/message_0.db",
                "message/message_2.db",
                "message/message_10.db"
            ]
        );
    }

    #[test]
    fn missing_message_key_paths_reports_uncovered_numbered_dbs() {
        let files = vec![
            source("message/message_0.db", PathBuf::from("message_0.db")),
            source("message/message_1.db", PathBuf::from("message_1.db")),
            source("message/message_2.db", PathBuf::from("message_2.db")),
        ];
        let source_message_dbs = numbered_message_sources(&files);
        let mut keys = decrypt::DatabaseKeys::new();
        keys.insert("message/message_0.db".to_string(), key_entry("exact"));
        keys.insert("other/message_2.db".to_string(), key_entry("basename"));

        assert_eq!(
            missing_message_key_paths(&source_message_dbs, &keys),
            vec!["message/message_1.db".to_string()]
        );
    }

    #[test]
    fn missing_message_key_paths_treats_empty_keys_as_missing() {
        let files = vec![source(
            "message/message_0.db",
            PathBuf::from("message_0.db"),
        )];
        let source_message_dbs = numbered_message_sources(&files);
        let mut keys = decrypt::DatabaseKeys::new();
        keys.insert("message/message_0.db".to_string(), key_entry(""));

        assert_eq!(
            missing_message_key_paths(&source_message_dbs, &keys),
            vec!["message/message_0.db".to_string()]
        );
        assert_eq!(usable_key_count(&keys), 0);
    }

    #[test]
    fn collect_hot_wal_files_reports_non_empty_wals() {
        let dir = tempfile::tempdir().unwrap();
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();
        let db_path = message_dir.join("message_0.db");
        let empty_db_path = message_dir.join("message_1.db");
        std::fs::write(&db_path, b"db").unwrap();
        std::fs::write(decrypt::sqlite_sidecar_path(&db_path, "-wal"), b"wal").unwrap();
        std::fs::write(&empty_db_path, b"db").unwrap();
        std::fs::write(decrypt::sqlite_sidecar_path(&empty_db_path, "-wal"), b"").unwrap();
        let files = vec![
            source("message/message_0.db", db_path),
            source("message/message_1.db", empty_db_path),
        ];
        let source_message_dbs = numbered_message_sources(&files);

        let hot_wals = collect_hot_wal_files(&source_message_dbs);

        assert_eq!(hot_wals.len(), 1);
        assert_eq!(hot_wals[0].rel_path, "message/message_0.db-wal");
        assert_eq!(hot_wals[0].size, 3);
    }

    #[test]
    fn duration_format_is_concise() {
        assert_eq!(format_duration(Duration::from_secs(42)), "42s");
        assert_eq!(format_duration(Duration::from_secs(120)), "2m");
        assert_eq!(format_duration(Duration::from_secs(7_200)), "2h");
        assert_eq!(format_duration(Duration::from_secs(172_800)), "2d");
    }
}
