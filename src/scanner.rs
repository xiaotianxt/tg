use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
#[cfg(target_os = "macos")]
use std::process::{Output, Stdio};
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

use crate::{database_keys, database_keys::DatabaseKeys, dictionary, key_material, paths};

#[cfg(target_os = "linux")]
mod elf_locator;
#[cfg(target_os = "linux")]
mod gdb;
#[cfg(target_os = "linux")]
mod linux_process;
#[cfg(target_os = "macos")]
mod lldb;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum KeyExtractionMethod {
    Auto,
    #[value(
        name = "login",
        alias = "lldb-login",
        alias = "lldb-cold",
        alias = "gdb-login"
    )]
    Login,
}

fn find_telegram_pid() -> Result<i32, String> {
    let process = dictionary::desktop_app_process();
    let output = Command::new("pgrep")
        .arg("-x")
        .arg(process)
        .output()
        .map_err(|e| format!("Failed to run pgrep: {}", e))?;

    if !output.status.success() {
        return Err("Telegram is not running.".to_string());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pid = stdout
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .parse::<i32>()
        .map_err(|e| format!("Invalid PID: {}", e))?;
    Ok(pid)
}

pub(crate) fn telegram_pid() -> Result<i32, String> {
    find_telegram_pid()
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
        .map(|uid| uid == 0)
        .unwrap_or(false)
}

fn real_home_dir() -> Option<PathBuf> {
    if is_root() {
        if let Ok(sudo_user) = std::env::var("SUDO_USER") {
            if !sudo_user.is_empty() && sudo_user != "root" {
                if let Some(home) = passwd_home_dir(&sudo_user) {
                    return Some(home);
                }

                #[cfg(target_os = "macos")]
                {
                    return Some(PathBuf::from("/Users").join(sudo_user));
                }

                #[cfg(target_os = "linux")]
                {
                    return Some(PathBuf::from("/home").join(sudo_user));
                }
            }
        }
    }

    std::env::var("HOME").ok().map(PathBuf::from)
}

fn passwd_home_dir(user: &str) -> Option<PathBuf> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        if name != user {
            return None;
        }
        let home = fields.nth(4)?;
        (!home.is_empty()).then(|| PathBuf::from(home))
    })
}

fn resolve_db_storage_dir(db_dir: Option<&Path>) -> Option<PathBuf> {
    match db_dir {
        Some(path) if path.is_dir() => Some(path.to_path_buf()),
        Some(_) => None,
        None => real_home_dir().and_then(|home| paths::find_db_storage_dir(&home)),
    }
}

pub(crate) fn extract_keys_with_method_and_jobs(
    timeout_secs: u64,
    method: KeyExtractionMethod,
    jobs: usize,
    db_dir: Option<&Path>,
    keys_path: &Path,
) -> Result<String, String> {
    if method == KeyExtractionMethod::Login && timeout_secs == 0 {
        return Err("Login key capture requires a timeout greater than zero.".to_string());
    }
    match method {
        KeyExtractionMethod::Auto => extract_keys_auto(jobs, db_dir, keys_path),
        KeyExtractionMethod::Login => {
            extract_keys_with_login_capture(timeout_secs, jobs, db_dir, keys_path)
        }
    }
}

fn extract_keys_auto(
    jobs: usize,
    db_dir: Option<&Path>,
    keys_path: &Path,
) -> Result<String, String> {
    if let Some(path) = extract_keys_from_cached_material(jobs, db_dir, keys_path)? {
        return Ok(path);
    }

    let command = resolve_db_storage_dir(db_dir)
        .or_else(|| db_dir.map(Path::to_path_buf))
        .map(|db_dir| paths::key_recovery_command(&db_dir, keys_path))
        .unwrap_or_else(|| "tg keys".to_string());
    Err(format!(
        "No usable cached account key material. Capture it once with:\n\n  \
         sudo {command} --method login --timeout 180"
    ))
}

fn extract_keys_from_cached_material(
    jobs: usize,
    db_dir: Option<&Path>,
    keys_path: &Path,
) -> Result<Option<String>, String> {
    let material_path = paths::default_key_material_path();
    let material = match key_material::load(&material_path) {
        Ok(Some(material)) => material,
        Ok(None) => return Ok(None),
        Err(error) => {
            log::warn!("Cannot use cached account key material: {error}");
            return Ok(None);
        }
    };
    let Some(db_storage) = resolve_db_storage_dir(db_dir) else {
        return Ok(None);
    };
    let existing = load_existing_keys(keys_path)?;
    let outcome =
        database_keys::derive_from_storage(material.as_bytes(), &db_storage, &existing, jobs)?;
    if outcome.is_complete() || outcome.derived_databases > 0 {
        log_derivation_outcome("cached account material", &outcome);
        write_derivation_outcome(keys_path, outcome)?;
        return Ok(Some(keys_path.to_string_lossy().to_string()));
    }

    if outcome.total_databases > 0 {
        log::warn!(
            "Cached account key material did not cover {} database(s); a new capture is required.",
            outcome.missing_paths.len()
        );
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
fn extract_keys_with_login_capture(
    timeout_secs: u64,
    jobs: usize,
    db_dir: Option<&Path>,
    keys_path: &Path,
) -> Result<String, String> {
    let db_storage = resolve_db_storage_dir(db_dir)
        .ok_or_else(|| "Cannot find local db_storage directory.".to_string())?;
    validate_sudo_credentials(timeout_secs)?;
    let pid = find_telegram_pid().map_err(|e| format!("Cannot find desktop process: {e}"))?;
    let lldb_path = find_lldb_command()?;
    let work_dir = tempfile::Builder::new()
        .prefix("tg-lldb-material-")
        .tempdir()
        .map_err(|e| format!("Cannot create lldb capture directory: {e}"))?;

    log::info!("Waiting for account key derivation in the desktop client.");
    log::info!("Log out of the account, then log back in within {timeout_secs} seconds.");
    let material = lldb::capture_account_material(lldb::CaptureRequest {
        pid,
        lldb_path: &lldb_path,
        work_dir: work_dir.path(),
        timeout_secs,
    })?;

    finish_login_capture(material, &db_storage, jobs, keys_path)
}

#[cfg(target_os = "linux")]
fn extract_keys_with_login_capture(
    timeout_secs: u64,
    jobs: usize,
    db_dir: Option<&Path>,
    keys_path: &Path,
) -> Result<String, String> {
    let db_storage = resolve_db_storage_dir(db_dir)
        .ok_or_else(|| "Cannot find local db_storage directory.".to_string())?;
    let identity =
        linux_process::find_invoking_user_main_process(dictionary::desktop_app_process())?;
    let binary_path = identity.proc_exe_path();
    let hook = elf_locator::find_hook(&binary_path)?;
    identity.verify()?;
    let runtime_address = elf_locator::runtime_address(&identity, hook.file_offset)?;
    let gdb_path = find_gdb_command()?;
    let work_dir = tempfile::Builder::new()
        .prefix("tg-gdb-material-")
        .tempdir_in("/tmp")
        .map_err(|error| format!("Cannot create gdb capture directory: {error}"))?;

    log::info!("Waiting for account key derivation in the desktop client.");
    log::info!("Log out of the account, then log back in within {timeout_secs} seconds.");
    let material = gdb::capture_account_material(gdb::CaptureRequest {
        identity: &identity,
        gdb_path: &gdb_path,
        work_dir: work_dir.path(),
        runtime_address,
        code_fingerprint: &hook.code_fingerprint,
        architecture: hook.architecture,
        timeout_secs,
    })?;

    finish_login_capture(material, &db_storage, jobs, keys_path)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn extract_keys_with_login_capture(
    _timeout_secs: u64,
    _jobs: usize,
    _db_dir: Option<&Path>,
    _keys_path: &Path,
) -> Result<String, String> {
    Err("Login key extraction is only supported on macOS and Linux.".to_string())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn finish_login_capture(
    material: key_material::AccountKeyMaterial,
    db_storage: &Path,
    jobs: usize,
    keys_path: &Path,
) -> Result<String, String> {
    let existing = load_existing_keys(keys_path)?;
    let outcome =
        database_keys::derive_from_storage(material.as_bytes(), db_storage, &existing, jobs)?;
    if outcome.derived_databases == 0
        && !database_keys::validates_against_storage(material.as_bytes(), db_storage)?
    {
        return Err(
            "Captured account key material did not validate against any local database."
                .to_string(),
        );
    }

    key_material::store(&paths::default_key_material_path(), &material)?;
    log_derivation_outcome("captured account material", &outcome);
    write_derivation_outcome(keys_path, outcome)?;
    Ok(keys_path.to_string_lossy().to_string())
}

fn load_existing_keys(path: &Path) -> Result<DatabaseKeys, String> {
    match database_keys::load_key_file(path) {
        Ok(keys) => Ok(keys.unwrap_or_default()),
        Err(error) => match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                log::warn!("Replacing unusable database keys file: {error}");
                Ok(DatabaseKeys::new())
            }
            _ => Err(error),
        },
    }
}

fn log_derivation_outcome(label: &str, outcome: &database_keys::DerivationOutcome) {
    log::info!(
        "Database keys from {label}: {} reused, {} derived across {} new salt(s), {} total.",
        outcome.reused_databases,
        outcome.derived_databases,
        outcome.derived_salts,
        outcome.total_databases
    );
    if !outcome.missing_paths.is_empty() {
        log::warn!(
            "No verified key for {} database(s). Run with debug logging for source details.",
            outcome.missing_paths.len()
        );
        log::debug!(
            "Databases without a verified key: {:?}",
            outcome.missing_paths
        );
    }
}

fn write_derivation_outcome(
    path: &Path,
    outcome: database_keys::DerivationOutcome,
) -> Result<(), String> {
    let mut content = zeroize::Zeroizing::new(
        serde_json::to_string_pretty(&outcome.keys)
            .map_err(|error| format!("Cannot encode database keys: {error}"))?,
    );
    content.push('\n');
    write_keys_file(path, &content)
}

#[cfg(target_os = "macos")]
fn validate_sudo_credentials(timeout_secs: u64) -> Result<(), String> {
    let mut cmd = Command::new("sudo");
    cmd.arg("-v");
    cmd.stdout(Stdio::null());
    attach_sudo_tty(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to request sudo authentication: {}", e))?;
    let started = Instant::now();
    loop {
        match child
            .try_wait()
            .map_err(|e| format!("Failed to wait for sudo authentication: {}", e))?
        {
            Some(status) if status.success() => return Ok(()),
            Some(status) => {
                return Err(format!(
                    "sudo authentication failed with exit code {:?}.",
                    status.code()
                ));
            }
            None if timeout_secs > 0 && started.elapsed() >= Duration::from_secs(timeout_secs) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "sudo authentication timed out after {} seconds.",
                    timeout_secs
                ));
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

#[cfg(target_os = "macos")]
fn attach_sudo_tty(cmd: &mut Command) {
    match std::fs::File::open("/dev/tty") {
        Ok(tty) => {
            cmd.stdin(Stdio::from(tty));
        }
        Err(_) => {
            cmd.stdin(Stdio::inherit());
        }
    }

    match std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        Ok(tty) => {
            cmd.stderr(Stdio::from(tty));
        }
        Err(_) => {
            cmd.stderr(Stdio::inherit());
        }
    }
}

#[cfg(target_os = "macos")]
fn find_lldb_command() -> Result<PathBuf, String> {
    let output = Command::new("xcrun")
        .args(["--find", "lldb"])
        .output()
        .map_err(|e| format!("Failed to locate lldb with xcrun: {}", e))?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let path = stdout.trim();
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(" xcrun said: {detail}")
    };
    Err(format!(
        "lldb is required for `keys --method login`; install Apple Command Line Tools with `xcode-select --install`.{detail}"
    ))
}

#[cfg(target_os = "linux")]
fn find_gdb_command() -> Result<PathBuf, String> {
    for candidate in ["/usr/bin/gdb", "/bin/gdb", "/usr/local/bin/gdb"] {
        let candidate = PathBuf::from(candidate);
        let Ok(canonical) = std::fs::canonicalize(&candidate) else {
            continue;
        };
        if trusted_root_executable(&canonical) {
            return Ok(canonical);
        }
    }
    Err(
        "gdb is required for `keys --method login`; install it with `sudo apt install gdb`."
            .to_string(),
    )
}

#[cfg(target_os = "linux")]
fn trusted_root_executable(path: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let mode = metadata.permissions().mode();
    if !metadata.is_file() || metadata.uid() != 0 || mode & 0o111 == 0 || mode & 0o022 != 0 {
        return false;
    }

    let mut parent = path.parent();
    while let Some(directory) = parent {
        let Ok(metadata) = std::fs::metadata(directory) else {
            return false;
        };
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.permissions().mode() & 0o022 != 0 {
            return false;
        }
        parent = directory.parent();
    }
    true
}

#[cfg(target_os = "macos")]
fn wait_child_with_output(
    mut child: std::process::Child,
    timeout_secs: u64,
    label: &str,
) -> Result<Output, String> {
    let started = Instant::now();
    loop {
        match child
            .try_wait()
            .map_err(|e| format!("Failed to wait for {}: {}", label, e))?
        {
            Some(_) => break,
            None if timeout_secs > 0 && started.elapsed() >= Duration::from_secs(timeout_secs) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{} timed out after {} seconds.",
                    label, timeout_secs
                ));
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    child
        .wait_with_output()
        .map_err(|e| format!("Failed to read {} output: {}", label, e))
}

fn write_keys_file(keys_path: &std::path::Path, content: &str) -> Result<(), String> {
    if let Some(parent) = keys_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        paths::ensure_private_dir(parent)
            .map_err(|e| format!("Cannot create key directory {}: {}", parent.display(), e))?;
    }
    write_private_key_file(keys_path, content)
        .map_err(|e| format!("Cannot write keys file {}: {}", keys_path.display(), e))?;
    restrict_key_file_permissions(keys_path).map_err(|e| {
        format!(
            "Cannot restrict database key permissions for {}: {e}",
            keys_path.display()
        )
    })?;
    paths::restore_invoking_user_ownership(keys_path).map_err(|e| {
        format!(
            "Cannot restore database key ownership for {}: {e}",
            keys_path.display()
        )
    })?;
    Ok(())
}

#[cfg(unix)]
fn write_private_key_file(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => restrict_key_file_permissions(path)?,
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "keys path is not a regular file",
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(content.as_bytes())
}

#[cfg(not(unix))]
fn write_private_key_file(path: &Path, content: &str) -> std::io::Result<()> {
    std::fs::write(path, content)
}

#[cfg(unix)]
fn restrict_key_file_permissions(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_key_file_permissions(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_key_refresh_replaces_malformed_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.json");
        std::fs::write(&path, b"not json").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        assert!(load_existing_keys(&path).unwrap().is_empty());

        let directory = dir.path().join("not-a-file");
        std::fs::create_dir(&directory).unwrap();
        assert!(load_existing_keys(&directory).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn custom_key_output_preserves_parent_and_creates_private_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("shared");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = parent.join("keys.json");

        write_keys_file(&path, "{}\n").unwrap();

        assert_eq!(
            std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
