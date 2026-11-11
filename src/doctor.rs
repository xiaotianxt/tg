use std::path::{Path, PathBuf};

use crate::database_keys::{self, DatabaseKeys, SourceDatabase};
use crate::source_store::SourceStore;
use crate::{db, paths, scanner};

pub(crate) struct DoctorOptions<'a> {
    pub session: Option<&'a str>,
    pub db_dir: Option<&'a Path>,
    pub keys_path: &'a Path,
    pub jobs: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckStatus {
    Ok,
    Missing,
    Error,
}

/// One diagnostic line. Checks without a status are counts or facts.
#[derive(Debug, serde::Serialize)]
pub(crate) struct DoctorCheck {
    pub label: &'static str,
    pub status: Option<CheckStatus>,
    pub detail: Option<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub(crate) enum SessionDiagnosis {
    Resolved(db::SessionProbe),
    Failed {
        error: String,
    },
    /// The source store could not be opened, so no probe ran.
    Unavailable,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct SessionCheck {
    pub query: String,
    pub diagnosis: SessionDiagnosis,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
    pub session: Option<SessionCheck>,
}

impl DoctorReport {
    fn push(&mut self, label: &'static str, status: Option<CheckStatus>, detail: Option<String>) {
        self.checks.push(DoctorCheck {
            label,
            status,
            detail,
        });
    }

    fn status(&mut self, label: &'static str, status: CheckStatus, detail: Option<String>) {
        self.push(label, Some(status), detail);
    }

    fn fact(&mut self, label: &'static str, detail: String) {
        self.push(label, None, Some(detail));
    }
}

pub(crate) fn run(options: DoctorOptions<'_>) -> DoctorReport {
    let mut report = DoctorReport {
        checks: Vec::new(),
        session: None,
    };
    check_process(&mut report);
    check_tool(
        &mut report,
        "ffmpeg",
        crate::media_decrypt::resolve_ffmpeg(),
    );
    check_tool(
        &mut report,
        "voice decoder",
        crate::export::resolve_voice_decoder(),
    );
    let keys = check_keys(&mut report, options.keys_path);
    let root = check_source(&mut report, options.db_dir, keys.as_ref());
    drop(keys);
    let store = root
        .as_deref()
        .and_then(|root| check_direct(&mut report, root, options.keys_path));

    if let Some(session) = options.session {
        let diagnosis = match store.as_ref() {
            Some(store) => match db::probe_session(store, session, options.jobs) {
                Ok(probe) => SessionDiagnosis::Resolved(probe),
                Err(error) => SessionDiagnosis::Failed { error },
            },
            None => SessionDiagnosis::Unavailable,
        };
        report.session = Some(SessionCheck {
            query: session.to_string(),
            diagnosis,
        });
    }
    report
}

fn check_process(report: &mut DoctorReport) {
    match scanner::telegram_pid() {
        Ok(pid) => report.status(
            "Telegram process",
            CheckStatus::Ok,
            Some(format!("pid {pid}")),
        ),
        Err(error) => report.status("Telegram process", CheckStatus::Missing, Some(error)),
    }
}

/// Media conversion runs these tools; resolving them here uses the same
/// lookup, so a service's PATH problem shows up before a conversion fails.
fn check_tool(report: &mut DoctorReport, label: &'static str, path: Result<PathBuf, String>) {
    match path {
        Ok(path) => report.status(label, CheckStatus::Ok, Some(path.display().to_string())),
        Err(error) => report.status(label, CheckStatus::Missing, Some(error)),
    }
}

fn check_keys(report: &mut DoctorReport, path: &Path) -> Option<DatabaseKeys> {
    match database_keys::load_key_file(path) {
        Ok(Some(keys)) => {
            let usable = usable_key_count(&keys);
            if usable == 0 {
                report.status(
                    "keys",
                    CheckStatus::Error,
                    Some(format!("0 usable keys in {}", path.display())),
                );
            } else {
                report.status(
                    "keys",
                    CheckStatus::Ok,
                    Some(format!("{usable} keys in {}", path.display())),
                );
            }
            Some(keys)
        }
        Ok(None) => {
            report.status(
                "keys",
                CheckStatus::Missing,
                Some(path.display().to_string()),
            );
            None
        }
        Err(error) => {
            report.status("keys", CheckStatus::Error, Some(error));
            None
        }
    }
}

fn check_source(
    report: &mut DoctorReport,
    requested: Option<&Path>,
    keys: Option<&DatabaseKeys>,
) -> Option<PathBuf> {
    let root = match requested {
        Some(path) if path.is_dir() => Some(path.to_path_buf()),
        Some(path) => {
            report.status(
                "source db dir",
                CheckStatus::Missing,
                Some(path.display().to_string()),
            );
            None
        }
        None => std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .and_then(|home| paths::find_db_storage_dir(&home)),
    };
    let Some(root) = root else {
        if requested.is_none() {
            report.status(
                "source db dir",
                CheckStatus::Missing,
                Some("auto-detection failed".to_string()),
            );
        }
        return None;
    };

    report.status(
        "source db dir",
        CheckStatus::Ok,
        Some(root.display().to_string()),
    );
    match database_keys::collect_source_databases(&root) {
        Ok(files) => {
            let message_files = numbered_message_sources(&files);
            report.fact(
                "source numbered message dbs",
                message_files.len().to_string(),
            );
            let coverage = match keys {
                Some(keys) => {
                    let covered = message_files
                        .iter()
                        .filter(|file| has_usable_key(keys, &file.rel_path))
                        .count();
                    format!("{covered}/{} numbered dbs", message_files.len())
                }
                None => "unknown".to_string(),
            };
            report.fact("message key coverage", coverage);
            let hot_wals = message_files
                .iter()
                .filter(|file| wal_size(&file.full_path).is_some_and(|size| size > 0))
                .count();
            report.fact("live message WAL files", hot_wals.to_string());
        }
        Err(error) => report.status("source catalog", CheckStatus::Error, Some(error)),
    }
    Some(root)
}

fn check_direct(report: &mut DoctorReport, root: &Path, keys_path: &Path) -> Option<SourceStore> {
    let store = match SourceStore::discover(Some(root), keys_path) {
        Ok(store) => store,
        Err(error) => {
            report.status("direct source", CheckStatus::Error, Some(error));
            return None;
        }
    };
    report.status("direct source", CheckStatus::Ok, None);
    check_database(report, &store, "session/session.db", "session db");
    check_database(report, &store, "message/message_fts.db", "message FTS");
    Some(store)
}

fn check_database(report: &mut DoctorReport, store: &SourceStore, path: &str, label: &'static str) {
    if !db::has_database(store, path) {
        return report.status(label, CheckStatus::Missing, None);
    }
    match store.open(path) {
        Ok(_) => report.status(label, CheckStatus::Ok, None),
        Err(error) => report.status(label, CheckStatus::Error, Some(error)),
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
