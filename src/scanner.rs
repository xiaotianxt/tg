use std::collections::BTreeSet;
#[cfg(target_os = "macos")]
use std::ffi::CString;
use std::ffi::{OsStr, OsString};
#[cfg(target_os = "macos")]
use std::os::raw::{c_char, c_int};
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
#[cfg(target_os = "macos")]
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    decrypt::{self, DatabaseKeys},
    dictionary, paths,
};

const INTERNAL_SCAN_ARG: &str = "__tg-scan-keys";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyExtractionMethod {
    Memory,
    LldbCold,
}

#[cfg(target_os = "macos")]
// SAFETY: This declares the scanner entry point provided by the checked-in C
// source linked in build.rs. The call site builds a stable argv for the call.
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
    // SAFETY: `cstrings` owns every NUL-terminated argument for the duration
    // of the call, and `argc` matches the number of pointers in `argv`.
    unsafe { tg_scan_keys_macos(argv.len() as c_int, argv.as_ptr()) }
}

#[cfg(target_os = "linux")]
fn run_internal_scanner(args: Vec<OsString>) -> i32 {
    match linux_scanner::run(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{}", e);
            1
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn run_internal_scanner(_args: Vec<OsString>) -> i32 {
    eprintln!("tg key extraction is only supported on macOS and Linux");
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

fn find_db_storage_dir() -> Option<PathBuf> {
    let home = real_home_dir()?;
    for account_files in dictionary::account_files_candidate_dirs(&home) {
        if !account_files.is_dir() {
            continue;
        }

        let Ok(entries) = std::fs::read_dir(&account_files) else {
            continue;
        };
        for entry in entries.flatten() {
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
    }

    None
}

/// Extract DB encryption keys from Telegram process memory.
/// Runs tg's embedded macOS scanner, wrapping with sudo if not already root.
pub fn extract_keys(timeout_secs: u64) -> Result<String, String> {
    extract_keys_with_method(timeout_secs, KeyExtractionMethod::Memory)
}

pub(crate) fn extract_keys_with_method(
    timeout_secs: u64,
    method: KeyExtractionMethod,
) -> Result<String, String> {
    match method {
        KeyExtractionMethod::Memory => extract_keys_from_memory(timeout_secs),
        KeyExtractionMethod::LldbCold => extract_keys_with_lldb_cold(timeout_secs),
    }
}

fn extract_keys_from_memory(timeout_secs: u64) -> Result<String, String> {
    let pid = find_telegram_pid().map_err(|e| format!("Cannot find Telegram process: {}", e))?;
    log::info!("Telegram PID: {}", pid);
    let pids = find_key_scan_pids(pid);
    log::info!("Scanning {} process(es) for keys.", pids.len());
    let work_dir = tempfile::Builder::new()
        .prefix("tg-keys-")
        .tempdir()
        .map_err(|e| format!("Cannot create temporary key scan dir: {}", e))?;

    let needs_sudo = !is_root();
    if needs_sudo {
        log::info!("Running key scanner (requires sudo)...");
        log::info!("You may be prompted for your password.");
        validate_sudo_credentials(timeout_secs)?;
    }

    let exe =
        std::env::current_exe().map_err(|e| format!("Cannot locate current tg binary: {}", e))?;
    let db_storage = find_db_storage_dir();
    let keys_path = paths::default_keys_path();

    let mut combined_stdout = String::new();
    let mut combined_stderr = String::new();
    let mut keys = DatabaseKeys::new();
    let mut candidate_paths = Vec::new();
    let mut successful_scans = 0usize;

    for scan_pid in pids {
        let scan_dir = work_dir.path().join(format!("pid-{scan_pid}"));
        std::fs::create_dir_all(&scan_dir).map_err(|e| {
            format!(
                "Cannot create scanner work dir {}: {}",
                scan_dir.display(),
                e
            )
        })?;
        let output = match run_scanner_process(
            &exe,
            scan_pid,
            db_storage.as_deref(),
            &scan_dir,
            needs_sudo,
            timeout_secs,
        ) {
            Ok(output) => output,
            Err(e) => {
                log::warn!("Key scanner for pid {} did not complete: {}", scan_pid, e);
                append_scan_log(&mut combined_stderr, scan_pid, &e);
                continue;
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        append_scan_log(&mut combined_stdout, scan_pid, &stdout);
        append_scan_log(&mut combined_stderr, scan_pid, &stderr);

        if output.status.success() && !stderr.is_empty() {
            log::warn!("[scanner pid {scan_pid} stderr] {}", stderr.trim_end());
        }
        if !output.status.success() {
            log::warn!(
                "Key scanner for pid {} failed with exit code {:?}.",
                scan_pid,
                output.status.code()
            );
            continue;
        }

        successful_scans += 1;
        log::info!("{}", stdout.trim_end());

        let scanned_keys_path = scan_dir.join("all_keys.json");
        if scanned_keys_path.exists() {
            let content = std::fs::read_to_string(&scanned_keys_path)
                .map_err(|e| format!("Cannot read keys file: {}", e))?;
            let scanned_keys: DatabaseKeys = serde_json::from_str(&content)
                .map_err(|e| format!("Invalid scanner keys JSON: {}", e))?;
            merge_database_keys(&mut keys, scanned_keys);
        }

        let candidates_path = scan_dir.join("candidate_keys.txt");
        if candidates_path.exists() {
            candidate_paths.push(candidates_path);
        }
    }

    if successful_scans == 0 {
        return Err(scanner_failure_message(
            None,
            &combined_stdout,
            &combined_stderr,
        ));
    }

    if let Some(db_storage) = db_storage.as_deref() {
        let matched = match_raw_candidate_keys(&candidate_paths, db_storage)?;
        merge_database_keys(&mut keys, matched);
    }

    let content = serde_json::to_string_pretty(&keys)
        .map_err(|e| format!("Cannot encode keys JSON: {}", e))?
        + "\n";
    let key_count = count_encryption_keys(&content)?;
    if key_count == 0 {
        return Err(zero_key_scan_message(
            &combined_stdout,
            &combined_stderr,
            &keys_path,
        ));
    }

    write_keys_file(&keys_path, &content)?;
    log::info!("Found {} database keys.", key_count);
    Ok(keys_path.to_string_lossy().to_string())
}

#[cfg(target_os = "macos")]
fn extract_keys_with_lldb_cold(timeout_secs: u64) -> Result<String, String> {
    let db_storage = find_db_storage_dir()
        .ok_or_else(|| "Cannot find local db_storage directory.".to_string())?;
    validate_sudo_credentials(timeout_secs)?;

    let app_name = dictionary::desktop_app_name();
    quit_desktop_app_if_running(app_name)?;
    let lldb_path = find_lldb_command()?;

    let work_dir = create_lldb_work_dir()?;
    let script_path = work_dir.join("capture_keys.py");
    let candidate_path = work_dir.join("candidate_keys.txt");
    let log_path = work_dir.join("lldb-capture.log");

    std::fs::write(&script_path, LLDB_KEY_CAPTURE_SCRIPT)
        .map_err(|e| format!("Cannot write lldb capture script: {}", e))?;
    restrict_key_file_permissions(&script_path);

    Command::new("open")
        .arg("-a")
        .arg(app_name)
        .status()
        .map_err(|e| format!("Cannot open desktop client: {}", e))?;

    let pid = wait_for_process(app_name, Duration::from_secs(10))?;
    let capture_result = run_lldb_key_capture(LldbCaptureRequest {
        pid,
        lldb_path: &lldb_path,
        script_path: &script_path,
        candidate_path: &candidate_path,
        log_path: &log_path,
        db_storage: &db_storage,
        work_dir: &work_dir,
        timeout_secs,
    });

    let mut combined_stdout = String::new();
    match capture_result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            append_labeled_output(&mut combined_stdout, "lldb output", &stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            append_labeled_output(&mut combined_stdout, "lldb error output", &stderr);
            if !output.status.success() && !candidate_file_has_keys(&candidate_path) {
                return Err(lldb_capture_failure_message(
                    output.status.code(),
                    &combined_stdout,
                    &stderr,
                ));
            }
        }
        Err(e) => {
            if !candidate_file_has_keys(&candidate_path) {
                return Err(format!("{}.{}", e, lldb_log_summary(&log_path)));
            }
            log::warn!(
                "lldb key capture did not exit cleanly: {}. Using captured key candidates.",
                e
            );
        }
    }

    let matched = match_raw_candidate_keys(&[candidate_path], &db_storage)?;
    let content = serde_json::to_string_pretty(&matched)
        .map_err(|e| format!("Cannot encode keys JSON: {}", e))?
        + "\n";
    let key_count = count_encryption_keys(&content)?;
    if key_count == 0 {
        let log_summary = lldb_log_summary(&log_path);
        return Err(format!(
            "lldb key capture completed but found 0 matching database keys.{}{}",
            combined_stdout, log_summary
        ));
    }

    let keys_path = paths::default_keys_path();
    write_keys_file(&keys_path, &content)?;
    cleanup_lldb_work_dir(&work_dir);
    log::info!("Found {} database keys with lldb.", key_count);
    Ok(keys_path.to_string_lossy().to_string())
}

#[cfg(not(target_os = "macos"))]
fn extract_keys_with_lldb_cold(_timeout_secs: u64) -> Result<String, String> {
    Err("lldb cold key extraction is only supported on macOS.".to_string())
}

fn run_scanner_process(
    exe: &Path,
    pid: i32,
    db_storage: Option<&Path>,
    work_dir: &Path,
    needs_sudo: bool,
    timeout_secs: u64,
) -> Result<Output, String> {
    let mut cmd = if needs_sudo {
        let mut c = Command::new("sudo");
        c.arg(exe);
        attach_sudo_tty(&mut c);
        c
    } else {
        let mut c = Command::new(exe);
        c.stderr(Stdio::piped());
        c
    };
    cmd.stdout(Stdio::piped());
    cmd.current_dir(work_dir);
    cmd.arg(INTERNAL_SCAN_ARG);
    cmd.arg(format!("{}", pid));
    if let Some(db_storage) = db_storage {
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
                return Err(format!(
                    "Scanner for pid {} timed out after {} seconds.",
                    pid, timeout_secs
                ));
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }

    child
        .wait_with_output()
        .map_err(|e| format!("Failed to read scanner output: {}", e))
}

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
struct LldbCaptureRequest<'a> {
    pid: i32,
    lldb_path: &'a Path,
    script_path: &'a Path,
    candidate_path: &'a Path,
    log_path: &'a Path,
    db_storage: &'a Path,
    work_dir: &'a Path,
    timeout_secs: u64,
}

#[cfg(target_os = "macos")]
fn run_lldb_key_capture(request: LldbCaptureRequest<'_>) -> Result<Output, String> {
    let mut cmd = Command::new("sudo");
    cmd.arg("-n")
        .arg("env")
        .arg("TERM=dumb")
        .arg(format!(
            "TG_LLDB_DB_STORAGE={}",
            request.db_storage.display()
        ))
        .arg(format!(
            "TG_LLDB_CANDIDATE_KEYS={}",
            request.candidate_path.display()
        ))
        .arg(format!("TG_LLDB_LOG={}", request.log_path.display()))
        .arg(format!("TG_LLDB_TIMEOUT_SECONDS={}", request.timeout_secs))
        .arg(format!(
            "TG_LLDB_TARGET_MODULE_SUFFIX={}",
            dictionary::target_module_suffix()
        ))
        .arg(request.lldb_path)
        .arg("-b")
        .arg("-o")
        .arg(format!(
            "command script import {}",
            request.script_path.display()
        ))
        .arg("-o")
        .arg(format!("process attach --pid {}", request.pid))
        .arg("-o")
        .arg("capture_tg_lldb_keys")
        .arg("-o")
        .arg("process continue")
        .current_dir(request.work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd
        .spawn()
        .map_err(|e| format!("Failed to run lldb key capture: {}", e))?;
    wait_child_with_output(child, request.timeout_secs + 20, "lldb key capture")
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
        "lldb is required for `keys --method lldb-cold`; install Apple Command Line Tools with `xcode-select --install`.{detail}"
    ))
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

#[cfg(target_os = "macos")]
fn quit_desktop_app_if_running(app_name: &str) -> Result<(), String> {
    if !process_is_running(app_name) {
        return Ok(());
    }

    let script = format!(
        "tell application \"{}\" to quit",
        app_name.replace('"', "\\\"")
    );
    let _ = Command::new("osascript").arg("-e").arg(script).status();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(15) {
        if !process_is_running(app_name) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    Err(format!(
        "{} is still running. Quit it, then rerun `keys --method lldb-cold`.",
        app_name
    ))
}

#[cfg(target_os = "macos")]
fn wait_for_process(app_name: &str, timeout: Duration) -> Result<i32, String> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if let Ok(pid) = find_telegram_pid() {
            return Ok(pid);
        }
        let output = Command::new("pgrep")
            .arg("-x")
            .arg(app_name)
            .output()
            .map_err(|e| format!("Failed to run pgrep: {}", e))?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(pid) = stdout
                .lines()
                .next()
                .and_then(|line| line.trim().parse::<i32>().ok())
            {
                return Ok(pid);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!("Timed out waiting for {} to start.", app_name))
}

#[cfg(target_os = "macos")]
fn process_is_running(app_name: &str) -> bool {
    Command::new("pgrep")
        .arg("-x")
        .arg(app_name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn create_lldb_work_dir() -> Result<PathBuf, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("System clock error: {}", e))?
        .as_nanos();
    let path = PathBuf::from(format!(
        "/private/tmp/tg-lldb-keys-{}-{}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&path)
        .map_err(|e| format!("Cannot create lldb work dir {}: {}", path.display(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
    }
    Ok(path)
}

#[cfg(target_os = "macos")]
fn cleanup_lldb_work_dir(path: &Path) {
    if std::env::var_os("TG_LLDB_KEEP_WORKDIR").is_some() {
        log::info!("Keeping lldb work dir: {}", path.display());
        return;
    }
    let _ = std::fs::remove_dir_all(path);
}

#[cfg(target_os = "macos")]
fn lldb_log_summary(path: &Path) -> String {
    let Ok(content) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let mut installed = false;
    let mut hits = 0usize;
    let mut candidates = 0usize;
    let mut matched = 0usize;
    let mut detach_reason = None;
    for line in content.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match record.get("event").and_then(|value| value.as_str()) {
            Some("installed") => installed = true,
            Some("key_setup") => {
                hits += 1;
                if record
                    .get("candidate")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false)
                {
                    candidates += 1;
                }
                matched = matched.max(
                    record
                        .get("matched_count")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0) as usize,
                );
            }
            Some("detach") => {
                detach_reason = record
                    .get("reason")
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
            }
            _ => {}
        }
    }
    format!(
        "\nlldb summary: installed={}, key_setup_hits={}, candidates={}, matched_dbs={}, detach={}",
        installed,
        hits,
        candidates,
        matched,
        detach_reason.unwrap_or_else(|| "unknown".to_string())
    )
}

#[cfg(target_os = "macos")]
fn candidate_file_has_keys(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|content| {
            content.lines().any(|line| {
                let key = line.trim();
                key.len() == 64 && key.as_bytes().iter().copied().all(is_hex_byte)
            })
        })
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
const LLDB_KEY_CAPTURE_SCRIPT: &str = r#"
import binascii
import glob
import hashlib
import hmac
import json
import os
import threading
import time
from pathlib import Path

import lldb

KEY_SETUP_OFFSET = 0x4B287C4
PAGE_SZ = 4096
SALT_SZ = 16
HMAC_SZ = 64
MAX_HITS = 96

DB_STORAGE = os.environ["TG_LLDB_DB_STORAGE"]
CANDIDATE_PATH = os.environ["TG_LLDB_CANDIDATE_KEYS"]
LOG_PATH = os.environ["TG_LLDB_LOG"]
TIMEOUT_SECONDS = int(os.environ.get("TG_LLDB_TIMEOUT_SECONDS", "60"))
TARGET_MODULE_SUFFIX = os.environ["TG_LLDB_TARGET_MODULE_SUFFIX"]

_hits = 0
_seen_keys = set()
_matched = set()
_db_pages = []
_timer_started = False


def _log(record):
    with open(LOG_PATH, "a", encoding="utf-8") as f:
        f.write(json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n")
        f.flush()


def _make_private_to_invoking_user(path):
    try:
        os.chmod(path, 0o600)
    except OSError:
        pass
    try:
        uid = int(os.environ.get("SUDO_UID", ""))
        gid = int(os.environ.get("SUDO_GID", ""))
    except ValueError:
        return
    try:
        os.chown(path, uid, gid)
    except OSError:
        pass


def _db_rel_path(path):
    text = str(path)
    marker = "/db_storage/"
    if marker in text:
        return text.split(marker, 1)[1]
    try:
        return str(path.relative_to(DB_STORAGE))
    except Exception:
        return path.name


def _load_db_pages():
    pages = []
    for path_text in glob.glob(os.path.join(DB_STORAGE, "**", "*.db"), recursive=True):
        path = Path(path_text)
        try:
            page = path.read_bytes()[:PAGE_SZ]
        except OSError:
            continue
        if len(page) != PAGE_SZ or page[:16] == b"SQLite format 3\x00":
            continue
        pages.append((_db_rel_path(path), page))
    return pages


def _key_matches_page(key, page):
    salt = page[:SALT_SZ]
    page1 = page[SALT_SZ:PAGE_SZ]
    mac_salt = bytes(byte ^ 0x3A for byte in salt)
    mac_key = hashlib.pbkdf2_hmac("sha512", key, mac_salt, 2, 32)
    digest = hmac.new(mac_key, page1[:-HMAC_SZ] + b"\x01\x00\x00\x00", "sha512").digest()
    return hmac.compare_digest(digest, page1[-HMAC_SZ:])


def _matches_for_key(key):
    return [rel for rel, page in _db_pages if _key_matches_page(key, page)]


def _read_memory(process, address, size):
    if not address or size <= 0:
        return None
    error = lldb.SBError()
    data = process.ReadMemory(address, size, error)
    if not error.Success() or not data:
        return None
    return data


def _candidate_key(raw):
    if not raw:
        return None
    if raw.startswith(b"x'"):
        raw = raw[2:]
    raw = raw.rstrip(b"'")
    try:
        if len(raw) >= 64 and all(chr(byte).lower() in "0123456789abcdef" for byte in raw[:64]):
            key = bytes.fromhex(raw[:64].decode("ascii"))
        else:
            key = raw[:32]
    except Exception:
        key = raw[:32]
    if len(key) != 32:
        return None
    return key


def _target_module_text_load(target):
    for module in target.module_iter():
        file_spec = module.GetFileSpec()
        path = os.path.join(file_spec.GetDirectory() or "", file_spec.GetFilename() or "")
        if path.endswith(TARGET_MODULE_SUFFIX):
            text = module.FindSection("__TEXT")
            if text and text.IsValid():
                return text.GetLoadAddress(target), path
    return None, None


def _detach_and_exit(debugger, reason):
    _log({"event": "detach", "reason": reason, "matched_count": len(_matched), "hits": _hits})
    process = debugger.GetSelectedTarget().GetProcess()
    if process and process.IsValid():
        process.Detach()
    os._exit(0)


def _start_timeout(debugger):
    global _timer_started
    if _timer_started:
        return
    _timer_started = True

    def worker():
        time.sleep(TIMEOUT_SECONDS)
        _detach_and_exit(debugger, "timeout")

    thread = threading.Thread(target=worker, daemon=True)
    thread.start()


def on_key_setup(frame, bp_loc, internal_dict):
    global _hits
    _hits += 1
    process = frame.GetThread().GetProcess()
    key_ptr = frame.FindRegister("x1").GetValueAsUnsigned()
    key_len = frame.FindRegister("x2").GetValueAsUnsigned()
    key = None
    if key_ptr and key_len in (32, 48, 64, 66, 67, 96, 99):
        key = _candidate_key(_read_memory(process, key_ptr, int(key_len)))

    matches = []
    candidate = False
    if key:
        key_hex = binascii.hexlify(key).decode("ascii")
        if key_hex not in _seen_keys:
            _seen_keys.add(key_hex)
            candidate = True
            with open(CANDIDATE_PATH, "a", encoding="utf-8") as f:
                f.write(key_hex + "\n")
                f.flush()
        matches = _matches_for_key(key)
        _matched.update(matches)

    _log({
        "event": "key_setup",
        "key_len": key_len,
        "candidate": candidate,
        "matches": matches,
        "matched_count": len(_matched),
        "hits": _hits,
    })

    debugger = process.GetTarget().GetDebugger()
    if len(_matched) >= len(_db_pages) and _db_pages:
        _detach_and_exit(debugger, "all_dbs_matched")
    if _hits >= MAX_HITS:
        _detach_and_exit(debugger, "max_hits")
    return False


def install(debugger, command, result, internal_dict):
    global _db_pages
    os.makedirs(os.path.dirname(LOG_PATH), exist_ok=True)
    open(LOG_PATH, "w", encoding="utf-8").close()
    open(CANDIDATE_PATH, "w", encoding="utf-8").close()
    _make_private_to_invoking_user(LOG_PATH)
    _make_private_to_invoking_user(CANDIDATE_PATH)
    _db_pages = _load_db_pages()
    target = debugger.GetSelectedTarget()
    _start_timeout(debugger)
    text_load, module_path = _target_module_text_load(target)
    if text_load is None:
        result.SetError("cannot find target database module text load address")
        return
    bp = target.BreakpointCreateByAddress(text_load + KEY_SETUP_OFFSET)
    bp.SetScriptCallbackFunction("capture_keys.on_key_setup")
    bp.SetAutoContinue(True)
    _log({
        "event": "installed",
        "db_pages": len(_db_pages),
        "locations": bp.GetNumLocations(),
        "offset": hex(KEY_SETUP_OFFSET),
    })
    result.PutCString("installed lldb key capture")


def __lldb_init_module(debugger, internal_dict):
    debugger.HandleCommand("command script add -f capture_keys.install capture_tg_lldb_keys")
"#;

fn append_scan_log(target: &mut String, pid: i32, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    if !target.is_empty() {
        target.push('\n');
    }
    target.push_str(&format!("[pid {pid}]\n"));
    target.push_str(text.trim_end());
    target.push('\n');
}

fn merge_database_keys(target: &mut DatabaseKeys, incoming: DatabaseKeys) {
    for (rel_path, entry) in incoming {
        let Some(enc_key) = entry.get("enc_key").filter(|key| !key.is_empty()) else {
            continue;
        };
        target
            .entry(rel_path)
            .or_default()
            .entry("enc_key".to_string())
            .or_insert_with(|| enc_key.clone());
    }
}

fn find_key_scan_pids(root_pid: i32) -> Vec<i32> {
    let Ok(output) = Command::new("ps")
        .args(["-ax", "-o", "pid=", "-o", "ppid=", "-o", "command="])
        .output()
    else {
        return vec![root_pid];
    };
    if !output.status.success() {
        return vec![root_pid];
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let processes = stdout
        .lines()
        .filter_map(parse_process_table_line)
        .collect::<Vec<_>>();

    key_scan_pids_from_process_table(root_pid, &processes)
}

fn parse_process_table_line(line: &str) -> Option<(i32, i32, String)> {
    let (pid_text, rest) = split_first_field(line)?;
    let (ppid_text, command) = split_first_field(rest)?;
    let pid = pid_text.parse::<i32>().ok()?;
    let ppid = ppid_text.parse::<i32>().ok()?;
    Some((pid, ppid, command.trim_start().to_string()))
}

fn split_first_field(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    if input.is_empty() {
        return None;
    }
    match input.find(char::is_whitespace) {
        Some(index) => Some((&input[..index], &input[index..])),
        None => Some((input, "")),
    }
}

fn key_scan_pids_from_process_table(root_pid: i32, processes: &[(i32, i32, String)]) -> Vec<i32> {
    let mut descendants = BTreeSet::new();
    let mut stack = vec![root_pid];
    while let Some(parent) = stack.pop() {
        for (pid, ppid, _) in processes {
            if *ppid == parent && descendants.insert(*pid) {
                stack.push(*pid);
            }
        }
    }

    let mut result = vec![root_pid];
    for (pid, _, command) in processes {
        if descendants.contains(pid) && should_scan_descendant_command(command) {
            result.push(*pid);
        }
    }
    result.sort_unstable();
    result.dedup();
    result
}

fn should_scan_descendant_command(command: &str) -> bool {
    let command = command.to_ascii_lowercase();
    command.contains(dictionary::account_files_dir())
        || command.contains("--service-sandbox-type=none")
}

fn write_keys_file(keys_path: &std::path::Path, content: &str) -> Result<(), String> {
    if let Some(parent) = keys_path.parent() {
        paths::ensure_private_dir(parent)
            .map_err(|e| format!("Cannot create key directory {}: {}", parent.display(), e))?;
    }
    std::fs::write(keys_path, content)
        .map_err(|e| format!("Cannot write keys file {}: {}", keys_path.display(), e))?;
    restrict_key_file_permissions(keys_path);
    Ok(())
}

fn count_encryption_keys(content: &str) -> Result<usize, String> {
    let keys: DatabaseKeys =
        serde_json::from_str(content).map_err(|e| format!("Invalid scanner keys JSON: {}", e))?;
    Ok(keys
        .values()
        .filter(|entry| entry.get("enc_key").is_some_and(|key| !key.is_empty()))
        .count())
}

fn match_raw_candidate_keys(
    candidate_paths: &[PathBuf],
    db_storage: &std::path::Path,
) -> Result<DatabaseKeys, String> {
    let mut candidate_set = BTreeSet::new();
    for path in candidate_paths {
        candidate_set.extend(read_raw_candidate_keys(path)?);
    }
    let candidates = decode_candidate_keys(candidate_set)?;
    let mut matched = DatabaseKeys::new();
    if candidates.is_empty() {
        return Ok(matched);
    }

    for source in load_source_db_pages(db_storage)? {
        for candidate in &candidates {
            if decrypt::key_bytes_match_page1(&candidate.bytes, &source.page1) {
                matched
                    .entry(source.rel_path.clone())
                    .or_default()
                    .insert("enc_key".to_string(), candidate.hex.clone());
                break;
            }
        }
    }
    Ok(matched)
}

struct CandidateKey {
    hex: String,
    bytes: Vec<u8>,
}

struct SourceDbPage {
    rel_path: String,
    page1: Vec<u8>,
}

fn decode_candidate_keys(candidate_set: BTreeSet<String>) -> Result<Vec<CandidateKey>, String> {
    candidate_set
        .into_iter()
        .map(|hex| {
            let bytes =
                hex::decode(&hex).map_err(|e| format!("Invalid raw key candidate: {}", e))?;
            Ok(CandidateKey { hex, bytes })
        })
        .collect()
}

fn load_source_db_pages(db_storage: &std::path::Path) -> Result<Vec<SourceDbPage>, String> {
    let mut pages = Vec::new();
    for source in decrypt::collect_db_files(db_storage) {
        if let Some(page1) = decrypt::read_encrypted_page1(&source.full_path)? {
            pages.push(SourceDbPage {
                rel_path: source.rel_path,
                page1,
            });
        }
    }
    Ok(pages)
}

fn read_raw_candidate_keys(path: &std::path::Path) -> Result<Vec<String>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Cannot read raw key candidates {}: {}", path.display(), e))?;
    let mut candidates = BTreeSet::new();
    for line in content.lines() {
        let key = line.trim();
        if key.len() == 64 && key.as_bytes().iter().copied().all(is_hex_byte) {
            candidates.insert(key.to_ascii_lowercase());
        }
    }
    Ok(candidates.into_iter().collect())
}

fn zero_key_scan_message(stdout: &str, stderr: &str, keys_path: &std::path::Path) -> String {
    let mut message = format!(
        "Scanner completed but found 0 database keys; leaving {} unchanged.",
        keys_path.display()
    );
    append_labeled_output(&mut message, "Output", stdout);
    append_labeled_output(&mut message, "Error output", stderr);
    message.push_str(
        "\n\nThe desktop client may have changed its in-memory key format after an update. \
         Keep the client open and retry key extraction after updating tg.",
    );
    message
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
    append_scanner_recovery_hint(&mut message);
    message
}

#[cfg(target_os = "macos")]
fn lldb_capture_failure_message(code: Option<i32>, output: &str, error_output: &str) -> String {
    let mut message = format!(
        "lldb key capture failed with exit code {:?}.{}",
        code, output
    );
    append_lldb_recovery_hint(&mut message, error_output);
    message
}

#[cfg(target_os = "macos")]
fn append_lldb_recovery_hint(message: &mut String, error_output: &str) {
    let lower = error_output.to_ascii_lowercase();
    if !lower.contains("not allowed to attach")
        && !lower.contains("attach failed")
        && !lower.contains("task_for_pid")
    {
        return;
    }

    message.push_str(
        "\n\nmacOS denied the lldb/debugserver attach. This usually means Developer Tools \
         permission is not enabled for this terminal.\n\n",
    );
    message.push_str("Run once:\n\n");
    message.push_str("  sudo DevToolsSecurity -enable\n\n");
    message.push_str(
        "Then open System Settings -> Privacy & Security -> Developer Tools and enable your terminal app. \
         Quit and reopen that terminal, then rerun:\n\n",
    );
    message.push_str("  sudo tg keys --method lldb-cold --timeout 90");
}

#[cfg(target_os = "macos")]
fn append_scanner_recovery_hint(message: &mut String) {
    message.push_str("\n\nIf key extraction failed with `task_for_pid failed` or another macOS process permission error, quit Telegram, re-sign it, reopen it, then retry:\n\n");
    message.push_str("  sudo codesign --force --deep --sign - /Applications/Telegram.app\n");
    message.push_str("  sudo tg keys\n\n");
    message.push_str("If Telegram is installed somewhere else, use that `.app` path instead.");
}

#[cfg(target_os = "linux")]
fn append_scanner_recovery_hint(message: &mut String) {
    message.push_str("\n\nIf key extraction failed with a Linux process memory permission error, keep the desktop client open and retry with:\n\n");
    message.push_str("  sudo tg keys\n\n");
    message.push_str(
        "The scanner reads the local desktop process memory and matches keys to local DB salts.",
    );
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn append_scanner_recovery_hint(message: &mut String) {
    message.push_str("\n\ntg key extraction is only supported on macOS and Linux.");
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

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcMapRegion {
    start: u64,
    end: u64,
    readable: bool,
    writable: bool,
}

#[cfg(any(target_os = "linux", test))]
fn parse_proc_maps_region(line: &str) -> Option<ProcMapRegion> {
    let mut parts = line.split_whitespace();
    let range = parts.next()?;
    let perms = parts.next()?;
    let (start, end) = range.split_once('-')?;
    let start = u64::from_str_radix(start, 16).ok()?;
    let end = u64::from_str_radix(end, 16).ok()?;
    if start >= end {
        return None;
    }

    let mut chars = perms.chars();
    let readable = chars.next() == Some('r');
    let writable = chars.next() == Some('w');
    Some(ProcMapRegion {
        start,
        end,
        readable,
        writable,
    })
}

fn is_hex_byte(byte: u8) -> bool {
    byte.is_ascii_hexdigit()
}

#[cfg(any(target_os = "linux", test))]
fn ascii_hex_lower(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| (*byte as char).to_ascii_lowercase())
        .collect()
}

#[cfg(any(target_os = "linux", test))]
fn find_hex_key_patterns(bytes: &[u8]) -> Vec<(String, String)> {
    const HEX_PATTERN_LEN: usize = 96;
    const FULL_PATTERN_LEN: usize = 2 + HEX_PATTERN_LEN + 1;

    if bytes.len() < FULL_PATTERN_LEN {
        return Vec::new();
    }

    let mut keys = Vec::new();
    for i in 0..=bytes.len() - FULL_PATTERN_LEN {
        if bytes[i] != b'x' || bytes[i + 1] != b'\'' {
            continue;
        }
        let hex = &bytes[i + 2..i + 2 + HEX_PATTERN_LEN];
        if bytes[i + 2 + HEX_PATTERN_LEN] != b'\'' || !hex.iter().copied().all(is_hex_byte) {
            continue;
        }

        keys.push((ascii_hex_lower(&hex[..64]), ascii_hex_lower(&hex[64..])));
    }
    keys
}

#[cfg(target_os = "linux")]
mod linux_scanner {
    use super::{find_hex_key_patterns, find_telegram_pid, parse_proc_maps_region};
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::ffi::OsString;
    use std::fs::{self, File};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};

    const SQLITE_HDR: &[u8] = b"SQLite format 3\0";
    const CHUNK_SIZE: usize = 2 * 1024 * 1024;
    const PATTERN_OVERLAP: usize = 128;

    pub(super) fn run(args: Vec<OsString>) -> Result<(), String> {
        let (pid, db_storage_arg) = parse_args(args)?;
        let db_storage = match db_storage_arg {
            Some(path) => path,
            None => super::find_db_storage_dir()
                .ok_or_else(|| "Cannot auto-detect local DB directory.".to_string())?,
        };

        println!("============================================================");
        println!("  Linux local desktop key scanner");
        println!("============================================================");
        println!("Process PID: {}", pid);
        println!("DB storage directory: {}", db_storage.display());

        let salts = collect_db_salts(&db_storage)?;
        println!("Found {} encrypted DBs", salts.len());

        let candidates = scan_process(pid)?;
        println!("Found {} unique key candidates", candidates.len());

        let mut matched = BTreeMap::<String, BTreeMap<String, String>>::new();
        for (key_hex, salt_hex) in candidates {
            let Some(rel_path) = salts.get(&salt_hex) else {
                continue;
            };
            matched
                .entry(rel_path.clone())
                .or_default()
                .insert("enc_key".to_string(), key_hex);
        }

        let json = serde_json::to_string_pretty(&matched)
            .map_err(|e| format!("Cannot encode keys JSON: {}", e))?;
        let mut file = File::create("all_keys.json")
            .map_err(|e| format!("Cannot create all_keys.json: {}", e))?;
        file.write_all(json.as_bytes())
            .map_err(|e| format!("Cannot write all_keys.json: {}", e))?;
        file.write_all(b"\n")
            .map_err(|e| format!("Cannot finish all_keys.json: {}", e))?;

        println!("Saved {} matched keys to all_keys.json", matched.len());
        Ok(())
    }

    fn parse_args(args: Vec<OsString>) -> Result<(i32, Option<PathBuf>), String> {
        match args.as_slice() {
            [] => Ok((find_telegram_pid()?, None)),
            [one] => {
                if let Some(pid) = one.to_str().and_then(|value| value.parse::<i32>().ok()) {
                    if pid > 0 {
                        return Ok((pid, None));
                    }
                }
                Ok((find_telegram_pid()?, Some(PathBuf::from(one))))
            }
            [pid, path, ..] => {
                let pid = pid
                    .to_str()
                    .ok_or_else(|| "PID argument is not valid UTF-8".to_string())?
                    .parse::<i32>()
                    .map_err(|e| format!("Invalid PID argument: {}", e))?;
                if pid <= 0 {
                    return Err("PID argument must be positive".to_string());
                }
                Ok((pid, Some(PathBuf::from(path))))
            }
        }
    }

    fn collect_db_salts(db_storage: &Path) -> Result<HashMap<String, String>, String> {
        let mut salts = HashMap::new();
        collect_db_salts_in(db_storage, db_storage, &mut salts)?;
        Ok(salts)
    }

    fn collect_db_salts_in(
        dir: &Path,
        base: &Path,
        salts: &mut HashMap<String, String>,
    ) -> Result<(), String> {
        let entries = fs::read_dir(dir)
            .map_err(|e| format!("Cannot read DB directory {}: {}", dir.display(), e))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_db_salts_in(&path, base, salts)?;
                continue;
            }
            if path.extension().and_then(|value| value.to_str()) != Some("db") {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            if name.ends_with("-wal") || name.ends_with("-shm") {
                continue;
            }

            let Ok(header) = read_header(&path) else {
                continue;
            };
            if header.as_slice() == SQLITE_HDR {
                continue;
            }
            let rel_path = path
                .strip_prefix(base)
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_else(|_| name.to_string());
            salts.insert(hex::encode(header), rel_path);
        }
        Ok(())
    }

    fn read_header(path: &Path) -> Result<[u8; 16], String> {
        let mut file =
            File::open(path).map_err(|e| format!("Cannot open {}: {}", path.display(), e))?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header)
            .map_err(|e| format!("Cannot read {}: {}", path.display(), e))?;
        Ok(header)
    }

    fn scan_process(pid: i32) -> Result<Vec<(String, String)>, String> {
        let maps_path = format!("/proc/{}/maps", pid);
        let mem_path = format!("/proc/{}/mem", pid);
        let maps = fs::read_to_string(&maps_path)
            .map_err(|e| format!("Cannot read {}: {}", maps_path, e))?;
        let mut mem = File::open(&mem_path).map_err(|e| {
            format!(
                "Cannot open {}: {}. Try running `sudo tg keys` while the desktop client is open.",
                mem_path, e
            )
        })?;

        let mut found = HashSet::new();
        let mut buffer = vec![0u8; CHUNK_SIZE];
        for line in maps.lines() {
            let Some(region) = parse_proc_maps_region(line) else {
                continue;
            };
            if !region.readable || !region.writable {
                continue;
            }

            let mut offset = region.start;
            let mut tail = Vec::new();
            while offset < region.end {
                let to_read = (region.end - offset).min(CHUNK_SIZE as u64) as usize;
                if mem.seek(SeekFrom::Start(offset)).is_err() {
                    offset += to_read as u64;
                    tail.clear();
                    continue;
                }

                let read = match mem.read(&mut buffer[..to_read]) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(_) => {
                        offset += to_read as u64;
                        tail.clear();
                        continue;
                    }
                };

                let mut chunk = Vec::with_capacity(tail.len() + read);
                chunk.extend_from_slice(&tail);
                chunk.extend_from_slice(&buffer[..read]);
                for candidate in find_hex_key_patterns(&chunk) {
                    found.insert(candidate);
                }

                let keep = chunk.len().min(PATTERN_OVERLAP);
                tail.clear();
                tail.extend_from_slice(&chunk[chunk.len() - keep..]);
                offset += read as u64;
            }
        }

        let mut keys = found.into_iter().collect::<Vec<_>>();
        keys.sort();
        Ok(keys)
    }
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
        assert!(message.contains("sudo tg keys"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn lldb_attach_denial_message_points_to_developer_tools_permission() {
        let message = lldb_capture_failure_message(
            Some(1),
            "\nlldb output:\n(lldb) process attach --pid 123\n",
            "error: attach failed (Not allowed to attach to process.)",
        );

        assert!(message.contains("lldb key capture failed with exit code Some(1)."));
        assert!(message.contains("sudo DevToolsSecurity -enable"));
        assert!(message.contains("Developer Tools"));
        assert!(message.contains("sudo tg keys --method lldb-cold --timeout 90"));
    }

    #[test]
    fn empty_scanner_json_has_no_usable_keys() {
        assert_eq!(count_encryption_keys("{}").unwrap(), 0);
    }

    #[test]
    fn scanner_json_counts_only_non_empty_encryption_keys() {
        let content = r#"{
          "message/message_0.db": {"enc_key": ""},
          "message/message_1.db": {"enc_key": "abc"},
          "contact/contact.db": {}
        }"#;

        assert_eq!(count_encryption_keys(content).unwrap(), 1);
    }

    #[test]
    fn raw_candidate_reader_filters_and_normalizes_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("candidate_keys.txt");
        let key_upper = "A".repeat(64);
        let key_lower = "b".repeat(64);
        std::fs::write(
            &path,
            format!("{key_upper}\nnot-a-key\n{}\n{key_lower}\n", "c".repeat(63)),
        )
        .unwrap();

        assert_eq!(
            read_raw_candidate_keys(&path).unwrap(),
            vec!["a".repeat(64), "b".repeat(64)]
        );
    }

    #[test]
    fn process_table_parser_keeps_full_command() {
        assert_eq!(
            parse_process_table_line("  11  10 /App/Helper --flag value").unwrap(),
            (11, 10, "/App/Helper --flag value".to_string())
        );
    }

    #[test]
    fn key_scan_process_filter_skips_renderer_gpu_and_network_helpers() {
        let account_arg = format!(
            "--account-files-path=/tmp/{}",
            crate::dictionary::account_files_dir()
        );
        let processes = vec![
            (10, 1, "desktop-main".to_string()),
            (11, 10, format!("app-extension {account_arg}")),
            (
                12,
                11,
                "helper --type=utility --service-sandbox-type=network".to_string(),
            ),
            (13, 11, "helper --type=gpu-process".to_string()),
            (14, 11, "helper --type=renderer".to_string()),
            (
                15,
                11,
                "helper --type=utility --service-sandbox-type=none".to_string(),
            ),
            (
                16,
                15,
                "nested helper --type=utility --service-sandbox-type=none".to_string(),
            ),
        ];

        assert_eq!(
            key_scan_pids_from_process_table(10, &processes),
            vec![10, 11, 15, 16]
        );
    }

    #[test]
    fn proc_maps_parser_reads_range_and_permissions() {
        let region =
            parse_proc_maps_region("aaaab75b0000-aaaab75d2000 rw-p 00000000 00:00 0 [heap]")
                .unwrap();

        assert_eq!(region.start, 0xaaaab75b0000);
        assert_eq!(region.end, 0xaaaab75d2000);
        assert!(region.readable);
        assert!(region.writable);
    }

    #[test]
    fn hex_key_pattern_scanner_extracts_lowercase_key_and_salt() {
        let key = "A".repeat(64);
        let salt = "B".repeat(32);
        let data = format!("prefix x'{}{}' suffix", key, salt);

        let found = find_hex_key_patterns(data.as_bytes());

        assert_eq!(found, vec![("a".repeat(64), "b".repeat(32))]);
    }

    #[test]
    fn hex_key_pattern_scanner_rejects_non_hex_payloads() {
        let data = format!("x'{}{}'", "g".repeat(64), "0".repeat(32));

        assert!(find_hex_key_patterns(data.as_bytes()).is_empty());
    }
}
