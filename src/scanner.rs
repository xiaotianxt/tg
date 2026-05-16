#[cfg(target_os = "macos")]
use std::ffi::CString;
use std::ffi::{OsStr, OsString};
#[cfg(target_os = "macos")]
use std::os::raw::{c_char, c_int};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{dictionary, paths};

const INTERNAL_SCAN_ARG: &str = "__tg-scan-keys";

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
    append_scanner_recovery_hint(&mut message);
    message
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

#[cfg(any(target_os = "linux", test))]
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
