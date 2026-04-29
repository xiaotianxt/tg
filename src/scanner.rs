use std::ffi::{CString, OsStr, OsString};
use std::os::raw::{c_char, c_int};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::dictionary;

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
