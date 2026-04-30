use std::{
    fs, io,
    path::{Path, PathBuf},
};

const HOME_ENV: &str = "TG_HOME";
const KEYS_FILE: &str = "all_keys.json";

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

pub(crate) fn default_decrypted_dir() -> PathBuf {
    default_state_dir().join("decrypted")
}

pub(crate) fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    restrict_dir_permissions(path);
    Ok(())
}

#[cfg(unix)]
fn restrict_dir_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_dir_permissions(_path: &Path) {}
