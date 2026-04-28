use std::path::{Path, PathBuf};
use std::process::Command;

/// Extract DB encryption keys from Telegram process memory.
/// Runs the C scanner binary with sudo, parses its output.
pub fn extract_keys(scanner_path: &Path, _timeout_secs: u64) -> Result<String, String> {
    if !scanner_path.exists() {
        return Err(format!(
            "Scanner binary not found at {}. Run 'make scanner' first.",
            scanner_path.display()
        ));
    }

    // Find Telegram PID first
    let pid = find_telegram_pid().map_err(|e| format!("Cannot find Telegram process: {}", e))?;
    println!("Telegram PID: {}", pid);

    // Run the C scanner with sudo
    println!("Running key scanner (requires sudo)...");
    println!("You may be prompted for your password.\n");

    let output = Command::new("sudo")
        .arg(scanner_path)
        .arg(format!("{}", pid))
        .arg("/dev/null") // redirect stderr
        .output()
        .map_err(|e| format!("Failed to run scanner: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !stderr.is_empty() {
        eprintln!("[stderr] {}", stderr);
    }

    if !output.status.success() {
        return Err(format!(
            "Scanner failed with exit code {:?}.\nOutput: {}",
            output.status.code(),
            stdout
        ));
    }

    // Print scanner output
    println!("{}", stdout);

    // Check for all_keys.json
    let keys_path = PathBuf::from("all_keys.json");
    if keys_path.exists() {
        // Count keys
        let content = std::fs::read_to_string(&keys_path)
            .map_err(|e| format!("Cannot read keys file: {}", e))?;
        let key_count = content.matches("\"enc_key\"").count();
        println!("\nFound {} database keys.", key_count);
        Ok(keys_path.to_string_lossy().to_string())
    } else {
        Err("all_keys.json not found. Key extraction may have failed.".to_string())
    }
}

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
