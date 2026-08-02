use std::{
    fs, io,
    path::{Path, PathBuf},
};

use crate::dictionary;

const HOME_ENV: &str = "TG_HOME";
const KEYS_FILE: &str = "all_keys.json";
const KEY_MATERIAL_FILE: &str = "key_material.bin";

pub(crate) fn default_state_dir() -> PathBuf {
    if let Some(path) = std::env::var_os(HOME_ENV).filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }

    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(home).join(".tg");
    }

    PathBuf::from(".tg")
}

pub(crate) fn default_keys_path() -> PathBuf {
    default_state_dir().join(KEYS_FILE)
}

pub(crate) fn default_key_material_path() -> PathBuf {
    default_state_dir().join(KEY_MATERIAL_FILE)
}

pub(crate) fn key_recovery_command(db_dir: &Path, keys_path: &Path) -> String {
    fn quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }
    format!(
        "tg keys --db-dir {} --output {}",
        quote(db_dir),
        quote(keys_path)
    )
}

pub(crate) fn find_db_storage_dir(home: &Path) -> Option<PathBuf> {
    for account_files_dir in dictionary::account_files_candidate_dirs(home) {
        let Ok(accounts) = fs::read_dir(&account_files_dir) else {
            continue;
        };
        for account in accounts.flatten().filter(|entry| entry.path().is_dir()) {
            let account = account.path();
            let direct = account.join("db_storage");
            if direct.is_dir() {
                return Some(direct);
            }
            let Ok(children) = fs::read_dir(account) else {
                continue;
            };
            for child in children.flatten().filter(|entry| entry.path().is_dir()) {
                let nested = child.path().join("db_storage");
                if nested.is_dir() {
                    return Some(nested);
                }
            }
        }
    }

    let support_dir = dictionary::app_support_dir(home);
    let Ok(versions) = fs::read_dir(support_dir) else {
        return None;
    };
    for version in versions.flatten().filter(|entry| entry.path().is_dir()) {
        let Ok(accounts) = fs::read_dir(version.path()) else {
            continue;
        };
        for account in accounts.flatten().filter(|entry| entry.path().is_dir()) {
            let db_storage = account.path().join("db_storage");
            if db_storage.is_dir() {
                return Some(db_storage);
            }
        }
    }
    None
}

pub(crate) fn ensure_private_dir(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    match fs::create_dir(path) {
        Ok(()) => {
            restrict_dir_permissions(path)?;
            restore_invoking_user_ownership(path)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
                _ => Err(error),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty());
            match parent {
                Some(parent) => {
                    ensure_private_dir(parent)?;
                    ensure_private_dir(path)
                }
                None => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
pub(crate) fn restore_invoking_user_ownership(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::chown;

    let Some(uid) = std::env::var("SUDO_UID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
    else {
        return Ok(());
    };
    let Some(gid) = std::env::var("SUDO_GID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
    else {
        return Ok(());
    };
    chown(path, Some(uid), Some(gid))
}

#[cfg(not(unix))]
pub(crate) fn restore_invoking_user_ownership(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_dir_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_dir_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_command_shell_quotes_custom_paths() {
        assert_eq!(
            key_recovery_command(Path::new("/tmp/db dir's"), Path::new("/tmp/key file")),
            "tg keys --db-dir '/tmp/db dir'\\''s' --output '/tmp/key file'"
        );
    }

    #[test]
    fn empty_relative_parent_is_the_current_directory() {
        assert!(ensure_private_dir(Path::new("")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn private_dir_preserves_existing_parent_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("shared");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        let child = parent.join("private/nested");

        ensure_private_dir(&child).unwrap();

        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(&child).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let link = root.path().join("linked");
        std::os::unix::fs::symlink(&parent, &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());
    }
}
