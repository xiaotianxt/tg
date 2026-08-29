use crate::dictionary;
use std::env;
use std::fs;
use std::path::PathBuf;

const SKILL_TEMPLATE: &str = include_str!("../SKILL.md");

pub(crate) fn install(target_dir: Option<PathBuf>) -> Result<PathBuf, String> {
    let target_dir = target_dir.map_or_else(default_skill_dir, Ok)?;
    if fs::symlink_metadata(&target_dir).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(format!(
            "{} is a symlink; remove it explicitly or choose --dir",
            target_dir.display()
        ));
    }
    fs::create_dir_all(&target_dir)
        .map_err(|error| format!("Cannot create {}: {error}", target_dir.display()))?;

    let skill_path = target_dir.join("SKILL.md");
    fs::write(&skill_path, render_skill_template(SKILL_TEMPLATE))
        .map_err(|error| format!("Cannot write {}: {error}", skill_path.display()))?;
    Ok(skill_path)
}

fn default_skill_dir() -> Result<PathBuf, String> {
    let codex_home = match env::var_os("CODEX_HOME") {
        Some(path) => PathBuf::from(path),
        None => env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set; pass --dir to choose a skill directory".to_string())?
            .join(".codex"),
    };
    Ok(codex_home.join("skills").join("tg"))
}

fn render_skill_template(template: &str) -> String {
    let app_name = dictionary::desktop_app_process();
    let localized_name = dictionary::desktop_app_localized_name();
    let lower_app_name = app_name.to_ascii_lowercase();

    let mut rendered = template.to_string();
    for (from, to) in [
        ("Telegram聊天", format!("{localized_name}聊天")),
        ("Telegram群", format!("{localized_name}群")),
        ("Telegram里", format!("{localized_name}里")),
    ] {
        rendered = rendered.replace(from, &to);
    }

    rendered
        .replace("Telegram", app_name)
        .replace("telegram", &lower_app_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn install_writes_rendered_skill_md() {
        let dir = TempDir::new().unwrap();
        let path = install(Some(dir.path().join("tg"))).unwrap();
        let contents = fs::read_to_string(&path).unwrap();

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("SKILL.md")
        );
        assert!(contents.contains(dictionary::desktop_app_localized_name()));
        assert!(!contents.contains("Telegram"));
        assert!(!contents.contains("telegram"));
    }

    #[cfg(unix)]
    #[test]
    fn install_refuses_skill_directory_symlink() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::create_dir(&source).unwrap();
        symlink(&source, &target).unwrap();

        let error = install(Some(target.clone())).unwrap_err();

        assert!(error.contains("is a symlink"));
        assert!(fs::symlink_metadata(&target)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!source.join("SKILL.md").exists());
    }
}
