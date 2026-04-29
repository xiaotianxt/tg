use std::path::{Path, PathBuf};

use crate::{db, output::Output, scanner};

pub(crate) struct DoctorOptions<'a> {
    pub session: Option<&'a str>,
    pub decrypted_dir: &'a Path,
    pub jobs: usize,
}

pub(crate) fn run(options: DoctorOptions<'_>) -> Result<(), String> {
    let stdout = std::io::stdout();
    let mut out = Output::new(stdout.lock());

    out.line(format_args!("tg doctor"))?;
    out.blank_line()?;

    write_process_status(&mut out)?;
    write_file_status(&mut out, "scanner", &scanner::default_scanner_path())?;
    write_keys_status(&mut out)?;
    write_cache_status(&mut out, options.decrypted_dir)?;

    if let Some(session) = options.session {
        out.blank_line()?;
        write_session_status(&mut out, options.decrypted_dir, session, options.jobs)?;
    }

    out.flush()
}

fn write_process_status<W: std::io::Write>(out: &mut Output<W>) -> Result<(), String> {
    match scanner::telegram_pid() {
        Ok(pid) => out.line(format_args!("Telegram process: OK (pid {})", pid)),
        Err(e) => out.line(format_args!("Telegram process: MISSING ({})", e)),
    }
}

fn write_file_status<W: std::io::Write>(
    out: &mut Output<W>,
    label: &str,
    path: &Path,
) -> Result<(), String> {
    if path.exists() {
        out.line(format_args!("{}: OK ({})", label, path.display()))
    } else {
        out.line(format_args!("{}: MISSING ({})", label, path.display()))
    }
}

fn write_keys_status<W: std::io::Write>(out: &mut Output<W>) -> Result<(), String> {
    let path = PathBuf::from("all_keys.json");
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let key_count = content.matches("\"enc_key\"").count();
            out.line(format_args!(
                "keys: OK ({} keys in {})",
                key_count,
                path.display()
            ))
        }
        Err(_) => out.line(format_args!("keys: MISSING ({})", path.display())),
    }
}

fn write_cache_status<W: std::io::Write>(
    out: &mut Output<W>,
    decrypted_dir: &Path,
) -> Result<(), String> {
    if decrypted_dir.is_dir() {
        out.line(format_args!(
            "decrypted cache: OK ({})",
            decrypted_dir.display()
        ))?;
    } else {
        out.line(format_args!(
            "decrypted cache: MISSING ({})",
            decrypted_dir.display()
        ))?;
    }

    let (contact_db, message_dbs) = db::find_decrypted_dbs(decrypted_dir);
    match contact_db {
        Some(path) => out.line(format_args!("contact db: OK ({})", path.display()))?,
        None => out.line(format_args!("contact db: MISSING"))?,
    }
    out.line(format_args!("message dbs: {}", message_dbs.len()))
}

fn write_session_status<W: std::io::Write>(
    out: &mut Output<W>,
    decrypted_dir: &Path,
    session: &str,
    jobs: usize,
) -> Result<(), String> {
    out.line(format_args!("session query: {}", session))?;
    match db::probe_session(decrypted_dir, session, jobs) {
        Ok(probe) => {
            out.line(format_args!(
                "resolved session: {} ({})",
                probe.display_name, probe.username
            ))?;
            out.line(format_args!("message table: {}", probe.table_name))?;
            out.line(format_args!("databases with table: {}", probe.matching_dbs))?;
            out.line(format_args!("messages: {}", probe.message_count))
        }
        Err(e) => out.line(format_args!("session probe: ERROR ({})", e)),
    }
}
