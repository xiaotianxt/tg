use std::path::{Path, PathBuf};

use crate::database_keys::{self, DatabaseKeys, SourceDatabase};
use crate::output::Output;
use crate::source_store::SourceStore;
use crate::{db, paths, scanner};

pub(crate) struct DoctorOptions<'a> {
    pub session: Option<&'a str>,
    pub db_dir: Option<&'a Path>,
    pub keys_path: &'a Path,
    pub jobs: usize,
}

pub(crate) fn run(options: DoctorOptions<'_>) -> Result<(), String> {
    let stdout = std::io::stdout();
    let mut out = Output::new(stdout.lock());
    out.line(format_args!("tg doctor"))?;
    out.blank_line()?;
    write_process_status(&mut out)?;

    let keys = write_keys_status(&mut out, options.keys_path)?;
    let root = write_source_status(&mut out, options.db_dir, keys.as_ref())?;
    drop(keys);
    let store = match root.as_deref() {
        Some(root) => write_direct_status(&mut out, root, options.keys_path)?,
        None => None,
    };

    if let Some(session) = options.session {
        out.blank_line()?;
        out.line(format_args!("session query: {session}"))?;
        match store.as_ref() {
            Some(store) => match db::probe_session(store, session, options.jobs) {
                Ok(probe) => {
                    out.line(format_args!(
                        "resolved session: {} ({})",
                        probe.display_name, probe.username
                    ))?;
                    out.line(format_args!("message table: {}", probe.table_name))?;
                    out.line(format_args!(
                        "message rows: {} across {} database(s)",
                        probe.message_count, probe.matching_dbs
                    ))?;
                }
                Err(error) => out.line(format_args!("session probe: ERROR ({error})"))?,
            },
            None => out.line(format_args!("session probe: unavailable"))?,
        }
    }
    out.flush()
}

fn write_process_status<W: std::io::Write>(out: &mut Output<W>) -> Result<(), String> {
    match scanner::telegram_pid() {
        Ok(pid) => out.line(format_args!("Telegram process: OK (pid {pid})")),
        Err(error) => out.line(format_args!("Telegram process: MISSING ({error})")),
    }
}

fn write_keys_status<W: std::io::Write>(
    out: &mut Output<W>,
    path: &Path,
) -> Result<Option<DatabaseKeys>, String> {
    match database_keys::load_key_file(path) {
        Ok(Some(keys)) => {
            let usable = usable_key_count(&keys);
            if usable == 0 {
                out.line(format_args!(
                    "keys: ERROR (0 usable keys in {})",
                    path.display()
                ))?;
            } else {
                out.line(format_args!(
                    "keys: OK ({usable} keys in {})",
                    path.display()
                ))?;
            }
            Ok(Some(keys))
        }
        Ok(None) => {
            out.line(format_args!("keys: MISSING ({})", path.display()))?;
            Ok(None)
        }
        Err(error) => {
            out.line(format_args!("keys: ERROR ({error})"))?;
            Ok(None)
        }
    }
}

fn write_source_status<W: std::io::Write>(
    out: &mut Output<W>,
    requested: Option<&Path>,
    keys: Option<&DatabaseKeys>,
) -> Result<Option<PathBuf>, String> {
    let root = match requested {
        Some(path) if path.is_dir() => Some(path.to_path_buf()),
        Some(path) => {
            out.line(format_args!("source db dir: MISSING ({})", path.display()))?;
            None
        }
        None => std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .and_then(|home| paths::find_db_storage_dir(&home)),
    };
    let Some(root) = root else {
        if requested.is_none() {
            out.line(format_args!(
                "source db dir: MISSING (auto-detection failed)"
            ))?;
        }
        return Ok(None);
    };

    out.line(format_args!("source db dir: OK ({})", root.display()))?;
    match database_keys::collect_source_databases(&root) {
        Ok(files) => {
            let message_files = numbered_message_sources(&files);
            out.line(format_args!(
                "source numbered message dbs: {}",
                message_files.len()
            ))?;
            if let Some(keys) = keys {
                let covered = message_files
                    .iter()
                    .filter(|file| has_usable_key(keys, &file.rel_path))
                    .count();
                out.line(format_args!(
                    "message key coverage: {covered}/{} numbered dbs",
                    message_files.len()
                ))?;
            } else {
                out.line(format_args!("message key coverage: unknown"))?;
            }
            let hot_wals = message_files
                .iter()
                .filter(|file| wal_size(&file.full_path).is_some_and(|size| size > 0))
                .count();
            out.line(format_args!("live message WAL files: {hot_wals}"))?;
        }
        Err(error) => out.line(format_args!("source catalog: ERROR ({error})"))?,
    }
    Ok(Some(root))
}

fn write_direct_status<W: std::io::Write>(
    out: &mut Output<W>,
    root: &Path,
    keys_path: &Path,
) -> Result<Option<SourceStore>, String> {
    let store = match SourceStore::discover(Some(root), keys_path) {
        Ok(store) => store,
        Err(error) => {
            out.line(format_args!("direct source: ERROR ({error})"))?;
            return Ok(None);
        }
    };
    out.line(format_args!("direct source: OK"))?;
    write_database_probe(out, &store, "session/session.db", "session db")?;
    write_database_probe(out, &store, "message/message_fts.db", "message FTS")?;
    Ok(Some(store))
}

fn write_database_probe<W: std::io::Write>(
    out: &mut Output<W>,
    store: &SourceStore,
    path: &str,
    label: &str,
) -> Result<(), String> {
    if !db::has_database(store, path) {
        return out.line(format_args!("{label}: MISSING"));
    }
    match store.open(path) {
        Ok(_) => out.line(format_args!("{label}: OK")),
        Err(error) => out.line(format_args!("{label}: ERROR ({error})")),
    }
}

fn numbered_message_sources(files: &[SourceDatabase]) -> Vec<&SourceDatabase> {
    files
        .iter()
        .filter(|file| {
            file.rel_path
                .strip_prefix("message/")
                .is_some_and(db::is_message_db_name)
        })
        .collect()
}

fn has_usable_key(keys: &DatabaseKeys, path: &str) -> bool {
    keys.get(path)
        .and_then(|fields| fields.get("enc_key"))
        .is_some_and(|key| key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn usable_key_count(keys: &DatabaseKeys) -> usize {
    keys.iter()
        .filter(|(path, _)| has_usable_key(keys, path))
        .count()
}

fn wal_size(path: &Path) -> Option<u64> {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push("-wal");
    std::fs::metadata(PathBuf::from(sidecar))
        .ok()
        .map(|meta| meta.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_sources_exclude_fts_and_media_databases() {
        let files = [
            SourceDatabase {
                rel_path: "message/message_2.db".to_string(),
                full_path: PathBuf::from("message_2.db"),
            },
            SourceDatabase {
                rel_path: "message/message_fts.db".to_string(),
                full_path: PathBuf::from("message_fts.db"),
            },
        ];
        assert_eq!(numbered_message_sources(&files).len(), 1);
    }

    #[test]
    fn usable_keys_require_exact_hex_material() {
        let mut keys = DatabaseKeys::new();
        keys.insert(
            "message/message_0.db".to_string(),
            std::collections::HashMap::from([("enc_key".to_string(), "ab".repeat(32))]),
        );
        keys.insert(
            "message/message_1.db".to_string(),
            std::collections::HashMap::from([("enc_key".to_string(), "bad".to_string())]),
        );
        assert_eq!(usable_key_count(&keys), 1);
    }
}
