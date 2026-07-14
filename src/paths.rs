use std::{
    fs, io,
    path::{Path, PathBuf},
};

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

pub(crate) fn default_decrypted_dir() -> PathBuf {
    default_state_dir().join("decrypted")
}

pub(crate) fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    restrict_dir_permissions(path)?;
    restore_invoking_user_ownership(path)
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
