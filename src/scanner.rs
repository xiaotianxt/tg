use std::ffi::{CString, OsStr, OsString};
use std::os::raw::{c_char, c_int};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{dictionary, paths};

const INTERNAL_SCAN_ARG: &str = "__tg-scan-keys";

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn tg_scan_keys_macos(argc: c_int, argv: *const *const c_char) -> c_int;
}

pub(crate) fn maybe_run_internal_scanner() {
    let mut args = std::env::args_os();
    let _exe = args.next();
    let Some(first) = args.next() else {
        return;
    };
    if first != OsStr::new(INTERNAL_SCAN_ARG) {
        return;
    }

    let code = run_internal_scanner(args.collect());
    std::process::exit(code);
}

#[cfg(target_os = "macos")]
fn run_internal_scanner(args: Vec<OsString>) -> i32 {
    use std::os::unix::ffi::OsStrExt;

    let mut cstrings = Vec::with_capacity(args.len() + 1);
    cstrings.push(CString::new("tg-scan-keys").expect("static scanner argv is valid"));

    for arg in args {
        match CString::new(arg.as_os_str().as_bytes()) {
            Ok(value) => cstrings.push(value),
            Err(_) => {
                eprintln!("scanner argument contains an unsupported NUL byte");
                return 2;
            }
        }
    }

    let argv: Vec<*const c_char> = cstrings.iter().map(|arg| arg.as_ptr()).collect();
    unsafe { tg_scan_keys_macos(argv.len() as c_int, argv.as_ptr()) }
}

#[cfg(not(target_os = "macos"))]
fn run_internal_scanner(_args: Vec<OsString>) -> i32 {
    eprintln!("tg key extraction is only supported on macOS");
    1
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
                return Some(PathBuf::from("/Users").join(sudo_user));
            }
        }
    }

    std::env::var("HOME").ok().map(PathBuf::from)
}

fn find_db_storage_dir() -> Option<PathBuf> {
    let home = real_home_dir()?;
    let account_files = dictionary::documents_account_files_dir(&home);
    if !account_files.is_dir() {
        return None;
    }

    for entry in std::fs::read_dir(&account_files).ok()?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let direct = path.join("db_storage");
        if direct.is_dir() {
            return Some(direct);
        }

        if let Ok(sub_entries) = std::fs::read_dir(&path) {
            for sub_entry in sub_entries.flatten() {
                let nested = sub_entry.path().join("db_storage");
                if nested.is_dir() {
                    return Some(nested);
                }
            }
        }
    }

    None
}

/// Extract DB encryption keys from Telegram process memory.
/// Runs tg's embedded macOS scanner, wrapping with sudo if not already root.
pub fn extract_keys(timeout_secs: u64) -> Result<String, String> {
    let pid = find_telegram_pid().map_err(|e| format!("Cannot find Telegram process: {}", e))?;
    log::info!("Telegram PID: {}", pid);
    let work_dir = tempfile::Builder::new()
        .prefix("tg-keys-")
        .tempdir()
        .map_err(|e| format!("Cannot create temporary key scan dir: {}", e))?;

    let needs_sudo = !is_root();
    if needs_sudo {
        log::info!("Running key scanner (requires sudo)...");
        log::info!("You may be prompted for your password.");
    }

    let exe =
        std::env::current_exe().map_err(|e| format!("Cannot locate current tg binary: {}", e))?;
    let mut cmd = if needs_sudo {
        let mut c = Command::new("sudo");
        c.arg(&exe);
        c.stdin(Stdio::inherit());
        c.stderr(Stdio::inherit());
        c
    } else {
        let mut c = Command::new(&exe);
        c.stderr(Stdio::piped());
        c
    };
    cmd.stdout(Stdio::piped());
    cmd.current_dir(work_dir.path());
    cmd.arg(INTERNAL_SCAN_ARG);
    cmd.arg(format!("{}", pid));
    if let Some(db_storage) = find_db_storage_dir() {
        cmd.arg(db_storage);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to run scanner: {}", e))?;

    let started = Instant::now();
    loop {
        match child
            .try_wait()
            .map_err(|e| format!("Failed to wait for scanner: {}", e))?
        {
            Some(_) => break,
            None if timeout_secs > 0 && started.elapsed() >= Duration::from_secs(timeout_secs) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Scanner timed out after {} seconds.", timeout_secs));
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to read scanner output: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() && !stderr.is_empty() {
        log::warn!("[stderr] {}", stderr.trim_end());
    }

    if !output.status.success() {
        return Err(scanner_failure_message(
            output.status.code(),
            &stdout,
            &stderr,
        ));
    }

    log::info!("{}", stdout.trim_end());

    let scanned_keys_path = work_dir.path().join("all_keys.json");
    let keys_path = paths::default_keys_path();
    if scanned_keys_path.exists() {
        let content = std::fs::read_to_string(&scanned_keys_path)
            .map_err(|e| format!("Cannot read keys file: {}", e))?;
        if let Some(parent) = keys_path.parent() {
            paths::ensure_private_dir(parent)
                .map_err(|e| format!("Cannot create key directory {}: {}", parent.display(), e))?;
        }
        std::fs::write(&keys_path, &content)
            .map_err(|e| format!("Cannot write keys file {}: {}", keys_path.display(), e))?;
        restrict_key_file_permissions(&keys_path);
        let key_count = content.matches("\"enc_key\"").count();
        log::info!("Found {} database keys.", key_count);
        Ok(keys_path.to_string_lossy().to_string())
    } else {
        Err("all_keys.json not found. Key extraction may have failed.".to_string())
    }
}

#[cfg(unix)]
fn restrict_key_file_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_key_file_permissions(_path: &std::path::Path) {}

fn scanner_failure_message(code: Option<i32>, stdout: &str, stderr: &str) -> String {
    let mut message = format!("Scanner failed with exit code {:?}.", code);
    append_labeled_output(&mut message, "Output", stdout);
    append_labeled_output(&mut message, "Error output", stderr);
    message.push_str("\n\nIf key extraction failed with `task_for_pid failed` or another macOS process permission error, quit Telegram, re-sign it, reopen it, then retry:\n\n");
    message.push_str("  sudo codesign --force --deep --sign - /Applications/Telegram.app\n");
    message.push_str("  sudo tg keys\n\n");
    message.push_str("If Telegram is installed somewhere else, use that `.app` path instead.");
    message
}

fn append_labeled_output(message: &mut String, label: &str, output: &str) {
    let output = output.trim_end();
    if output.is_empty() {
        return;
    }

    message.push('\n');
    message.push_str(label);
    message.push_str(":\n");
    message.push_str(output);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_failure_message_includes_permission_recovery_hint() {
        let message =
            scanner_failure_message(Some(1), "Telegram PID: 123\n", "task_for_pid failed: 5\n");

        assert!(message.contains("Scanner failed with exit code Some(1)."));
        assert!(message.contains("Output:\nTelegram PID: 123"));
        assert!(message.contains("Error output:\ntask_for_pid failed: 5"));
        assert!(
            message.contains("sudo codesign --force --deep --sign - /Applications/Telegram.app")
        );
        assert!(message.contains("sudo tg keys"));
    }
}
