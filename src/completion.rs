use std::io;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use rusqlite::OpenFlags;

use crate::db;
use crate::CompleteKind;
use crate::CompletionShell;

const INDEX_FILE: &str = ".tg_index.db";

const FISH_COMPLETIONS: &str = r#"# tg fish completions
function __tg_complete_sessions
    tg __complete sessions 2>/dev/null
end

function __tg_no_subcommand
    not __fish_seen_subcommand_from keys decrypt sessions messages search query schema export image doctor refresh skill completions help
end

complete -c tg -f
complete -c tg -n "__tg_no_subcommand" -a keys -d "Extract local DB keys"
complete -c tg -n "__tg_no_subcommand" -a decrypt -d "Decrypt local databases"
complete -c tg -n "__tg_no_subcommand" -a sessions -d "List chat sessions"
complete -c tg -n "__tg_no_subcommand" -a messages -d "Read a session"
complete -c tg -n "__tg_no_subcommand" -a search -d "Search messages"
complete -c tg -n "__tg_no_subcommand" -a query -d "Run structured queries"
complete -c tg -n "__tg_no_subcommand" -a schema -d "Show query fields"
complete -c tg -n "__tg_no_subcommand" -a export -d "Export messages"
complete -c tg -n "__tg_no_subcommand" -a image -d "Export images"
complete -c tg -n "__tg_no_subcommand" -a doctor -d "Diagnose setup"
complete -c tg -n "__tg_no_subcommand" -a refresh -d "Refresh decrypted cache"
complete -c tg -n "__tg_no_subcommand" -a skill -d "Manage agent skill"
complete -c tg -n "__tg_no_subcommand" -a completions -d "Generate shell completions"
complete -c tg -n "__tg_no_subcommand" -a "(__tg_complete_sessions)"

complete -c tg -l decrypted-dir -r -d "Path to decrypted databases"
complete -c tg -l jobs -r -d "Parallel job count"
complete -c tg -l limit -r -d "Result limit"
complete -c tg -l since -r -d "Lower time bound"
complete -c tg -l all-time -d "Search full history"

complete -c tg -n "__fish_seen_subcommand_from messages export image doctor sessions" -a "(__tg_complete_sessions)"
complete -c tg -n "__fish_seen_subcommand_from query" -l session -r -a "(__tg_complete_sessions)" -d "Limit query to one session"
complete -c tg -n "__fish_seen_subcommand_from completions" -a "fish zsh bash"
complete -c tg -n "__fish_seen_subcommand_from skill" -a install -d "Install local agent skill"
"#;

const ZSH_COMPLETIONS: &str = r#"#compdef tg

_tg_sessions() {
  local -a candidates
  local value description
  while IFS=$'\t' read -r value description; do
    [[ -n "$value" ]] && candidates+=("${value}:${description}")
  done < <(tg __complete sessions 2>/dev/null)
  _describe 'sessions' candidates
}

_tg() {
  local curcontext="$curcontext" state
  typeset -A opt_args
  local -a commands
  commands=(
    'keys:extract local DB keys'
    'decrypt:decrypt local databases'
    'sessions:list chat sessions'
    'messages:read a session'
    'search:search messages'
    'query:run structured queries'
    'schema:show query fields'
    'export:export messages'
    'image:export images'
    'doctor:diagnose setup'
    'refresh:refresh decrypted cache'
    'skill:manage agent skill'
    'completions:generate shell completions'
  )

  _arguments -C \
    '(-h --help)'{-h,--help}'[Print help]' \
    '--decrypted-dir[Path to decrypted databases]:directory:_files -/' \
    '--jobs[Parallel job count]:jobs:' \
    '--limit[Result limit]:limit:' \
    '--since[Lower time bound]:time:' \
    '--all-time[Search full history]' \
    '--session[Limit query to one session]:session:_tg_sessions' \
    '1:command:->command' \
    '*::arg:->arg'

  case "$state" in
    command)
      _describe -t commands 'tg command' commands
      _tg_sessions
      ;;
    arg)
      case "${words[1]}" in
        messages|export|image|doctor|sessions)
          _tg_sessions
          ;;
        completions)
          _values 'shell' fish zsh bash
          ;;
        skill)
          _values 'skill command' install
          ;;
      esac
      ;;
  esac
}

_tg "$@"
"#;

const BASH_COMPLETIONS: &str = r#"# tg bash completions
__tg_complete_words() {
  local kind="$1"
  tg __complete "$kind" 2>/dev/null | while IFS=$'\t' read -r value _description; do
    [[ -n "$value" ]] && printf '%s\n' "$value"
  done
}

_tg() {
  local cur prev subcommand word
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"

  case "$prev" in
    --session)
      mapfile -t COMPREPLY < <(compgen -W "$(__tg_complete_words sessions)" -- "$cur")
      return 0
      ;;
    completions)
      mapfile -t COMPREPLY < <(compgen -W "fish zsh bash" -- "$cur")
      return 0
      ;;
  esac

  for word in "${COMP_WORDS[@]:1:$COMP_CWORD-1}"; do
    case "$word" in
      -*)
        ;;
      *)
        subcommand="$word"
        break
        ;;
    esac
  done

  if [[ -z "$subcommand" ]]; then
    if [[ "$cur" == -* ]]; then
      mapfile -t COMPREPLY < <(compgen -W "--decrypted-dir --jobs --limit --since --all-time --help -h" -- "$cur")
    else
      mapfile -t COMPREPLY < <(compgen -W "keys decrypt sessions messages search query schema export image doctor refresh skill completions $(__tg_complete_words sessions)" -- "$cur")
    fi
    return 0
  fi

  case "$subcommand" in
    messages|export|image|doctor|sessions)
      mapfile -t COMPREPLY < <(compgen -W "$(__tg_complete_words sessions)" -- "$cur")
      ;;
    completions)
      mapfile -t COMPREPLY < <(compgen -W "fish zsh bash" -- "$cur")
      ;;
    skill)
      mapfile -t COMPREPLY < <(compgen -W "install" -- "$cur")
      ;;
  esac
}

complete -F _tg tg
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    value: String,
    description: String,
}

pub(crate) fn print_script(shell: CompletionShell) -> Result<(), String> {
    let script = match shell {
        CompletionShell::Fish => FISH_COMPLETIONS,
        CompletionShell::Zsh => ZSH_COMPLETIONS,
        CompletionShell::Bash => BASH_COMPLETIONS,
    };
    io::stdout()
        .write_all(script.as_bytes())
        .map_err(|e| format!("Write completion script: {}", e))
}

pub(crate) fn print_candidates(
    kind: CompleteKind,
    decrypted_dir: &Path,
    limit: usize,
) -> Result<(), String> {
    let candidates = match kind {
        CompleteKind::Sessions => session_candidates(decrypted_dir, limit)?,
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for candidate in candidates {
        writeln!(
            out,
            "{}\t{}",
            sanitize(&candidate.value),
            sanitize(&candidate.description)
        )
        .map_err(|e| format!("Write completion candidate: {}", e))?;
    }
    Ok(())
}

fn session_candidates(decrypted_dir: &Path, limit: usize) -> Result<Vec<Candidate>, String> {
    let limit = limit.max(1);
    let indexed = session_candidates_from_index(decrypted_dir, limit)?;
    if !indexed.is_empty() {
        return Ok(indexed);
    }
    session_candidates_from_contacts(decrypted_dir, limit)
}

fn session_candidates_from_index(
    decrypted_dir: &Path,
    limit: usize,
) -> Result<Vec<Candidate>, String> {
    let index_path = decrypted_dir.join(INDEX_FILE);
    if !index_path.exists() {
        return Ok(Vec::new());
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = rusqlite::Connection::open_with_flags(&index_path, flags)
        .map_err(|e| format!("Open message index {}: {}", index_path.display(), e))?;
    let _ = conn.busy_timeout(Duration::from_millis(50));
    let mut stmt = conn
        .prepare(
            "SELECT session_id, session_display, COUNT(*) AS count, MAX(create_time) AS latest
             FROM messages
             GROUP BY session_id
             ORDER BY latest DESC
             LIMIT ?1",
        )
        .map_err(|e| format!("Prepare session completion query: {}", e))?;

    let rows = stmt
        .query_map([limit as i64], |row| {
            let value = row.get::<_, String>(0)?;
            let display = row.get::<_, Option<String>>(1)?.unwrap_or_default();
            let count = row.get::<_, i64>(2).unwrap_or_default();
            let description = if display.trim().is_empty() || display == value {
                format!("{count} messages")
            } else {
                format!("{} - {count} messages", display.trim())
            };
            Ok(Candidate { value, description })
        })
        .map_err(|e| format!("Read session completion rows: {}", e))?;

    Ok(rows.filter_map(|row| row.ok()).collect())
}

fn session_candidates_from_contacts(
    decrypted_dir: &Path,
    limit: usize,
) -> Result<Vec<Candidate>, String> {
    let (contact_db, _) = db::find_decrypted_dbs(decrypted_dir);
    let Some(contact_db) = contact_db else {
        return Ok(Vec::new());
    };
    let contacts = db::load_contacts(&contact_db)?;
    let mut candidates = contacts
        .into_values()
        .filter(|contact| !contact.username.trim().is_empty())
        .map(|contact| {
            let description =
                if contact.display.trim().is_empty() || contact.display == contact.username {
                    "contact".to_string()
                } else {
                    contact.display
                };
            Candidate {
                value: contact.username,
                description,
            }
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.description
            .cmp(&right.description)
            .then_with(|| left.value.cmp(&right.value))
    });
    candidates.truncate(limit);
    Ok(candidates)
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '\t' | '\n' | '\r' => ' ',
            _ => ch,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_removes_completion_separators() {
        assert_eq!(sanitize("a\tb\nc"), "a b c");
    }
}
