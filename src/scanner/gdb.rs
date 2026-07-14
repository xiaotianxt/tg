use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use zeroize::Zeroize;

use crate::key_material::AccountKeyMaterial;

use super::elf_locator::CaptureArchitecture;
use super::linux_process::ProcessIdentity;

const ACCOUNT_MATERIAL_LENGTH: usize = 32;
const MAX_DEBUG_OUTPUT_BYTES: usize = 64 * 1024;
const TERMINATION_GRACE: Duration = Duration::from_secs(2);
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(super) struct CaptureRequest<'a> {
    pub(super) identity: &'a ProcessIdentity,
    pub(super) gdb_path: &'a Path,
    pub(super) work_dir: &'a Path,
    pub(super) runtime_address: u64,
    pub(super) code_fingerprint: &'a [u8; 16],
    pub(super) architecture: CaptureArchitecture,
    pub(super) timeout_secs: u64,
}

pub(super) fn capture_account_material(
    request: CaptureRequest<'_>,
) -> Result<AccountKeyMaterial, String> {
    let script_path = request.work_dir.join("capture_account_material.py");
    let candidate_path = request.work_dir.join("captured_material.bin");
    let log_path = request.work_dir.join("capture.log");
    write_private_new_file(&script_path, GDB_CAPTURE_SCRIPT.as_bytes())?;
    write_private_new_file(&candidate_path, b"")?;
    write_private_new_file(&log_path, b"")?;

    let architecture = match request.architecture {
        CaptureArchitecture::X86_64 => "x86_64",
        CaptureArchitecture::Aarch64 => "aarch64",
    };
    let parent_pid = std::process::id() as libc::pid_t;
    let mut command = Command::new(request.gdb_path);
    command
        .env_clear()
        .env("TERM", "dumb")
        .env("TG_GDB_CAPTURE_OUTPUT", &candidate_path)
        .env("TG_GDB_CAPTURE_LOG", &log_path)
        .env(
            "TG_GDB_CAPTURE_ADDRESS",
            format!("{:#x}", request.runtime_address),
        )
        .env("TG_GDB_CAPTURE_ARCH", architecture)
        .env("TG_GDB_EXPECTED_PID", request.identity.pid.to_string())
        .env("TG_GDB_EXPECTED_UID", request.identity.uid.to_string())
        .env(
            "TG_GDB_EXPECTED_START_TIME",
            request.identity.start_time_ticks.to_string(),
        )
        .env(
            "TG_GDB_EXPECTED_DEVICE",
            request.identity.executable_device.to_string(),
        )
        .env(
            "TG_GDB_EXPECTED_INODE",
            request.identity.executable_inode.to_string(),
        )
        .env(
            "TG_GDB_EXPECTED_CODE",
            hex::encode(request.code_fingerprint),
        )
        .arg("-q")
        .arg("--nx")
        .arg("-batch")
        .arg("-ex")
        .arg("set pagination off")
        .arg("-ex")
        .arg("set confirm off")
        .arg("-ex")
        .arg("set print thread-events off")
        .arg("-ex")
        .arg(format!("attach {}", request.identity.pid))
        .arg("-x")
        .arg(&script_path)
        .arg("-ex")
        .arg("continue")
        .arg("-ex")
        .arg("detach")
        .current_dir(request.work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);

    // SAFETY: `pre_exec` runs after fork and before exec. The closure only calls
    // async-signal-safe Linux syscalls. PDEATHSIG guarantees that an abrupt tg
    // exit cannot leave a privileged debugger attached to the desktop process.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent_pid {
                return Err(std::io::Error::other("parent exited before debugger exec"));
            }
            Ok(())
        });
    }

    let child = command
        .spawn()
        .map_err(|error| format!("Failed to start the trusted system debugger: {error}"))?;
    let output = wait_for_debugger(child, request.timeout_secs);
    let material_error = match read_captured_account_material(&candidate_path) {
        Ok(material) => return Ok(material),
        Err(error) => error,
    };

    match output {
        Ok(output) => {
            log_debugger_output(&output);
            let mut error = format!(
                "Debugger capture exited with {:?}, but no valid material was available: {material_error}.{}",
                output.status.code(),
                capture_log_summary(&log_path)
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            if attach_was_denied(&stderr) {
                error.push_str(
                    "\nLinux denied debugger attach. Run the command through sudo from the desktop user and check the system ptrace policy.",
                );
            }
            Err(error)
        }
        Err(error) => Err(format!(
            "{error}; captured material status: {material_error}.{}",
            capture_log_summary(&log_path)
        )),
    }
}

fn write_private_new_file(path: &Path, content: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("Cannot create private debugger file: {error}"))?;
    file.write_all(content)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("Cannot write private debugger file: {error}"))
}

fn read_captured_account_material(path: &Path) -> Result<AccountKeyMaterial, String> {
    let mut content = std::fs::read(path)
        .map_err(|error| format!("cannot read private capture output: {error}"))?;
    if content.len() != ACCOUNT_MATERIAL_LENGTH {
        let length = content.len();
        content.zeroize();
        return Err(format!(
            "expected {ACCOUNT_MATERIAL_LENGTH} captured bytes, found {length}"
        ));
    }
    let mut bytes = [0u8; ACCOUNT_MATERIAL_LENGTH];
    bytes.copy_from_slice(&content);
    content.zeroize();
    Ok(AccountKeyMaterial::from_bytes(bytes))
}

struct ProcessGroupGuard {
    child: Child,
    process_group: libc::pid_t,
    reaped: bool,
}

impl ProcessGroupGuard {
    fn new(child: Child) -> Result<Self, String> {
        let process_group = libc::pid_t::try_from(child.id())
            .map_err(|_| "Debugger process id is out of range".to_string())?;
        Ok(Self {
            child,
            process_group,
            reaped: false,
        })
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>, String> {
        let status = self
            .child
            .try_wait()
            .map_err(|error| format!("Failed to wait for debugger: {error}"))?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status)
    }

    fn wait(&mut self) -> Result<ExitStatus, String> {
        let status = self
            .child
            .wait()
            .map_err(|error| format!("Failed to reap debugger: {error}"))?;
        self.reaped = true;
        Ok(status)
    }

    fn signal_group(&self, signal: libc::c_int) -> Result<(), String> {
        // SAFETY: the child was placed in a process group whose id equals its
        // pid before exec. A negative pid targets exactly that process group.
        let result = unsafe { libc::kill(-self.process_group, signal) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(format!("Cannot signal debugger process group: {error}"))
        }
    }

    fn terminate(&mut self) -> Result<ExitStatus, String> {
        self.signal_group(libc::SIGTERM)?;
        let started = Instant::now();
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            if started.elapsed() >= TERMINATION_GRACE {
                break;
            }
            std::thread::sleep(OUTPUT_POLL_INTERVAL);
        }
        self.signal_group(libc::SIGKILL)?;
        self.wait()
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        let _signal_result = self.signal_group(libc::SIGKILL);
        let _wait_result = self.child.wait();
        self.reaped = true;
    }
}

fn wait_for_debugger(mut child: Child, timeout_secs: u64) -> Result<Output, String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Debugger stdout pipe is unavailable".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Debugger stderr pipe is unavailable".to_string())?;
    let stdout_reader = spawn_bounded_reader(stdout);
    let stderr_reader = spawn_bounded_reader(stderr);
    let mut child = ProcessGroupGuard::new(child)?;
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(timeout_secs) {
            timed_out = true;
            break child.terminate()?;
        }
        std::thread::sleep(OUTPUT_POLL_INTERVAL);
    };
    let stdout = join_reader(stdout_reader, "stdout")?;
    let stderr = join_reader(stderr_reader, "stderr")?;
    let output = Output {
        status,
        stdout,
        stderr,
    };
    log_debugger_output(&output);
    if timed_out {
        return Err(format!(
            "Debugger capture timed out after {timeout_secs} seconds"
        ));
    }
    Ok(output)
}

fn spawn_bounded_reader<R>(mut reader: R) -> JoinHandle<Result<Vec<u8>, std::io::Error>>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut captured = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                return Ok(captured);
            }
            let remaining = MAX_DEBUG_OUTPUT_BYTES.saturating_sub(captured.len());
            captured.extend_from_slice(&buffer[..read.min(remaining)]);
        }
    })
}

fn join_reader(
    reader: JoinHandle<Result<Vec<u8>, std::io::Error>>,
    label: &str,
) -> Result<Vec<u8>, String> {
    reader
        .join()
        .map_err(|_| format!("Debugger {label} reader panicked"))?
        .map_err(|error| format!("Cannot read debugger {label}: {error}"))
}

fn log_debugger_output(output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().is_empty() {
        log::debug!("Debugger stdout (bounded):\n{}", stdout.trim_end());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        log::debug!("Debugger stderr (bounded):\n{}", stderr.trim_end());
    }
}

fn attach_was_denied(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("operation not permitted")
        || lower.contains("ptrace")
        || lower.contains("could not attach")
}

fn capture_log_summary(path: &Path) -> String {
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut content = String::new();
    if file
        .take(MAX_DEBUG_OUTPUT_BYTES as u64)
        .read_to_string(&mut content)
        .is_err()
    {
        return String::new();
    }
    let mut installed = false;
    let mut nonmatching_hits = 0;
    let mut captured = false;
    for line in content.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match record.get("event").and_then(|value| value.as_str()) {
            Some("installed") => installed = true,
            Some("ignored") => nonmatching_hits += 1,
            Some("captured") => captured = true,
            _ => {}
        }
    }
    format!(
        "\ndebugger summary: installed={installed}, ignored_hits={nonmatching_hits}, captured={captured}"
    )
}

const GDB_CAPTURE_SCRIPT: &str = r#"
import gdb
import json
import os

OUTPUT_PATH = os.environ["TG_GDB_CAPTURE_OUTPUT"]
LOG_PATH = os.environ["TG_GDB_CAPTURE_LOG"]
ADDRESS = int(os.environ["TG_GDB_CAPTURE_ADDRESS"], 0)
ARCHITECTURE = os.environ["TG_GDB_CAPTURE_ARCH"]
EXPECTED_PID = int(os.environ["TG_GDB_EXPECTED_PID"])
EXPECTED_UID = int(os.environ["TG_GDB_EXPECTED_UID"])
EXPECTED_START_TIME = int(os.environ["TG_GDB_EXPECTED_START_TIME"])
EXPECTED_DEVICE = int(os.environ["TG_GDB_EXPECTED_DEVICE"])
EXPECTED_INODE = int(os.environ["TG_GDB_EXPECTED_INODE"])
EXPECTED_CODE = bytes.fromhex(os.environ["TG_GDB_EXPECTED_CODE"])
ACCOUNT_MATERIAL_LENGTH = 32
IGNORED_LOG_LIMIT = 16
ignored_hits = 0


def _log(record):
    with open(LOG_PATH, "a", encoding="utf-8") as output:
        output.write(json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n")
        output.flush()


def _ignored(reason):
    global ignored_hits
    ignored_hits += 1
    if ignored_hits <= IGNORED_LOG_LIMIT:
        _log({"event": "ignored", "reason": reason})


def _status_uid(pid):
    with open("/proc/{}/status".format(pid), "r", encoding="utf-8") as status:
        for line in status:
            if line.startswith("Uid:"):
                return int(line.split()[1])
    return -1


def _start_time(pid):
    with open("/proc/{}/stat".format(pid), "r", encoding="utf-8") as stat:
        value = stat.read()
    fields = value[value.rfind(")") + 1:].split()
    return int(fields[19])


def _verify_identity(inferior):
    if inferior.pid != EXPECTED_PID:
        raise gdb.GdbError("desktop process pid changed")
    metadata = os.stat("/proc/{}/exe".format(EXPECTED_PID))
    matches = (
        _status_uid(EXPECTED_PID) == EXPECTED_UID
        and _start_time(EXPECTED_PID) == EXPECTED_START_TIME
        and metadata.st_dev == EXPECTED_DEVICE
        and metadata.st_ino == EXPECTED_INODE
    )
    if not matches:
        raise gdb.GdbError("desktop process identity changed")
    code = inferior.read_memory(ADDRESS, len(EXPECTED_CODE)).tobytes()
    if code != EXPECTED_CODE:
        raise gdb.GdbError("desktop process code fingerprint changed")


def _register(name):
    return int(gdb.parse_and_eval("$" + name))


class AccountMaterialBreakpoint(gdb.Breakpoint):
    def stop(self):
        inferior = gdb.selected_inferior()
        try:
            if ARCHITECTURE == "aarch64":
                key_pointer = _register("x1")
                key_length = _register("x2")
            elif ARCHITECTURE == "x86_64":
                key_pointer = _register("rsi")
                key_length = _register("rdx")
            else:
                _ignored("architecture")
                return False

            if not key_pointer or key_length != ACCOUNT_MATERIAL_LENGTH:
                _ignored("shape")
                return False
            material = inferior.read_memory(
                key_pointer, ACCOUNT_MATERIAL_LENGTH
            ).tobytes()
        except (gdb.error, gdb.MemoryError):
            _ignored("read")
            return False

        with open(OUTPUT_PATH, "r+b") as output:
            output.seek(0)
            output.write(material)
            output.truncate()
            output.flush()
            os.fsync(output.fileno())
        _log({"event": "captured"})
        return True


inferior = gdb.selected_inferior()
_verify_identity(inferior)
AccountMaterialBreakpoint("*{:#x}".format(ADDRESS), internal=True)
_log({"event": "installed"})
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_requires_direct_pointer_length_and_rechecks_process_identity() {
        assert!(GDB_CAPTURE_SCRIPT.contains("key_length != ACCOUNT_MATERIAL_LENGTH"));
        assert!(GDB_CAPTURE_SCRIPT.contains("_verify_identity(inferior)"));
        assert!(GDB_CAPTURE_SCRIPT.contains("EXPECTED_CODE"));
        assert!(GDB_CAPTURE_SCRIPT.contains("IGNORED_LOG_LIMIT"));
        assert!(GDB_CAPTURE_SCRIPT.contains("ARCHITECTURE == \"aarch64\""));
        assert!(GDB_CAPTURE_SCRIPT.contains("ARCHITECTURE == \"x86_64\""));
        assert!(!GDB_CAPTURE_SCRIPT.contains("material.hex"));
    }

    #[test]
    fn captured_material_error_preserves_observed_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.bin");
        std::fs::write(&path, [0xa5; ACCOUNT_MATERIAL_LENGTH]).unwrap();

        let material = read_captured_account_material(&path).unwrap();
        assert_eq!(material.as_bytes(), &[0xa5; ACCOUNT_MATERIAL_LENGTH]);

        std::fs::write(&path, [0xa5; ACCOUNT_MATERIAL_LENGTH - 1]).unwrap();
        let error = match read_captured_account_material(&path) {
            Ok(_) => panic!("truncated material must fail"),
            Err(error) => error,
        };
        assert!(error.contains("found 31"));
    }
}
