use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::dictionary;

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

pub fn default_scanner_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let installed = dir.join("scanner_macos");
            if installed.exists() {
                return installed;
            }
        }
    }

    PathBuf::from("./scanner_macos")
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
/// Runs the C scanner binary, wrapping with sudo if not already root.
pub fn extract_keys(scanner_path: &Path, timeout_secs: u64) -> Result<String, String> {
    if !scanner_path.exists() {
        return Err(format!(
            "Scanner binary not found at {}. Run 'make scanner' first.",
            scanner_path.display()
        ));
    }

    let pid = find_telegram_pid().map_err(|e| format!("Cannot find Telegram process: {}", e))?;
    log::info!("Telegram PID: {}", pid);

    let needs_sudo = !is_root();
    if needs_sudo {
        log::info!("Running key scanner (requires sudo)...");
        log::info!("You may be prompted for your password.");
    }

    let scanner_str = scanner_path.to_string_lossy();
    let mut cmd = if needs_sudo {
        let mut c = Command::new("sudo");
        c.arg(scanner_str.as_ref());
        c.stdin(Stdio::inherit());
        c.stderr(Stdio::inherit());
        c
    } else {
        let mut c = Command::new(scanner_str.as_ref());
        c.stderr(Stdio::piped());
        c
    };
    cmd.stdout(Stdio::piped());
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

    if !stderr.is_empty() {
        log::warn!("[stderr] {}", stderr.trim_end());
    }

    if !output.status.success() {
        return Err(format!(
            "Scanner failed with exit code {:?}.\nOutput: {}",
            output.status.code(),
            stdout
        ));
    }

    log::info!("{}", stdout.trim_end());

    let keys_path = PathBuf::from("all_keys.json");
    if keys_path.exists() {
        let content = std::fs::read_to_string(&keys_path)
            .map_err(|e| format!("Cannot read keys file: {}", e))?;
        let key_count = content.matches("\"enc_key\"").count();
        log::info!("Found {} database keys.", key_count);
        Ok(keys_path.to_string_lossy().to_string())
    } else {
        Err("all_keys.json not found. Key extraction may have failed.".to_string())
    }
}
