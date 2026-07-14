use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProcessIdentity {
    pub(super) pid: i32,
    pub(super) uid: u32,
    pub(super) parent_pid: i32,
    pub(super) start_time_ticks: u64,
    pub(super) executable_device: u64,
    pub(super) executable_inode: u64,
    pub(super) executable_path: PathBuf,
}

impl ProcessIdentity {
    pub(super) fn proc_exe_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/{}/exe", self.pid))
    }

    pub(super) fn verify(&self) -> Result<(), String> {
        let current = capture_identity(self.pid, self.uid)?;
        if current.start_time_ticks != self.start_time_ticks
            || current.executable_device != self.executable_device
            || current.executable_inode != self.executable_inode
        {
            return Err("Desktop client process changed during capture setup; retry.".to_string());
        }
        Ok(())
    }
}

pub(super) fn find_invoking_user_main_process(
    process_name: &str,
) -> Result<ProcessIdentity, String> {
    let uid = invoking_user_uid()?;
    let entries = std::fs::read_dir("/proc")
        .map_err(|error| format!("Cannot enumerate Linux processes: {error}"))?;
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) else {
            continue;
        };
        if comm.trim_end() != process_name {
            continue;
        }
        if let Ok(identity) = capture_identity(pid, uid) {
            candidates.push(identity);
        }
    }
    if candidates.is_empty() {
        return Err("Desktop client is not running for the invoking user.".to_string());
    }

    let candidate_pids = candidates
        .iter()
        .map(|candidate| candidate.pid)
        .collect::<BTreeSet<_>>();
    let mut roots = candidates
        .into_iter()
        .filter(|candidate| !candidate_pids.contains(&candidate.parent_pid))
        .collect::<Vec<_>>();
    roots.sort_by_key(|candidate| candidate.start_time_ticks);
    match roots.len() {
        1 => Ok(roots.remove(0)),
        count => Err(format!(
            "Found {count} independent desktop client processes for the invoking user; close duplicates and retry."
        )),
    }
}

fn invoking_user_uid() -> Result<u32, String> {
    let output = std::process::Command::new("id")
        .arg("-u")
        .output()
        .map_err(|error| format!("Cannot determine current uid: {error}"))?;
    let current_uid = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .map_err(|error| format!("Cannot parse current uid: {error}"))?;
    if current_uid != 0 {
        return Err(
            "Linux login capture must be started with `sudo tg keys --method login`.".to_string(),
        );
    }
    let uid = std::env::var("SUDO_UID")
        .map_err(|_| "Cannot identify the invoking desktop user from SUDO_UID.".to_string())?
        .parse::<u32>()
        .map_err(|error| format!("Invalid SUDO_UID: {error}"))?;
    if uid == 0 {
        return Err(
            "Run login capture through sudo from the desktop user, not a root shell.".to_string(),
        );
    }
    Ok(uid)
}

fn capture_identity(pid: i32, expected_uid: u32) -> Result<ProcessIdentity, String> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .map_err(|error| format!("Cannot read desktop process status: {error}"))?;
    let uid =
        parse_status_uid(&status).ok_or_else(|| "Desktop process status has no uid".to_string())?;
    if uid != expected_uid {
        return Err("Desktop process belongs to another user".to_string());
    }
    if status
        .lines()
        .find(|line| line.starts_with("State:"))
        .is_some_and(|line| line.contains(" Z ") || line.contains("(zombie)"))
    {
        return Err("Desktop process is a zombie".to_string());
    }

    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|error| format!("Cannot read desktop process identity: {error}"))?;
    let (parent_pid, start_time_ticks) = parse_proc_stat(&stat)?;
    let proc_exe = PathBuf::from(format!("/proc/{pid}/exe"));
    let metadata = std::fs::metadata(&proc_exe)
        .map_err(|error| format!("Cannot inspect desktop process executable: {error}"))?;
    let executable_path = std::fs::read_link(&proc_exe)
        .map_err(|error| format!("Cannot resolve desktop process executable: {error}"))?;

    Ok(ProcessIdentity {
        pid,
        uid,
        parent_pid,
        start_time_ticks,
        executable_device: metadata.dev(),
        executable_inode: metadata.ino(),
        executable_path,
    })
}

fn parse_status_uid(status: &str) -> Option<u32> {
    status
        .lines()
        .find(|line| line.starts_with("Uid:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn parse_proc_stat(stat: &str) -> Result<(i32, u64), String> {
    let close = stat
        .rfind(')')
        .ok_or_else(|| "Malformed /proc stat command field".to_string())?;
    let fields = stat
        .get(close + 1..)
        .ok_or_else(|| "Malformed /proc stat fields".to_string())?
        .split_whitespace()
        .collect::<Vec<_>>();
    // The tail begins at field 3 (state). ppid is field 4 and starttime is field 22.
    let parent_pid = fields
        .get(1)
        .ok_or_else(|| "Missing /proc stat parent pid".to_string())?
        .parse::<i32>()
        .map_err(|error| format!("Invalid /proc stat parent pid: {error}"))?;
    let start_time_ticks = fields
        .get(19)
        .ok_or_else(|| "Missing /proc stat start time".to_string())?
        .parse::<u64>()
        .map_err(|error| format!("Invalid /proc stat start time: {error}"))?;
    Ok((parent_pid, start_time_ticks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_parser_handles_spaces_and_parentheses_in_command_name() {
        let mut fields = vec!["S".to_string(), "42".to_string()];
        fields.extend((5..=21).map(|field| field.to_string()));
        fields.push("987654".to_string());
        let stat = format!("123 (desktop (main)) {}", fields.join(" "));

        assert_eq!(parse_proc_stat(&stat).unwrap(), (42, 987654));
    }

    #[test]
    fn status_uid_parser_uses_real_uid() {
        assert_eq!(
            parse_status_uid("Name:\tapp\nUid:\t1000\t1000\t1000\t1000\n"),
            Some(1000)
        );
    }
}
