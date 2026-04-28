use std::path::{Path, PathBuf};
use std::process::Command;

fn find_telegram_pid() -> Result<i32, String> {
    let output = Command::new("pgrep")
        .arg("-x")
        .arg("Telegram")
        .output()
        .map_err(|e| format!("Failed to run pgrep: {}", e))?;

    if !output.status.success() {
        return Err("Telegram is not running.".to_string());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pid = stdout.trim().parse::<i32>()
        .map_err(|e| format!("Invalid PID: {}", e))?;
    Ok(pid)
}

fn is_root() -> bool {
    Command::new("id").arg("-u").output()
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
    let xtelegram_base = home.join("Library/Containers/com.telegram.xinTelegram/Data/Documents/xtelegram_files");
    if !xtelegram_base.is_dir() {
        return None;
    }

    for entry in std::fs::read_dir(&xtelegram_base).ok()?.flatten() {
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
pub fn extract_keys(scanner_path: &Path, _timeout_secs: u64) -> Result<String, String> {
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
        c
    } else {
        Command::new(scanner_str.as_ref())
    };
    cmd.arg(format!("{}", pid));
    if let Some(db_storage) = find_db_storage_dir() {
        cmd.arg(db_storage);
    }

    let output = cmd
        .output()
        .map_err(|e| format!("Failed to run scanner: {}", e))?;

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
