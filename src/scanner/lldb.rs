use std::path::Path;
use std::process::{Command, Stdio};
use zeroize::Zeroize;

use crate::key_material::AccountKeyMaterial;

pub(super) struct CaptureRequest<'a> {
    pub(super) pid: i32,
    pub(super) lldb_path: &'a Path,
    pub(super) work_dir: &'a Path,
    pub(super) timeout_secs: u64,
}

pub(super) fn capture_account_material(
    request: CaptureRequest<'_>,
) -> Result<AccountKeyMaterial, String> {
    let script_path = request.work_dir.join("capture_account_material.py");
    let candidate_path = request.work_dir.join("captured_material.bin");
    let log_path = request.work_dir.join("capture.log");
    std::fs::write(&script_path, LLDB_CAPTURE_SCRIPT)
        .map_err(|e| format!("Cannot write lldb capture script: {e}"))?;
    std::fs::write(&candidate_path, b"")
        .map_err(|e| format!("Cannot create lldb capture output: {e}"))?;
    super::restrict_key_file_permissions(&script_path)
        .map_err(|e| format!("Cannot restrict lldb capture script permissions: {e}"))?;
    super::restrict_key_file_permissions(&candidate_path)
        .map_err(|e| format!("Cannot restrict lldb capture output permissions: {e}"))?;

    let mut command = Command::new("sudo");
    command
        .arg("-n")
        .arg("env")
        .arg("TERM=dumb")
        .arg(format!(
            "TG_LLDB_CAPTURE_OUTPUT={}",
            candidate_path.display()
        ))
        .arg(format!("TG_LLDB_CAPTURE_LOG={}", log_path.display()))
        .arg(request.lldb_path)
        .arg("-b")
        .arg("-o")
        .arg(format!("command script import {}", script_path.display()))
        .arg("-o")
        .arg(format!("process attach --pid {}", request.pid))
        .arg("-o")
        .arg("capture_tg_account_material")
        .arg("-o")
        .arg("process continue")
        .current_dir(request.work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = command
        .spawn()
        .map_err(|e| format!("Failed to run lldb account material capture: {e}"))?;
    let output = super::wait_child_with_output(
        child,
        request.timeout_secs.saturating_add(10),
        "lldb account material capture",
    );

    if let Ok(material) = read_captured_account_material(&candidate_path) {
        return Ok(material);
    }

    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = format!(
                "lldb account material capture exited with {:?}.{}{}{}",
                output.status.code(),
                labeled_output("lldb output", &stdout),
                labeled_output("lldb error output", &stderr),
                capture_log_summary(&log_path)
            );
            Err(with_attach_recovery_hint(message, &stderr))
        }
        Err(error) => Err(format!("{error}.{}", capture_log_summary(&log_path))),
    }
}

fn read_captured_account_material(path: &Path) -> Result<AccountKeyMaterial, String> {
    let mut content =
        std::fs::read(path).map_err(|e| format!("Cannot read captured key material: {e}"))?;
    if content.len() != 32 {
        content.zeroize();
        return Err("lldb did not capture matching key material".to_string());
    }
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&content);
    content.zeroize();
    Ok(AccountKeyMaterial::from_bytes(bytes))
}

fn labeled_output(label: &str, output: &str) -> String {
    if output.trim().is_empty() {
        String::new()
    } else {
        format!("\n{label}:\n{}", output.trim_end())
    }
}

fn capture_log_summary(path: &Path) -> String {
    let Ok(content) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let mut installed_locations = 0;
    let mut nonmatching_hits = 0;
    let mut captured = false;
    for line in content.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match record.get("event").and_then(|value| value.as_str()) {
            Some("installed") => {
                installed_locations = record
                    .get("locations")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
            }
            Some("ignored") => nonmatching_hits += 1,
            Some("captured") => captured = true,
            _ => {}
        }
    }
    format!(
        "\nlldb summary: locations={installed_locations}, ignored_hits={nonmatching_hits}, captured={captured}"
    )
}

fn with_attach_recovery_hint(mut message: String, error_output: &str) -> String {
    let lower = error_output.to_ascii_lowercase();
    if !lower.contains("not allowed to attach")
        && !lower.contains("attach failed")
        && !lower.contains("task_for_pid")
    {
        return message;
    }

    message.push_str(
        "\n\nmacOS denied the lldb/debugserver attach. Enable Developer Tools access for \
         this terminal, restart the terminal, and retry. You can initialize the system setting \
         with:\n\n  sudo DevToolsSecurity -enable\n\nThen rerun:\n\n  \
         sudo tg keys --method login --timeout 180",
    );
    message
}

const LLDB_CAPTURE_SCRIPT: &str = r#"
import json
import os

import lldb

OUTPUT_PATH = os.environ["TG_LLDB_CAPTURE_OUTPUT"]
LOG_PATH = os.environ["TG_LLDB_CAPTURE_LOG"]


def _log(record):
    with open(LOG_PATH, "a", encoding="utf-8") as output:
        output.write(json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n")
        output.flush()


def _register(frame, name):
    register = frame.FindRegister(name)
    if not register or not register.IsValid():
        return 0
    return register.GetValueAsUnsigned()


def _stack_u64(process, address):
    error = lldb.SBError()
    raw = process.ReadMemory(address, 8, error)
    if not error.Success() or not raw or len(raw) != 8:
        return 0
    return int.from_bytes(raw, byteorder="little", signed=False)


def _stack_u32(process, address):
    error = lldb.SBError()
    raw = process.ReadMemory(address, 4, error)
    if not error.Success() or not raw or len(raw) != 4:
        return 0
    return int.from_bytes(raw, byteorder="little", signed=False)


def _call_shape(frame):
    process = frame.GetThread().GetProcess()
    triple = process.GetTarget().GetTriple().lower()
    if "arm64" in triple or "aarch64" in triple:
        algorithm = _register(frame, "x0")
        password_ptr = _register(frame, "x1")
        password_len = _register(frame, "x2")
        salt_len = _register(frame, "x4")
        prf = _register(frame, "x5")
        rounds = _register(frame, "x6")
        derived_key_len = _stack_u64(process, _register(frame, "sp"))
    elif "x86_64" in triple:
        algorithm = _register(frame, "rdi")
        password_ptr = _register(frame, "rsi")
        password_len = _register(frame, "rdx")
        salt_len = _register(frame, "r8")
        prf = _register(frame, "r9")
        stack_pointer = _register(frame, "rsp")
        rounds = _stack_u32(process, stack_pointer + 8)
        derived_key_len = _stack_u64(process, stack_pointer + 24)
    else:
        return None

    matches = (
        algorithm == 2
        and password_len == 32
        and salt_len == 16
        and prf == 5
        and rounds == 256000
        and derived_key_len == 32
    )
    return process, password_ptr, password_len, matches


def on_pbkdf2(frame, bp_loc, internal_dict):
    shape = _call_shape(frame)
    if shape is None:
        _log({"event": "ignored", "reason": "unsupported_arch"})
        return False
    process, password_ptr, password_len, matches = shape
    if not matches or not password_ptr:
        _log({"event": "ignored", "reason": "shape"})
        return False

    error = lldb.SBError()
    material = process.ReadMemory(password_ptr, int(password_len), error)
    if not error.Success() or not material or len(material) != 32:
        _log({"event": "ignored", "reason": "read"})
        return False

    with open(OUTPUT_PATH, "wb") as output:
        output.write(material)
        output.flush()
    _log({"event": "captured"})
    process.Detach()
    os._exit(0)


def install(debugger, command, result, internal_dict):
    open(LOG_PATH, "w", encoding="utf-8").close()
    target = debugger.GetSelectedTarget()
    breakpoint = target.BreakpointCreateByName("CCKeyDerivationPBKDF")
    breakpoint.SetScriptCallbackFunction("capture_account_material.on_pbkdf2")
    breakpoint.SetAutoContinue(True)
    locations = breakpoint.GetNumLocations()
    _log({"event": "installed", "locations": locations})
    if locations == 0:
        result.SetError("cannot resolve CCKeyDerivationPBKDF")
        return
    result.PutCString("installed PBKDF2 account material capture")


def __lldb_init_module(debugger, internal_dict):
    debugger.HandleCommand("command script add -f capture_account_material.install capture_tg_account_material")
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_uses_stable_system_symbol_and_exact_kdf_shape() {
        assert!(LLDB_CAPTURE_SCRIPT.contains("BreakpointCreateByName(\"CCKeyDerivationPBKDF\")"));
        assert!(LLDB_CAPTURE_SCRIPT.contains("algorithm == 2"));
        assert!(LLDB_CAPTURE_SCRIPT.contains("password_len == 32"));
        assert!(LLDB_CAPTURE_SCRIPT.contains("salt_len == 16"));
        assert!(LLDB_CAPTURE_SCRIPT.contains("prf == 5"));
        assert!(LLDB_CAPTURE_SCRIPT.contains("rounds == 256000"));
        assert!(LLDB_CAPTURE_SCRIPT.contains("derived_key_len == 32"));
        assert!(!LLDB_CAPTURE_SCRIPT.contains("KEY_SETUP_OFFSET"));
    }

    #[test]
    fn attach_denial_points_to_developer_tools_permission() {
        let message = with_attach_recovery_hint(
            "lldb capture failed".to_string(),
            "error: attach failed (Not allowed to attach to process.)",
        );

        assert!(message.contains("sudo DevToolsSecurity -enable"));
        assert!(message.contains("Developer Tools"));
        assert!(message.contains("sudo tg keys --method login --timeout 180"));
    }

    #[test]
    fn captured_material_must_be_exactly_32_binary_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.bin");
        std::fs::write(&path, [0xa5; 32]).unwrap();

        let material = read_captured_account_material(&path).unwrap();
        assert_eq!(material.as_bytes(), &[0xa5; 32]);

        std::fs::write(&path, [0xa5; 31]).unwrap();
        assert!(read_captured_account_material(&path).is_err());
    }
}
