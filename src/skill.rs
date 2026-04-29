use crate::dictionary;
use std::env;
use std::fs;
use std::path::PathBuf;

const SKILL_TEMPLATE: &str = include_str!("../SKILL.md");

pub(crate) struct InstallOptions {
    pub target_dir: Option<PathBuf>,
}

pub(crate) fn install(options: InstallOptions) -> Result<PathBuf, String> {
    let target_dir = match options.target_dir {
        Some(path) => path,
        None => default_skill_dir()?,
    };
    fs::create_dir_all(&target_dir)
        .map_err(|e| format!("Cannot create {}: {}", target_dir.display(), e))?;

    let skill_path = target_dir.join("SKILL.md");
    fs::write(&skill_path, render_skill_template(SKILL_TEMPLATE))
        .map_err(|e| format!("Cannot write {}: {}", skill_path.display(), e))?;
    Ok(skill_path)
}

fn default_skill_dir() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("TG_SKILL_DIR") {
        return Ok(PathBuf::from(path));
    }

    let codex_home = match env::var_os("CODEX_HOME") {
        Some(path) => PathBuf::from(path),
        None => home_dir()?.join(".codex"),
    };
    Ok(codex_home.join("skills").join("tg"))
}

fn home_dir() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set; pass --dir to choose a skill directory".to_string())
}

pub(crate) fn render_skill_template(template: &str) -> String {
    let app_name = dictionary::desktop_app_name();
    let localized_name = dictionary::desktop_app_localized_name();
    let lower_app_name = app_name.to_ascii_lowercase();

    let mut rendered = template.to_string();
    let localized_replacements = [
        ("Telegram聊天记录", format!("{}聊天记录", localized_name)),
        ("Telegram聊天", format!("{}聊天", localized_name)),
        ("Telegram群", format!("{}群", localized_name)),
        ("Telegram里", format!("{}里", localized_name)),
        ("Telegram正在运行", format!("{}正在运行", localized_name)),
    ];

    for (from, to) in localized_replacements {
        rendered = rendered.replace(from, &to);
    }

    rendered
        .replace("Telegram", &app_name)
        .replace("telegram", &lower_app_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn render_replaces_public_app_words_from_dictionary() {
        let rendered = render_skill_template(
            "macOS Telegram Telegram聊天记录 telegram /Applications/Telegram.app",
        );

        assert!(rendered.contains(&dictionary::desktop_app_name()));
        assert!(rendered.contains(&dictionary::desktop_app_localized_name()));
        assert!(!rendered.contains("Telegram"));
        assert!(!rendered.contains("telegram"));
    }

    #[test]
    fn install_writes_rendered_skill_md() {
        let dir = TempDir::new().unwrap();
        let path = install(InstallOptions {
            target_dir: Some(dir.path().join("tg")),
        })
        .unwrap();
        let contents = fs::read_to_string(&path).unwrap();

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("SKILL.md")
        );
        assert!(contents.contains(&dictionary::desktop_app_name()));
        assert!(!contents.contains("Telegram"));
    }
}
