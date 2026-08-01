use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Arg;
use clap::Command;
use clap::CommandFactory;
use clap::ValueHint;

use crate::contact;
use crate::db;
use crate::Cli;
use crate::CompleteKind;
use crate::CompletionShell;

const FISH_COMPLETIONS: &str = r#"# tg fish completions
function __tg_complete
    set -l current (commandline -ct)
    set -l words (commandline -opc)
    set -l tg_cmd tg
    if test (count $words) -gt 0
        set tg_cmd $words[1]
    end
    command "$tg_cmd" __complete words --shell fish "--current=$current" -- $words 2>/dev/null
end

complete -c tg -f -a "(__tg_complete)"
"#;

const ZSH_COMPLETIONS: &str = r#"#compdef tg

_tg() {
  local -a candidates
  local tg_cmd="${words[1]:-tg}"
  local value description
  while IFS=$'\t' read -r value description; do
    [[ -n "$value" ]] && candidates+=("${value}:${description}")
  done < <("$tg_cmd" __complete words --shell zsh --cursor "$CURRENT" "--current=${words[CURRENT]}" -- "${words[@]}" 2>/dev/null)
  _describe -t values 'tg completions' candidates
}

_tg "$@"
"#;

const BASH_COMPLETIONS: &str = r#"# tg bash completions
_tg() {
  local cur value description tg_cmd
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  tg_cmd="${COMP_WORDS[0]:-tg}"
  while IFS=$'\t' read -r value description; do
    [[ -n "$value" ]] && COMPREPLY+=("$value")
  done < <("$tg_cmd" __complete words --shell bash --cursor "$COMP_CWORD" "--current=$cur" -- "${COMP_WORDS[@]}" 2>/dev/null)
}

complete -F _tg tg
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    value: String,
    description: String,
}

pub(crate) struct CompletionRequest<'a> {
    pub kind: CompleteKind,
    pub decrypted_dir: &'a Path,
    pub limit: usize,
    pub shell: Option<CompletionShell>,
    pub cursor: Option<usize>,
    pub current: Option<&'a str>,
    pub words: &'a [String],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DynamicKind {
    Sessions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    Files,
    Directories,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueCompletion {
    Dynamic(DynamicKind),
    Path(PathKind),
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

pub(crate) fn print_candidates(request: CompletionRequest<'_>) -> Result<(), String> {
    let candidates = match request.kind {
        CompleteKind::Sessions => session_candidates(
            request.decrypted_dir,
            request.limit,
            request.words.first().map(String::as_str),
        )?,
        CompleteKind::Words => word_candidates(&request)?,
    };
    print_candidate_lines(candidates)
}

fn print_candidate_lines(candidates: Vec<Candidate>) -> Result<(), String> {
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

fn word_candidates(request: &CompletionRequest<'_>) -> Result<Vec<Candidate>, String> {
    let current = request.current.unwrap_or_default();
    let shell = request.shell.unwrap_or(CompletionShell::Bash);
    let before = words_before_current(shell, request.cursor, current, request.words);
    let decrypted_dir = decrypted_dir_from(&before).unwrap_or_else(|| request.decrypted_dir.into());
    let user_words = strip_program_name(&before);

    if let Some(candidates) = complete_attached_value(current, &user_words, &decrypted_dir)? {
        return Ok(candidates);
    }
    if let Some(candidates) = complete_value_after_previous(current, &user_words, &decrypted_dir)? {
        return Ok(candidates);
    }

    let root = Cli::command();
    let context = command_context(&root, &user_words);
    if current.starts_with('-') {
        return Ok(filter_candidates(
            option_candidates(context.command),
            current,
        ));
    }
    if context.path.is_empty() {
        return root_candidates(&root, current, &decrypted_dir);
    }
    if context
        .command
        .get_subcommands()
        .any(|command| !command.is_hide_set())
    {
        return Ok(filter_candidates(
            command_candidates(context.command),
            current,
        ));
    }

    positional_candidates(&context, current, &decrypted_dir)
}

fn words_before_current(
    shell: CompletionShell,
    cursor: Option<usize>,
    current: &str,
    words: &[String],
) -> Vec<String> {
    match shell {
        CompletionShell::Bash => {
            let end = cursor.unwrap_or(words.len()).min(words.len());
            words[..end].to_vec()
        }
        CompletionShell::Zsh => {
            let end = cursor
                .and_then(|value| value.checked_sub(1))
                .unwrap_or(words.len())
                .min(words.len());
            words[..end].to_vec()
        }
        CompletionShell::Fish => {
            if !current.is_empty() && words.last().is_some_and(|word| word == current) {
                words[..words.len() - 1].to_vec()
            } else {
                words.to_vec()
            }
        }
    }
}

fn strip_program_name(words: &[String]) -> Vec<String> {
    if words.first().is_some_and(|word| is_tg_program_name(word)) {
        words[1..].to_vec()
    } else {
        words.to_vec()
    }
}

fn is_tg_program_name(word: &str) -> bool {
    Path::new(word).file_name().and_then(|name| name.to_str()) == Some("tg")
}

fn decrypted_dir_from(words: &[String]) -> Option<PathBuf> {
    let mut iter = words.iter().peekable();
    while let Some(word) = iter.next() {
        if word == "--decrypted-dir" {
            return iter.peek().map(|value| PathBuf::from(value.as_str()));
        }
        if let Some(value) = word.strip_prefix("--decrypted-dir=") {
            return Some(PathBuf::from(value));
        }
    }
    None
}

#[derive(Debug)]
struct CompletionContext<'a> {
    command: &'a Command,
    path: Vec<String>,
    positionals: Vec<String>,
}

fn command_context<'a>(root: &'a Command, user_words: &[String]) -> CompletionContext<'a> {
    let mut command = root;
    let mut path = Vec::new();
    let mut positionals = Vec::new();
    let mut expecting_value = false;

    for word in user_words {
        if expecting_value {
            expecting_value = false;
            continue;
        }
        if let Some(arg) = long_arg_from_word(command, word) {
            expecting_value = option_takes_value(arg) && !word.contains('=');
            continue;
        }
        if let Some(arg) = short_arg_from_word(command, word) {
            expecting_value = option_takes_value(arg);
            continue;
        }
        if word.starts_with('-') {
            continue;
        }
        if let Some(subcommand) = visible_subcommand(command, word) {
            command = subcommand;
            path.push(word.clone());
            positionals.clear();
            continue;
        }
        if path.is_empty() && std::ptr::eq(command, root) {
            if let Some(default_command) = visible_subcommand(root, "messages") {
                command = default_command;
                path.push("messages".to_string());
                positionals.push(word.clone());
                continue;
            }
        }
        positionals.push(word.clone());
    }

    CompletionContext {
        command,
        path,
        positionals,
    }
}

fn root_candidates(
    root: &Command,
    current: &str,
    decrypted_dir: &Path,
) -> Result<Vec<Candidate>, String> {
    let mut candidates = filter_candidates(command_candidates(root), current);
    candidates.extend(session_candidates(decrypted_dir, 200, Some(current))?);
    Ok(candidates)
}

fn complete_attached_value(
    current: &str,
    user_words: &[String],
    decrypted_dir: &Path,
) -> Result<Option<Vec<Candidate>>, String> {
    let Some((option_name, value_prefix)) = current.strip_prefix("--").and_then(|value| {
        let (name, value) = value.split_once('=')?;
        Some((name, value))
    }) else {
        return Ok(None);
    };

    let root = Cli::command();
    let context = command_context(&root, user_words);
    let Some(arg) = context
        .command
        .get_arguments()
        .find(|arg| !arg.is_hide_set() && arg.get_long() == Some(option_name))
    else {
        return Ok(None);
    };
    let Some(candidates) =
        value_candidates_for_arg(arg, &context.path, value_prefix, decrypted_dir)?
    else {
        return Ok(None);
    };
    let prefix = format!("--{option_name}=");
    let candidates = candidates
        .into_iter()
        .map(|candidate| Candidate {
            value: format!("{prefix}{}", candidate.value),
            description: candidate.description,
        })
        .collect();
    Ok(Some(candidates))
}

fn complete_value_after_previous(
    current: &str,
    user_words: &[String],
    decrypted_dir: &Path,
) -> Result<Option<Vec<Candidate>>, String> {
    let Some(previous) = user_words.last() else {
        return Ok(None);
    };

    let root = Cli::command();
    let context = command_context(&root, &user_words[..user_words.len() - 1]);
    option_from_token(context.command, previous)
        .map(|arg| value_candidates_for_arg(arg, &context.path, current, decrypted_dir))
        .transpose()
        .map(Option::flatten)
}

fn positional_candidates(
    context: &CompletionContext<'_>,
    current: &str,
    decrypted_dir: &Path,
) -> Result<Vec<Candidate>, String> {
    let index = context.positionals.len();
    if let Some(kind) = positional_dynamic_kind(&context.path, index) {
        return dynamic_candidates(kind, decrypted_dir, 200, current);
    }
    if let Some(arg) = positional_arg(context.command, index) {
        let values = possible_value_candidates(arg);
        if !values.is_empty() {
            return Ok(filter_candidates(values, current));
        }
    }
    Ok(Vec::new())
}

fn positional_dynamic_kind(path: &[String], index: usize) -> Option<DynamicKind> {
    match path {
        [command]
            if matches!(
                command.as_str(),
                "messages"
                    | "sessions"
                    | "unread"
                    | "doctor"
                    | "export"
                    | "image"
                    | "file"
                    | "sticker"
                    | "emoji"
                    | "voice"
            ) && index == 0 =>
        {
            Some(DynamicKind::Sessions)
        }
        _ => None,
    }
}

fn positional_arg(command: &Command, index: usize) -> Option<&Arg> {
    let positionals = command
        .get_positionals()
        .filter(|arg| !arg.is_hide_set())
        .collect::<Vec<_>>();
    positionals
        .get(index)
        .copied()
        .or_else(|| positionals.last().copied())
}

fn value_candidates_for_arg(
    arg: &Arg,
    _command_path: &[String],
    prefix: &str,
    decrypted_dir: &Path,
) -> Result<Option<Vec<Candidate>>, String> {
    if !option_takes_value(arg) {
        return Ok(None);
    }
    if let Some(completion) = arg_value_completion(arg) {
        return value_candidates(completion, prefix, decrypted_dir).map(Some);
    }
    let possible_values = possible_value_candidates(arg);
    if possible_values.is_empty() {
        Ok(None)
    } else {
        Ok(Some(filter_candidates(possible_values, prefix)))
    }
}

fn arg_value_completion(arg: &Arg) -> Option<ValueCompletion> {
    if let Some(completion) = value_hint_completion(arg.get_value_hint()) {
        return Some(completion);
    }

    let long = arg.get_long()?;
    match long {
        "session" => Some(ValueCompletion::Dynamic(DynamicKind::Sessions)),
        "decrypted-dir" | "output" | "db-dir" | "media-dir" | "dir" => {
            Some(ValueCompletion::Path(PathKind::Directories))
        }
        "keys" | "decoder" => Some(ValueCompletion::Path(PathKind::Files)),
        _ => None,
    }
}

fn value_hint_completion(value_hint: ValueHint) -> Option<ValueCompletion> {
    match value_hint {
        ValueHint::FilePath => Some(ValueCompletion::Path(PathKind::Files)),
        ValueHint::DirPath => Some(ValueCompletion::Path(PathKind::Directories)),
        _ => None,
    }
}

fn value_candidates(
    completion: ValueCompletion,
    prefix: &str,
    decrypted_dir: &Path,
) -> Result<Vec<Candidate>, String> {
    match completion {
        ValueCompletion::Dynamic(kind) => dynamic_candidates(kind, decrypted_dir, 200, prefix),
        ValueCompletion::Path(kind) => {
            let candidates = path_candidates(prefix, kind)?;
            Ok(filter_candidates(candidates, prefix))
        }
    }
}

fn dynamic_candidates(
    kind: DynamicKind,
    decrypted_dir: &Path,
    limit: usize,
    query: &str,
) -> Result<Vec<Candidate>, String> {
    match kind {
        DynamicKind::Sessions => session_candidates(decrypted_dir, limit, Some(query)),
    }
}

fn command_candidates(command: &Command) -> Vec<Candidate> {
    command
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
        .map(|command| Candidate {
            value: command.get_name().to_string(),
            description: command
                .get_about()
                .map(ToString::to_string)
                .unwrap_or_default(),
        })
        .collect()
}

fn option_candidates(command: &Command) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for arg in command.get_arguments().filter(|arg| !arg.is_hide_set()) {
        let description = arg.get_help().map(ToString::to_string).unwrap_or_default();
        if let Some(long) = arg.get_long() {
            candidates.push(Candidate {
                value: format!("--{long}"),
                description: description.clone(),
            });
        }
        if let Some(short) = arg.get_short() {
            candidates.push(Candidate {
                value: format!("-{short}"),
                description: description.clone(),
            });
        }
    }
    if !candidates
        .iter()
        .any(|candidate| candidate.value == "--help")
    {
        candidates.push(Candidate {
            value: "--help".to_string(),
            description: "Print help".to_string(),
        });
        candidates.push(Candidate {
            value: "-h".to_string(),
            description: "Print help".to_string(),
        });
    }
    if command.get_version().is_some()
        && !candidates
            .iter()
            .any(|candidate| candidate.value == "--version")
    {
        candidates.push(Candidate {
            value: "--version".to_string(),
            description: "Print version".to_string(),
        });
        candidates.push(Candidate {
            value: "-V".to_string(),
            description: "Print version".to_string(),
        });
    }
    candidates
}

fn possible_value_candidates(arg: &Arg) -> Vec<Candidate> {
    arg.get_possible_values()
        .into_iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| Candidate {
            value: value.get_name().to_string(),
            description: value
                .get_help()
                .map(ToString::to_string)
                .unwrap_or_default(),
        })
        .collect()
}

fn option_takes_value(arg: &Arg) -> bool {
    arg.get_action().takes_values()
}

fn option_from_token<'a>(command: &'a Command, token: &str) -> Option<&'a Arg> {
    long_arg_from_word(command, token).or_else(|| short_arg_from_word(command, token))
}

fn long_arg_from_word<'a>(command: &'a Command, word: &str) -> Option<&'a Arg> {
    let name = word.strip_prefix("--")?.split_once('=').map_or_else(
        || word.strip_prefix("--").unwrap_or_default(),
        |(name, _)| name,
    );
    command
        .get_arguments()
        .find(|arg| !arg.is_hide_set() && arg.get_long() == Some(name))
}

fn short_arg_from_word<'a>(command: &'a Command, word: &str) -> Option<&'a Arg> {
    let mut chars = word.strip_prefix('-')?.chars();
    let short = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    command
        .get_arguments()
        .find(|arg| !arg.is_hide_set() && arg.get_short() == Some(short))
}

fn visible_subcommand<'a>(command: &'a Command, name: &str) -> Option<&'a Command> {
    command
        .get_subcommands()
        .find(|command| !command.is_hide_set() && command.get_name() == name)
}

fn filter_candidates(candidates: Vec<Candidate>, prefix: &str) -> Vec<Candidate> {
    let mut unique = BTreeMap::<String, String>::new();
    for candidate in candidates {
        if candidate.value.starts_with(prefix) {
            unique
                .entry(candidate.value)
                .or_insert(candidate.description);
        }
    }
    unique
        .into_iter()
        .map(|(value, description)| Candidate { value, description })
        .collect()
}

fn path_candidates(prefix: &str, kind: PathKind) -> Result<Vec<Candidate>, String> {
    let (dir_prefix, name_prefix) = split_path_prefix(prefix);
    let read_dir = expand_tilde(if dir_prefix.is_empty() {
        "."
    } else {
        dir_prefix
    })?;
    let Ok(entries) = fs::read_dir(&read_dir) else {
        return Ok(Vec::new());
    };

    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("Read path completion entry: {}", e))?;
        let file_name = entry.file_name().to_string_lossy().to_string();
        if !name_prefix.starts_with('.') && file_name.starts_with('.') {
            continue;
        }
        if !file_name.starts_with(name_prefix) {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|e| format!("Read path completion file type: {}", e))?;
        let is_dir = file_type.is_dir();
        if kind == PathKind::Directories && !is_dir {
            continue;
        }
        let mut value = format!("{dir_prefix}{file_name}");
        if is_dir {
            value.push('/');
        }
        candidates.push(Candidate {
            value,
            description: if is_dir { "directory" } else { "file" }.to_string(),
        });
    }
    Ok(candidates)
}

fn split_path_prefix(prefix: &str) -> (&str, &str) {
    prefix
        .rfind('/')
        .map(|index| prefix.split_at(index + 1))
        .unwrap_or(("", prefix))
}

fn expand_tilde(path: &str) -> Result<PathBuf, String> {
    if path == "~" {
        return std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set".to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set".to_string())?;
        return Ok(home.join(rest));
    }
    Ok(PathBuf::from(path))
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
    username: String,
    display: String,
    remark: String,
    nick_name: String,
    alias: String,
    sort_key: String,
    is_stranger: bool,
}

impl ContactCompletionRow {
    fn from_contact(contact: contact::Contact) -> Self {
        let display = contact.personal_display_name().to_string();
        let sort_key = make_contact_sort_key(&display, &contact.username);
        Self {
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

    let contacts = contact::load_contacts(&contact_db)?;
    let rows = contacts
        .into_values()
        .filter(|contact| !contact.username.trim().is_empty())
        .map(ContactCompletionRow::from_contact)
        .collect::<Vec<_>>();

    let normalized_query = normalize_completion_text(query);
    if !normalized_query.is_empty() {
        let prefix_rows = rows
            .iter()
            .filter(|row| {
                row.fields().iter().any(|(_, field)| {
                    normalize_completion_text(field).starts_with(&normalized_query)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if !prefix_rows.is_empty() {
            return Ok(score_contact_rows(prefix_rows, limit, query));
        }
    }
    Ok(score_contact_rows(rows, limit, query))
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

fn normalize_completion_text(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
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
    use rusqlite::{params, Connection};
    use tempfile::TempDir;

    #[test]
    fn sanitize_removes_completion_separators() {
        assert_eq!(sanitize("a\tb\nc"), "a b c");
    }

    #[test]
    fn generated_shell_scripts_are_protocol_shims() {
        for script in [FISH_COMPLETIONS, ZSH_COMPLETIONS, BASH_COMPLETIONS] {
            assert!(script.contains("__complete words"));
            assert!(!script.contains("--anonymous"));
            assert!(!script.contains("--time-bucket"));
            assert!(!script.contains("messages|"));
            assert!(!script.contains("keys decrypt"));
        }
    }

    #[test]
    fn root_completion_comes_from_clap_commands() {
        let dir = tempfile::tempdir().unwrap();

        let candidates = complete(&["tg"], "se", dir.path());

        assert!(candidates.iter().any(|candidate| {
            candidate.value == "sessions"
                && candidate.description.starts_with("List all chat sessions")
        }));
        assert!(candidates
            .iter()
            .any(|candidate| candidate.value == "search"));
    }

    #[test]
    fn root_completion_also_includes_default_session_candidates() {
        let dir = create_decrypted_contacts(&[("tgid_alice", "Alice Zhang", "", "alice")]);

        let candidates = complete(&["tg"], "tgid_a", dir.path());

        assert!(candidates
            .iter()
            .any(|candidate| candidate.value == "tgid_alice"));
    }

    #[test]
    fn dynamic_session_completion_matches_display_not_only_value_prefix() {
        let dir = create_decrypted_contacts(&[("45573543064@chatroom", "Linux Club", "", "")]);

        for (words, current) in [
            (vec!["tg"], "Linu"),
            (vec!["tg", "messages"], "Linu"),
            (vec!["tg", "query", "--session"], "Linu"),
        ] {
            let candidates = complete(&words, current, dir.path());

            assert!(candidates.iter().any(|candidate| {
                candidate.value == "45573543064@chatroom" && candidate.description == "Linux Club"
            }));
        }
    }

    #[test]
    fn command_options_come_from_clap_command() {
        let dir = tempfile::tempdir().unwrap();

        let candidates = complete(&["tg", "messages"], "--ano", dir.path());

        assert!(candidates.iter().any(|candidate| {
            candidate.value == "--anonymous"
                && candidate
                    .description
                    .starts_with("Use group/member public names")
        }));
    }

    #[test]
    fn path_invoked_command_completion_strips_program_name() {
        let dir = tempfile::tempdir().unwrap();

        let candidates = complete(&["target/release/tg", "query"], "--se", dir.path());

        assert!(candidates
            .iter()
            .any(|candidate| candidate.value == "--session"));
        assert!(!candidates
            .iter()
            .any(|candidate| candidate.value == "--search"));
    }

    #[test]
    fn default_messages_options_come_from_messages_command() {
        let dir = tempfile::tempdir().unwrap();

        let candidates = complete(&["tg", "tgid_alice"], "--lim", dir.path());

        assert!(candidates
            .iter()
            .any(|candidate| candidate.value == "--limit"));
    }

    #[test]
    fn query_session_option_completes_sessions() {
        let dir = create_decrypted_contacts(&[("tgid_alice", "Alice Zhang", "", "alice")]);

        let candidates = complete(&["tg", "query", "--session"], "tgid_a", dir.path());

        assert_eq!(
            candidates,
            vec![Candidate {
                value: "tgid_alice".to_string(),
                description: "Alice Zhang".to_string(),
            }]
        );
    }

    #[test]
    fn clap_value_enum_positionals_complete_possible_values() {
        let dir = tempfile::tempdir().unwrap();

        let candidates = complete(&["tg", "completions"], "f", dir.path());

        assert_eq!(
            candidates,
            vec![Candidate {
                value: "fish".to_string(),
                description: String::new(),
            }]
        );
    }

    #[test]
    fn command_specific_static_values_complete_in_rust_resolver() {
        let dir = tempfile::tempdir().unwrap();

        let voice = complete(&["tg", "voice", "tgid_alice", "--format"], "w", dir.path());

        assert_eq!(
            voice,
            vec![Candidate {
                value: "wav".to_string(),
                description: "WAV output".to_string(),
            }]
        );

        let query = complete(&["tg", "query", "--order"], "n", dir.path());
        assert_eq!(
            query,
            vec![Candidate {
                value: "newest".to_string(),
                description: "Newest messages first".to_string(),
            }]
        );
    }

    #[test]
    fn clap_command_carries_completion_metadata() {
        let root = Cli::command();
        let messages = visible_subcommand(&root, "messages").unwrap();
        let time_bucket = long_arg_from_word(messages, "--time-bucket").unwrap();
        let decrypted_dir = long_arg_from_word(messages, "--decrypted-dir").unwrap();

        assert!(possible_value_candidates(time_bucket).contains(&Candidate {
            value: "1m".to_string(),
            description: "One-minute buckets".to_string(),
        }));
        assert_eq!(decrypted_dir.get_value_hint(), ValueHint::DirPath);
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
    fn session_completion_keeps_fuzzy_match() {
        let dir = create_decrypted_contacts(&[("tgid_alice", "Alice Zhang", "", "alice")]);

        let candidates = session_candidates(dir.path(), 10, Some("Alic Zhang")).unwrap();

        assert_eq!(candidates[0].value, "tgid_alice");
    }

    #[test]
    fn session_completion_prefers_prefix_pool_over_contains_matches() {
        let dir = create_decrypted_contacts(&[
            ("tgid_prefix", "Linux Club", "", ""),
            ("tgid_contains", "My Linux Club", "", ""),
        ]);

        let candidates = session_candidates(dir.path(), 10, Some("Linux")).unwrap();

        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.value.as_str())
                .collect::<Vec<_>>(),
            vec!["tgid_prefix"]
        );
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

    fn complete(words: &[&str], current: &str, decrypted_dir: &Path) -> Vec<Candidate> {
        let words = words
            .iter()
            .map(|word| (*word).to_string())
            .collect::<Vec<_>>();
        let request = CompletionRequest {
            kind: CompleteKind::Words,
            decrypted_dir,
            limit: 200,
            shell: Some(CompletionShell::Bash),
            cursor: Some(words.len()),
            current: Some(current),
            words: &words,
        };
        word_candidates(&request).unwrap()
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
