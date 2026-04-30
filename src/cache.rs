use std::{path::Path, time::Duration};

use crate::{db, decrypt, paths, scanner};

const KEY_REFRESH_TIMEOUT_SECS: u64 = 30;
const MESSAGE_AUTO_REFRESH_GRACE: Duration = Duration::from_secs(60);

pub(crate) fn refresh_decrypted(
    decrypted_dir: &Path,
    jobs: usize,
) -> Result<decrypt::DecryptStats, String> {
    let config = decrypt::DecryptConfig {
        incremental: true,
        since: None,
        scope: decrypt::DecryptScope::All,
        recent_output_grace: None,
        quiet: true,
        jobs,
    };
    decrypt::decrypt_all(&paths::default_keys_path(), decrypted_dir, None, &config)
}

pub(crate) fn refresh_message_decrypted(
    decrypted_dir: &Path,
    jobs: usize,
) -> Result<decrypt::DecryptStats, String> {
    let config = decrypt::DecryptConfig {
        incremental: true,
        since: None,
        scope: decrypt::DecryptScope::Messages,
        recent_output_grace: Some(MESSAGE_AUTO_REFRESH_GRACE),
        quiet: true,
        jobs,
    };
    decrypt::decrypt_all(&paths::default_keys_path(), decrypted_dir, None, &config)
}

pub(crate) fn refresh_keys_and_decrypted(
    decrypted_dir: &Path,
    jobs: usize,
) -> Result<decrypt::DecryptStats, String> {
    scanner::extract_keys(KEY_REFRESH_TIMEOUT_SECS)?;
    refresh_decrypted(decrypted_dir, jobs)
}

pub(crate) fn refresh_keys_and_message_decrypted(
    decrypted_dir: &Path,
    jobs: usize,
) -> Result<decrypt::DecryptStats, String> {
    scanner::extract_keys(KEY_REFRESH_TIMEOUT_SECS)?;
    refresh_message_decrypted(decrypted_dir, jobs)
}

pub(crate) fn needs_message_key_retry(refresh: &Result<decrypt::DecryptStats, String>) -> bool {
    match refresh {
        Ok(stats) => failures_can_affect_messages(stats),
        Err(e) => !decrypt::is_refresh_lock_busy_error(e),
    }
}

pub(crate) fn retry_reason(refresh: &Result<decrypt::DecryptStats, String>) -> String {
    match refresh {
        Ok(_) => "contact/message database failed to decrypt".to_string(),
        Err(e) => e.clone(),
    }
}

pub(crate) fn needs_search_refresh_warning(
    refresh: &Result<decrypt::DecryptStats, String>,
) -> bool {
    match refresh {
        Ok(stats) => failures_can_affect_search(stats),
        Err(_) => true,
    }
}

pub(crate) fn search_refresh_reason(refresh: &Result<decrypt::DecryptStats, String>) -> String {
    match refresh {
        Ok(_) => "contact/message/search index database failed to decrypt".to_string(),
        Err(e) => e.clone(),
    }
}

pub(crate) fn failures_can_affect_messages(stats: &decrypt::DecryptStats) -> bool {
    stats
        .failed_paths
        .iter()
        .any(|path| failure_can_affect_messages(path))
}

pub(crate) fn message_failure_summary(stats: &decrypt::DecryptStats) -> String {
    let paths: Vec<&str> = stats
        .failed_paths
        .iter()
        .filter(|path| failure_can_affect_messages(path))
        .map(String::as_str)
        .collect();

    if paths.is_empty() {
        return "none".to_string();
    }

    let mut summary = paths.iter().take(5).copied().collect::<Vec<_>>().join(", ");
    if paths.len() > 5 {
        summary.push_str(&format!(" ... {} total", paths.len()));
    }
    summary
}

pub(crate) fn failures_can_affect_search(stats: &decrypt::DecryptStats) -> bool {
    stats
        .failed_paths
        .iter()
        .any(|path| failure_can_affect_messages(path) || failure_can_affect_telegram_fts(path))
}

pub(crate) fn failures_can_affect_telegram_fts(stats: &decrypt::DecryptStats) -> bool {
    stats
        .failed_paths
        .iter()
        .any(|path| failure_can_affect_telegram_fts(path))
}

fn failure_can_affect_messages(path: &str) -> bool {
    path == "contact/contact.db"
        || path
            .strip_prefix("message/")
            .is_some_and(db::is_message_db_name)
}

fn failure_can_affect_telegram_fts(path: &str) -> bool {
    path == "message/message_fts.db"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats_with_failed_paths(paths: &[&str]) -> decrypt::DecryptStats {
        decrypt::DecryptStats {
            success: 0,
            failed: paths.len(),
            skipped: 0,
            total: paths.len(),
            failed_paths: paths.iter().map(|path| path.to_string()).collect(),
        }
    }

    #[test]
    fn message_retry_considers_contact_and_numbered_message_dbs_relevant() {
        assert!(failures_can_affect_messages(&stats_with_failed_paths(&[
            "contact/contact.db"
        ])));
        assert!(failures_can_affect_messages(&stats_with_failed_paths(&[
            "message/message_0.db"
        ])));
    }

    #[test]
    fn message_retry_ignores_unrelated_decrypt_failures() {
        assert!(!failures_can_affect_messages(&stats_with_failed_paths(&[
            "favorite/favorite.db"
        ])));
        assert!(!failures_can_affect_messages(&stats_with_failed_paths(&[
            "message/message_fts.db"
        ])));
    }

    #[test]
    fn message_retry_ignores_refresh_lock_contention() {
        let refresh = Err("Decrypted cache refresh is already running for decrypted".to_string());
        assert!(!needs_message_key_retry(&refresh));
    }

    #[test]
    fn message_failure_summary_lists_relevant_paths_only() {
        assert_eq!(
            message_failure_summary(&stats_with_failed_paths(&[
                "favorite/favorite.db",
                "message/message_1.db",
                "message/message_fts.db",
                "contact/contact.db",
            ])),
            "message/message_1.db, contact/contact.db"
        );
    }

    #[test]
    fn search_considers_telegram_fts_relevant() {
        assert!(failures_can_affect_search(&stats_with_failed_paths(&[
            "message/message_fts.db"
        ])));
        assert!(failures_can_affect_telegram_fts(&stats_with_failed_paths(
            &["message/message_fts.db"]
        )));
    }
}
