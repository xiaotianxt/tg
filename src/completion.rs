use std::collections::HashSet;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OpenFlags, OptionalExtension};

use crate::contact;
use crate::db;
use crate::paths;
use crate::CompleteKind;
use crate::CompletionShell;

const CONTACT_COMPLETION_CACHE_FILE: &str = ".tg_contact_completion.db";
const CONTACT_COMPLETION_SCHEMA_VERSION: i64 = 3;
const CONTACT_COMPLETION_BUSY_TIMEOUT: Duration = Duration::from_millis(50);
const CONTACT_COMPLETION_POOL_FLOOR: usize = 200;
const CONTACT_COMPLETION_POOL_FACTOR: usize = 8;

const FISH_COMPLETIONS: &str = r#"# tg fish completions
function __tg_complete_sessions
    set -l token (commandline -ct)
    tg __complete sessions -- "$token" 2>/dev/null
end

function __tg_no_subcommand
    not __fish_seen_subcommand_from keys decrypt sessions messages search query schema export image voice doctor refresh skill completions help
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
complete -c tg -n "__tg_no_subcommand" -a voice -d "Export voices"
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
complete -c tg -n "__tg_no_subcommand; or __fish_seen_subcommand_from messages search query export" -l anonymous -d "Use public names"

complete -c tg -n "__fish_seen_subcommand_from messages export image voice doctor sessions" -a "(__tg_complete_sessions)"
complete -c tg -n "__fish_seen_subcommand_from query" -l session -r -a "(__tg_complete_sessions)" -d "Limit query to one session"
complete -c tg -n "__fish_seen_subcommand_from completions" -a "fish zsh bash"
complete -c tg -n "__fish_seen_subcommand_from skill" -a install -d "Install local agent skill"
"#;

const ZSH_COMPLETIONS: &str = r#"#compdef tg

_tg_sessions() {
  local -a candidates
  local value description
  local token="${PREFIX:-}"
  while IFS=$'\t' read -r value description; do
    [[ -n "$value" ]] && candidates+=("${value}:${description}")
  done < <(tg __complete sessions -- "$token" 2>/dev/null)
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
    'voice:export voices'
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
    '--anonymous[Use group/member public names]' \
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
        messages|export|image|voice|doctor|sessions)
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
  local token="${2-}"
  tg __complete "$kind" -- "$token" 2>/dev/null | while IFS=$'\t' read -r value _description; do
    [[ -n "$value" ]] && printf '%s\n' "$value"
  done
}

__tg_is_command() {
  case "$1" in
    keys|decrypt|sessions|messages|search|query|schema|export|image|voice|doctor|refresh|skill|completions|help)
      return 0
      ;;
  esac
  return 1
}

__tg_option_takes_value() {
  case "$1" in
    --all-time|--full|--verbose|--keys|--tail|--head|--anonymous|--list|--all|--help|-h)
      return 1
      ;;
    -*)
      return 0
      ;;
  esac
  return 1
}

__tg_options_for_command() {
  case "$1" in
    messages)
      printf '%s\n' "--decrypted-dir --limit --offset --search --since --all-time --tail --head --time-bucket --anonymous --jobs --help -h"
      ;;
    sessions)
      printf '%s\n' "--decrypted-dir --top --jobs --help -h"
      ;;
    search)
      printf '%s\n' "--decrypted-dir --limit --since --all-time --anonymous --jobs --help -h"
      ;;
    query)
      printf '%s\n' "--session --decrypted-dir --contains --not --since --all-time --until --limit --offset --order --match-mode --fields --format --max-cell-chars --anonymous --jobs --help -h"
      ;;
    schema)
      printf '%s\n' "--db --decrypted-dir --format --max-cell-chars --jobs --help -h"
      ;;
    export)
      printf '%s\n' "--decrypted-dir --format --output --media-dir --since --limit --all-time --anonymous --jobs --help -h"
      ;;
    image)
      printf '%s\n' "--decrypted-dir --output --list --all --index --id --limit --since --jobs --help -h"
      ;;
    voice)
      printf '%s\n' "--decrypted-dir --output --format --decoder --list --all --index --id --limit --since --sample-rate --jobs --help -h"
      ;;
    doctor)
      printf '%s\n' "--decrypted-dir --jobs --help -h"
      ;;
    refresh)
      printf '%s\n' "--decrypted-dir --keys --jobs --help -h"
      ;;
    decrypt)
      printf '%s\n' "--keys --output --db-dir --full --since --verbose --jobs --help -h"
      ;;
    keys)
      printf '%s\n' "--timeout --help -h"
      ;;
    *)
      printf '%s\n' "--help -h"
      ;;
  esac
}

__tg_effective_subcommand() {
  local word skip_next=0
  for word in "${COMP_WORDS[@]:1:$COMP_CWORD-1}"; do
    if (( skip_next )); then
      skip_next=0
      continue
    fi
    case "$word" in
      --)
        continue
        ;;
      --*=*)
        continue
        ;;
      -*)
        if __tg_option_takes_value "$word"; then
          skip_next=1
        fi
        continue
        ;;
      *)
        if __tg_is_command "$word"; then
          printf '%s\n' "$word"
        else
          printf '%s\n' "messages"
        fi
        return 0
        ;;
    esac
  done
}

_tg() {
  local cur prev subcommand
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"
  subcommand="$(__tg_effective_subcommand)"

  case "$prev" in
    --session)
      COMPREPLY=( $(compgen -W "$(__tg_complete_words sessions "$cur")" -- "$cur") )
      return 0
      ;;
    --time-bucket)
      COMPREPLY=( $(compgen -W "1m 1min 1h 1d 1mo 1y full none" -- "$cur") )
      return 0
      ;;
    --format)
      if [[ "$subcommand" == "voice" ]]; then
        COMPREPLY=( $(compgen -W "native wav pcm" -- "$cur") )
      else
        COMPREPLY=( $(compgen -W "table json txt csv" -- "$cur") )
      fi
      return 0
      ;;
    --order)
      COMPREPLY=( $(compgen -W "newest oldest" -- "$cur") )
      return 0
      ;;
    --match-mode)
      COMPREPLY=( $(compgen -W "all any" -- "$cur") )
      return 0
      ;;
    completions)
      COMPREPLY=( $(compgen -W "fish zsh bash" -- "$cur") )
      return 0
      ;;
  esac

  if [[ -z "$subcommand" ]]; then
    if [[ "$cur" == -* ]]; then
      COMPREPLY=( $(compgen -W "--decrypted-dir --jobs --limit --since --all-time --help -h" -- "$cur") )
    else
      COMPREPLY=( $(compgen -W "keys decrypt sessions messages search query schema export image voice doctor refresh skill completions $(__tg_complete_words sessions "$cur")" -- "$cur") )
    fi
    return 0
  fi

  case "$subcommand" in
    messages)
      if [[ "$cur" == -* ]]; then
        COMPREPLY=( $(compgen -W "$(__tg_options_for_command messages)" -- "$cur") )
      else
        COMPREPLY=( $(compgen -W "$(__tg_complete_words sessions "$cur")" -- "$cur") )
      fi
      ;;
    export|image|voice|doctor|sessions)
      if [[ "$cur" == -* ]]; then
        COMPREPLY=( $(compgen -W "$(__tg_options_for_command "$subcommand")" -- "$cur") )
      else
        COMPREPLY=( $(compgen -W "$(__tg_complete_words sessions "$cur")" -- "$cur") )
      fi
      ;;
    search|query|schema|refresh|decrypt|keys)
      if [[ "$cur" == -* ]]; then
        COMPREPLY=( $(compgen -W "$(__tg_options_for_command "$subcommand")" -- "$cur") )
      fi
      ;;
    completions)
      COMPREPLY=( $(compgen -W "fish zsh bash" -- "$cur") )
      ;;
    skill)
      COMPREPLY=( $(compgen -W "install" -- "$cur") )
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
    query: Option<&str>,
) -> Result<(), String> {
    let candidates = match kind {
        CompleteKind::Sessions => session_candidates(decrypted_dir, limit, query)?,
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

fn session_candidates(
    decrypted_dir: &Path,
    limit: usize,
    query: Option<&str>,
) -> Result<Vec<Candidate>, String> {
    let limit = limit.max(1);
    let query = query.unwrap_or_default();
    session_candidates_from_contacts(decrypted_dir, limit, query)
}

#[derive(Clone)]
struct ContactCompletionRow {
    id: i64,
    username: String,
    display: String,
    remark: String,
    nick_name: String,
    alias: String,
    sort_key: String,
    is_stranger: bool,
}

impl ContactCompletionRow {
    fn from_contact(id: i64, contact: contact::Contact) -> Self {
        let display = contact.personal_display_name().to_string();
        let sort_key = make_contact_sort_key(&display, &contact.username);
        Self {
            id,
            username: contact.username,
            display,
            remark: contact.remark,
            nick_name: contact.nick_name,
            alias: contact.alias,
            sort_key,
            is_stranger: contact.is_stranger,
        }
    }

    fn fields(&self) -> [(usize, &str); 5] {
        [
            (0, &self.display),
            (1, &self.remark),
            (2, &self.nick_name),
            (3, &self.alias),
            (4, &self.username),
        ]
    }

    fn candidate(self) -> Candidate {
        let name = if !self.remark.trim().is_empty()
            && !self.nick_name.trim().is_empty()
            && self.remark != self.nick_name
        {
            format!("{} ({})", self.remark, self.nick_name)
        } else if self.display.trim().is_empty() || self.display == self.username {
            "contact".to_string()
        } else {
            self.display
        };
        let description = if self.is_stranger {
            format!("*{name}")
        } else {
            name
        };
        Candidate {
            value: self.username,
            description,
        }
    }
}

fn session_candidates_from_contacts(
    decrypted_dir: &Path,
    limit: usize,
    query: &str,
) -> Result<Vec<Candidate>, String> {
    let Some(contact_db) = db::find_decrypted_contact_db(decrypted_dir) else {
        return Ok(Vec::new());
    };

    session_candidates_from_contact_cache(decrypted_dir, &contact_db, limit, query)
        .or_else(|_| session_candidates_from_contact_db(&contact_db, limit, query))
}

fn session_candidates_from_contact_db(
    contact_db: &Path,
    limit: usize,
    query: &str,
) -> Result<Vec<Candidate>, String> {
    let contacts = contact::load_contacts(contact_db)?;
    let rows = contacts
        .into_values()
        .filter(|contact| !contact.username.trim().is_empty())
        .enumerate()
        .map(|(index, contact)| ContactCompletionRow::from_contact(index as i64 + 1, contact))
        .collect::<Vec<_>>();
    Ok(score_contact_rows(rows, limit, query))
}

fn session_candidates_from_contact_cache(
    decrypted_dir: &Path,
    contact_db: &Path,
    limit: usize,
    query: &str,
) -> Result<Vec<Candidate>, String> {
    let source_signature = contact_db_signature(contact_db)
        .ok_or_else(|| format!("Cannot stat contact DB {}", contact_db.display()))?;
    let cache_path = contact_completion_cache_path(decrypted_dir);
    if let Some(candidates) =
        query_existing_contact_cache(&cache_path, &source_signature, limit, query)
    {
        return Ok(candidates);
    }

    paths::ensure_private_dir(decrypted_dir).map_err(|e| {
        format!(
            "Cannot secure contact completion cache dir {}: {}",
            decrypted_dir.display(),
            e
        )
    })?;
    let mut conn = Connection::open(&cache_path).map_err(|e| {
        format!(
            "Open contact completion cache {}: {}",
            cache_path.display(),
            e
        )
    })?;
    let _ = conn.busy_timeout(CONTACT_COMPLETION_BUSY_TIMEOUT);
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| format!("Set contact completion cache journal mode: {}", e))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| format!("Set contact completion cache synchronous mode: {}", e))?;
    ensure_contact_cache_schema(&conn)?;

    if contact_cache_meta_string(&conn, "source_signature")?.as_deref() != Some(&source_signature) {
        rebuild_contact_cache(&mut conn, contact_db, &source_signature)?;
    }

    query_contact_cache(&conn, limit, query)
}

fn contact_completion_cache_path(decrypted_dir: &Path) -> PathBuf {
    decrypted_dir.join(CONTACT_COMPLETION_CACHE_FILE)
}

fn query_existing_contact_cache(
    cache_path: &Path,
    source_signature: &str,
    limit: usize,
    query: &str,
) -> Option<Vec<Candidate>> {
    if !cache_path.exists() {
        return None;
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(cache_path, flags).ok()?;
    let _ = conn.busy_timeout(CONTACT_COMPLETION_BUSY_TIMEOUT);
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .ok()?;
    if version != CONTACT_COMPLETION_SCHEMA_VERSION {
        return None;
    }
    if contact_cache_meta_string(&conn, "source_signature")
        .ok()
        .flatten()
        .as_deref()
        != Some(source_signature)
    {
        return None;
    }

    query_contact_cache(&conn, limit, query).ok()
}

fn ensure_contact_cache_schema(conn: &Connection) -> Result<(), String> {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|e| format!("Read contact completion cache schema version: {}", e))?;
    if version != 0 && version != CONTACT_COMPLETION_SCHEMA_VERSION {
        conn.execute_batch(
            "DROP TABLE IF EXISTS contact_completion_meta;
             DROP TABLE IF EXISTS contact_completion_grams;
             DROP TABLE IF EXISTS contact_completion_fields;
             DROP TABLE IF EXISTS contact_completion_contacts;",
        )
        .map_err(|e| format!("Reset contact completion cache schema: {}", e))?;
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS contact_completion_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS contact_completion_contacts (
            id INTEGER PRIMARY KEY,
            username TEXT NOT NULL UNIQUE,
            display TEXT NOT NULL,
            remark TEXT NOT NULL,
            nick_name TEXT NOT NULL,
            alias TEXT NOT NULL,
            sort_key TEXT NOT NULL,
            is_stranger INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS contact_completion_fields (
            contact_id INTEGER NOT NULL,
            field_priority INTEGER NOT NULL,
            value TEXT NOT NULL,
            normalized_value TEXT NOT NULL,
            field_len INTEGER NOT NULL,
            PRIMARY KEY (contact_id, field_priority, value)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS contact_completion_grams (
            gram TEXT NOT NULL,
            contact_id INTEGER NOT NULL,
            field_priority INTEGER NOT NULL,
            PRIMARY KEY (gram, contact_id, field_priority)
        ) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS idx_contact_completion_contacts_sort
            ON contact_completion_contacts(sort_key, username);
        CREATE INDEX IF NOT EXISTS idx_contact_completion_fields_value
            ON contact_completion_fields(normalized_value, contact_id);
        CREATE INDEX IF NOT EXISTS idx_contact_completion_grams_contact
            ON contact_completion_grams(contact_id);",
    )
    .map_err(|e| format!("Create contact completion cache schema: {}", e))?;
    conn.pragma_update(None, "user_version", CONTACT_COMPLETION_SCHEMA_VERSION)
        .map_err(|e| format!("Set contact completion cache schema version: {}", e))?;
    Ok(())
}

fn rebuild_contact_cache(
    conn: &mut Connection,
    contact_db: &Path,
    source_signature: &str,
) -> Result<(), String> {
    let contacts = contact::load_contacts(contact_db)?;
    let mut rows = contacts
        .into_values()
        .filter(|contact| !contact.username.trim().is_empty())
        .enumerate()
        .map(|(index, contact)| ContactCompletionRow::from_contact(index as i64 + 1, contact))
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        left.sort_key
            .cmp(&right.sort_key)
            .then_with(|| left.username.cmp(&right.username))
    });
    for (index, row) in rows.iter_mut().enumerate() {
        row.id = index as i64 + 1;
    }

    let tx = conn
        .transaction()
        .map_err(|e| format!("Start contact completion cache rebuild: {}", e))?;
    tx.execute_batch(
        "DELETE FROM contact_completion_grams;
         DELETE FROM contact_completion_fields;
         DELETE FROM contact_completion_contacts;
         DELETE FROM contact_completion_meta;",
    )
    .map_err(|e| format!("Clear contact completion cache: {}", e))?;

    {
        let mut contact_stmt = tx
            .prepare(
                "INSERT INTO contact_completion_contacts (
                    id, username, display, remark, nick_name, alias, sort_key, is_stranger
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )
            .map_err(|e| format!("Prepare contact completion cache insert: {}", e))?;
        let mut gram_stmt = tx
            .prepare(
                "INSERT OR IGNORE INTO contact_completion_grams (
                    gram, contact_id, field_priority
                 ) VALUES (?1, ?2, ?3)",
            )
            .map_err(|e| format!("Prepare contact completion gram insert: {}", e))?;
        let mut field_stmt = tx
            .prepare(
                "INSERT OR IGNORE INTO contact_completion_fields (
                    contact_id, field_priority, value, normalized_value, field_len
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|e| format!("Prepare contact completion field insert: {}", e))?;

        for row in &rows {
            contact_stmt
                .execute(params![
                    row.id,
                    &row.username,
                    &row.display,
                    &row.remark,
                    &row.nick_name,
                    &row.alias,
                    &row.sort_key,
                    row.is_stranger as i64
                ])
                .map_err(|e| format!("Insert contact completion row: {}", e))?;

            for (priority, field) in row.fields() {
                let normalized = normalize_completion_text(field);
                if normalized.is_empty() {
                    continue;
                }
                field_stmt
                    .execute(params![
                        row.id,
                        priority as i64,
                        field.trim(),
                        normalized,
                        field.chars().count() as i64
                    ])
                    .map_err(|e| format!("Insert contact completion field: {}", e))?;
            }

            for (gram, priority) in indexed_contact_grams(row) {
                gram_stmt
                    .execute(params![gram, row.id, priority as i64])
                    .map_err(|e| format!("Insert contact completion gram: {}", e))?;
            }
        }
    }

    set_contact_cache_meta_string(&tx, "source_signature", source_signature)?;
    tx.commit()
        .map_err(|e| format!("Commit contact completion cache rebuild: {}", e))?;
    Ok(())
}

fn query_contact_cache(
    conn: &Connection,
    limit: usize,
    query: &str,
) -> Result<Vec<Candidate>, String> {
    let normalized_query = normalize_completion_text(query);
    if normalized_query.is_empty() {
        return contact_cache_sorted_candidates(conn, limit);
    }

    let direct_rows = query_direct_contact_cache(conn, &normalized_query)?;
    let direct_candidates = score_contact_rows(direct_rows, limit, query);
    if !direct_candidates.is_empty() {
        return Ok(direct_candidates);
    }

    let grams = completion_grams(&normalized_query);
    if grams.is_empty() {
        return query_contact_cache_full_scan(conn, limit, query);
    }

    let placeholders = (1..=grams.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let limit_index = grams.len() + 1;
    let pool_limit = limit
        .saturating_mul(CONTACT_COMPLETION_POOL_FACTOR)
        .max(CONTACT_COMPLETION_POOL_FLOOR);
    let sql = format!(
        "SELECT c.id, c.username, c.display, c.remark, c.nick_name, c.alias, c.sort_key, c.is_stranger
         FROM contact_completion_grams g
         JOIN contact_completion_contacts c ON c.id = g.contact_id
         WHERE g.gram IN ({placeholders})
         GROUP BY c.id
         ORDER BY COUNT(*) DESC, MIN(g.field_priority) ASC, LENGTH(c.sort_key) ASC,
                  c.sort_key ASC, c.username ASC
         LIMIT ?{limit_index}"
    );
    let mut params = grams.into_iter().map(Value::Text).collect::<Vec<_>>();
    params.push(Value::Integer(pool_limit as i64));

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("Prepare contact completion cache query: {}", e))?;
    let rows = stmt
        .query_map(params_from_iter(params.iter()), contact_row_from_sql)
        .map_err(|e| format!("Read contact completion cache rows: {}", e))?
        .filter_map(|row| row.ok())
        .collect::<Vec<_>>();
    let candidates = score_contact_rows(rows, limit, query);
    if candidates.is_empty() {
        query_contact_cache_full_scan(conn, limit, query)
    } else {
        Ok(candidates)
    }
}

fn query_direct_contact_cache(
    conn: &Connection,
    normalized_query: &str,
) -> Result<Vec<ContactCompletionRow>, String> {
    let contains_pattern = like_contains_pattern(normalized_query);
    let prefix_pattern = like_prefix_pattern(normalized_query);
    let mut stmt = conn
        .prepare(
            "SELECT c.id, c.username, c.display, c.remark, c.nick_name, c.alias, c.sort_key, c.is_stranger
             FROM contact_completion_fields f
             JOIN contact_completion_contacts c ON c.id = f.contact_id
             WHERE f.normalized_value LIKE ?1 ESCAPE '\\'
             GROUP BY c.id
             ORDER BY
                 MIN(CASE
                     WHEN f.normalized_value = ?2 THEN 0
                     WHEN f.normalized_value LIKE ?3 ESCAPE '\\' THEN 1
                     ELSE 2
                 END),
                 MIN(f.field_priority) ASC,
                 MIN(f.field_len) ASC,
                 c.sort_key ASC,
                 c.username ASC",
        )
        .map_err(|e| format!("Prepare direct contact completion query: {}", e))?;
    let rows = stmt
        .query_map(
            params![contains_pattern, normalized_query, prefix_pattern],
            contact_row_from_sql,
        )
        .map_err(|e| format!("Read direct contact completion rows: {}", e))?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

fn contact_cache_sorted_candidates(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<Candidate>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, username, display, remark, nick_name, alias, sort_key, is_stranger
             FROM contact_completion_contacts
             ORDER BY sort_key ASC, username ASC
             LIMIT ?1",
        )
        .map_err(|e| format!("Prepare sorted contact completion query: {}", e))?;
    let rows = stmt
        .query_map([limit as i64], contact_row_from_sql)
        .map_err(|e| format!("Read sorted contact completion rows: {}", e))?;
    Ok(rows
        .filter_map(|row| row.ok())
        .map(ContactCompletionRow::candidate)
        .collect())
}

fn query_contact_cache_full_scan(
    conn: &Connection,
    limit: usize,
    query: &str,
) -> Result<Vec<Candidate>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, username, display, remark, nick_name, alias, sort_key, is_stranger
             FROM contact_completion_contacts",
        )
        .map_err(|e| format!("Prepare contact completion full scan: {}", e))?;
    let rows = stmt
        .query_map([], contact_row_from_sql)
        .map_err(|e| format!("Read contact completion full scan rows: {}", e))?
        .filter_map(|row| row.ok())
        .collect::<Vec<_>>();
    Ok(score_contact_rows(rows, limit, query))
}

fn contact_row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContactCompletionRow> {
    Ok(ContactCompletionRow {
        id: row.get(0)?,
        username: row.get(1)?,
        display: row.get(2)?,
        remark: row.get(3)?,
        nick_name: row.get(4)?,
        alias: row.get(5)?,
        sort_key: row.get(6)?,
        is_stranger: row.get::<_, i64>(7)? != 0,
    })
}

fn score_contact_rows(
    mut rows: Vec<ContactCompletionRow>,
    limit: usize,
    query: &str,
) -> Vec<Candidate> {
    if normalize_completion_text(query).is_empty() {
        rows.sort_by(|left, right| {
            left.sort_key
                .cmp(&right.sort_key)
                .then_with(|| left.username.cmp(&right.username))
        });
        rows.truncate(limit);
        return rows
            .into_iter()
            .map(ContactCompletionRow::candidate)
            .collect();
    }

    let mut scored = rows
        .into_iter()
        .filter_map(|row| {
            let fields = row.fields();
            let score = db::best_contact_field_score(&fields, query)?;
            Some((row, score))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        left.1
            .field_priority
            .cmp(&right.1.field_priority)
            .then_with(|| right.1.score.cmp(&left.1.score))
            .then_with(|| left.1.distance.cmp(&right.1.distance))
            .then_with(|| left.1.field_len.cmp(&right.1.field_len))
            .then_with(|| left.0.sort_key.cmp(&right.0.sort_key))
            .then_with(|| left.0.username.cmp(&right.0.username))
    });
    scored.truncate(limit);
    scored.into_iter().map(|(row, _)| row.candidate()).collect()
}

fn make_contact_sort_key(display: &str, username: &str) -> String {
    let value = if display.trim().is_empty() {
        username
    } else {
        display
    };
    let normalized = normalize_completion_text(value);
    if normalized.is_empty() {
        username.to_string()
    } else {
        normalized
    }
}

fn indexed_contact_grams(row: &ContactCompletionRow) -> Vec<(String, usize)> {
    let mut grams = Vec::new();
    let mut seen = HashSet::new();
    let fields = row.fields();
    for (priority, field) in fields {
        for gram in completion_grams(&normalize_completion_text(field)) {
            if seen.insert((gram.clone(), priority)) {
                grams.push((gram, priority));
            }
        }
    }
    grams
}

fn completion_grams(value: &str) -> Vec<String> {
    let chars = value.chars().collect::<Vec<_>>();
    let mut grams = Vec::new();
    let mut seen = HashSet::new();
    for ch in &chars {
        let gram = ch.to_string();
        if seen.insert(gram.clone()) {
            grams.push(gram);
        }
    }
    for window in chars.windows(2) {
        let gram = window.iter().collect::<String>();
        if seen.insert(gram.clone()) {
            grams.push(gram);
        }
    }
    grams
}

fn normalize_completion_text(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn like_contains_pattern(value: &str) -> String {
    format!("%{}%", escape_like_pattern(value))
}

fn like_prefix_pattern(value: &str) -> String {
    format!("{}%", escape_like_pattern(value))
}

fn escape_like_pattern(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn contact_db_signature(path: &Path) -> Option<String> {
    let mut parts = vec![format!("main:{}", file_signature(path)?)];
    if let Some(wal_path) = sibling_path_with_suffix(path, "-wal") {
        if let Some(signature) = file_signature(&wal_path) {
            parts.push(format!("wal:{signature}"));
        }
    }
    Some(parts.join("|"))
}

fn file_signature(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    let mtime_ns = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!("{}:{}", mtime_ns, meta.len()))
}

fn sibling_path_with_suffix(path: &Path, suffix: &str) -> Option<PathBuf> {
    let mut file_name = path.file_name()?.to_os_string();
    file_name.push(suffix);
    Some(path.with_file_name(file_name))
}

fn contact_cache_meta_string(conn: &Connection, key: &str) -> Result<Option<String>, String> {
    conn.query_row(
        "SELECT value FROM contact_completion_meta WHERE key = ?1",
        [key],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|e| format!("Read contact completion cache meta: {}", e))
}

fn set_contact_cache_meta_string(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO contact_completion_meta (key, value) VALUES (?1, ?2)",
        params![key, value],
    )
    .map_err(|e| format!("Write contact completion cache meta: {}", e))?;
    Ok(())
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
    use tempfile::TempDir;

    #[test]
    fn sanitize_removes_completion_separators() {
        assert_eq!(sanitize("a\tb\nc"), "a b c");
    }

    #[test]
    fn shell_completion_scripts_include_anonymous_flag() {
        assert!(FISH_COMPLETIONS.contains("-l anonymous"));
        assert!(ZSH_COMPLETIONS.contains("--anonymous["));
        assert!(BASH_COMPLETIONS.contains("--time-bucket --anonymous"));
        assert!(!BASH_COMPLETIONS.contains("mapfile"));
    }

    #[test]
    fn session_completion_uses_contact_cache_for_query() {
        let dir = create_decrypted_contacts(&[
            ("tgid_alice", "Alice Zhang", "", "alice"),
            ("tgid_zhang", "张测试", "", "zt"),
        ]);

        let candidates = session_candidates(dir.path(), 10, Some("测试")).unwrap();

        assert_eq!(candidates[0].value, "tgid_zhang");
        assert!(dir.path().join(CONTACT_COMPLETION_CACHE_FILE).exists());
    }

    #[test]
    fn session_completion_matches_nick_name_when_remark_exists() {
        let dir = create_decrypted_contacts(&[
            ("tgid_other", "Alice Zhang", "Alice Work", "alice"),
            ("tgid_real_name", "真名测试", "某个备注", "real"),
        ]);

        let candidates = session_candidates(dir.path(), 10, Some("真名")).unwrap();

        assert_eq!(candidates[0].value, "tgid_real_name");
        assert_eq!(candidates[0].description, "某个备注 (真名测试)");
    }

    #[test]
    fn session_completion_marks_strangers_in_description() {
        let dir = create_decrypted_contacts_with_strangers(
            &[("tgid_friend", "Friend Name", "", "friend")],
            &[("tgid_stranger", "Stranger Name", "", "stranger")],
        );

        let candidates = session_candidates(dir.path(), 10, Some("Stranger")).unwrap();

        assert_eq!(candidates[0].value, "tgid_stranger");
        assert_eq!(candidates[0].description, "*Stranger Name");
    }

    #[test]
    fn session_completion_rebuilds_cache_after_contact_change() {
        let dir = create_decrypted_contacts(&[("tgid_old", "Old Contact", "", "old")]);
        let old = session_candidates(dir.path(), 10, Some("Old")).unwrap();
        assert_eq!(old[0].value, "tgid_old");

        std::thread::sleep(Duration::from_millis(2));
        let contact_db = dir.path().join("contact/contact.db");
        let conn = Connection::open(contact_db).unwrap();
        insert_contact(&conn, "tgid_new", "新增联系人", "", "new");

        let candidates = session_candidates(dir.path(), 10, Some("新增")).unwrap();
        assert_eq!(candidates[0].value, "tgid_new");
    }

    fn create_decrypted_contacts(rows: &[(&str, &str, &str, &str)]) -> TempDir {
        create_decrypted_contacts_with_strangers(rows, &[])
    }

    fn create_decrypted_contacts_with_strangers(
        rows: &[(&str, &str, &str, &str)],
        stranger_rows: &[(&str, &str, &str, &str)],
    ) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        let contact_dir = dir.path().join("contact");
        fs::create_dir_all(&contact_dir).unwrap();
        let conn = Connection::open(contact_dir.join("contact.db")).unwrap();
        create_contact_like_table(&conn, "contact");
        for (username, nick_name, remark, alias) in rows {
            insert_contact(&conn, username, nick_name, remark, alias);
        }
        if !stranger_rows.is_empty() {
            create_contact_like_table(&conn, "stranger");
            for (username, nick_name, remark, alias) in stranger_rows {
                insert_contact_like_table(&conn, "stranger", username, nick_name, remark, alias);
            }
        }
        drop(conn);
        dir
    }

    fn create_contact_like_table(conn: &Connection, table: &str) {
        let sql = format!(
            "CREATE TABLE {table} (
                username TEXT,
                nick_name TEXT,
                remark TEXT,
                alias TEXT
            )"
        );
        conn.execute(&sql, []).unwrap();
    }

    fn insert_contact(
        conn: &Connection,
        username: &str,
        nick_name: &str,
        remark: &str,
        alias: &str,
    ) {
        insert_contact_like_table(conn, "contact", username, nick_name, remark, alias);
    }

    fn insert_contact_like_table(
        conn: &Connection,
        table: &str,
        username: &str,
        nick_name: &str,
        remark: &str,
        alias: &str,
    ) {
        let sql = format!(
            "INSERT INTO {table} (username, nick_name, remark, alias)
             VALUES (?1, ?2, ?3, ?4)"
        );
        conn.execute(&sql, params![username, nick_name, remark, alias])
            .unwrap();
    }
}
