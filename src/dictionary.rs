use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const MASK: u8 = 0x5a;

fn decode_text(encoded: &[u8]) -> String {
    let bytes: Vec<u8> = encoded.iter().map(|byte| byte ^ MASK).collect();
    String::from_utf8(bytes).expect("dictionary entry must be valid UTF-8")
}

fn decode_bytes(encoded: &[u8]) -> Vec<u8> {
    encoded.iter().map(|byte| byte ^ MASK).collect()
}

fn cached_text(cell: &'static OnceLock<String>, encoded: &[u8]) -> &'static str {
    cell.get_or_init(|| decode_text(encoded)).as_str()
}

fn cached_bytes(cell: &'static OnceLock<Vec<u8>>, encoded: &[u8]) -> &'static [u8] {
    cell.get_or_init(|| decode_bytes(encoded)).as_slice()
}

pub(crate) fn desktop_app_process() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    #[cfg(target_os = "linux")]
    {
        cached_text(&VALUE, &[45, 63, 57, 50, 59, 46])
    }
    #[cfg(not(target_os = "linux"))]
    {
        cached_text(&VALUE, &[13, 63, 25, 50, 59, 46])
    }
}

pub(crate) fn desktop_app_localized_name() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    cached_text(&VALUE, &[191, 228, 244, 190, 229, 251])
}

/// The app's name as people write it, the same on every platform.
pub(crate) fn app_display_name() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    cached_text(&VALUE, &[13, 63, 25, 50, 59, 46])
}

/// Replaces the public cover name in text shown only to the owner's own tools
/// (the installed skill, MCP metadata) with the real app name, so the source
/// and public docs keep the cover name.
pub(crate) fn render_app_names(template: &str) -> String {
    let app_name = app_display_name();
    let localized_name = desktop_app_localized_name();
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
        .replace("telegram", &app_name.to_ascii_lowercase())
}

pub(crate) fn container_id() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    cached_text(
        &VALUE,
        &[
            57, 53, 55, 116, 46, 63, 52, 57, 63, 52, 46, 116, 34, 51, 52, 13, 63, 25, 50, 59, 46,
        ],
    )
}

pub(crate) fn account_files_dir() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    cached_text(&VALUE, &[34, 45, 63, 57, 50, 59, 46, 5, 60, 51, 54, 63, 41])
}

pub(crate) fn account_id_prefix() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    cached_text(&VALUE, &[45, 34, 51, 62, 5])
}

pub(crate) fn sticker_magic() -> &'static [u8] {
    static VALUE: OnceLock<Vec<u8>> = OnceLock::new();
    cached_bytes(&VALUE, &[45, 34, 61, 60])
}

#[cfg(target_os = "linux")]
pub(crate) fn cipher_config_anchor() -> &'static [u8] {
    static VALUE: OnceLock<Vec<u8>> = OnceLock::new();
    cached_bytes(
        &VALUE,
        &[
            57, 53, 55, 116, 14, 63, 52, 57, 63, 52, 46, 116, 13, 25, 30, 24, 116, 25, 53, 52, 60,
            51, 61, 116, 25, 51, 42, 50, 63, 40,
        ],
    )
}

pub(crate) fn msg_body_column() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    cached_text(
        &VALUE,
        &[55, 63, 41, 41, 59, 61, 63, 5, 57, 53, 52, 46, 63, 52, 46],
    )
}

pub(crate) fn msg_compression_marker_column() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    cached_text(
        &VALUE,
        &[
            13, 25, 30, 24, 5, 25, 14, 5, 55, 63, 41, 41, 59, 61, 63, 5, 57, 53, 52, 46, 63, 52, 46,
        ],
    )
}

pub(crate) fn msg_sender_column() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    cached_text(
        &VALUE,
        &[40, 63, 59, 54, 5, 41, 63, 52, 62, 63, 40, 5, 51, 62],
    )
}

pub(crate) fn msg_packed_meta_column() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    cached_text(
        &VALUE,
        &[42, 59, 57, 49, 63, 62, 5, 51, 52, 60, 53, 5, 62, 59, 46, 59],
    )
}

pub(crate) fn container_data_dir(home: &Path) -> PathBuf {
    home.join("Library/Containers")
        .join(container_id())
        .join("Data")
}

pub(crate) fn documents_account_files_dir(home: &Path) -> PathBuf {
    container_data_dir(home)
        .join("Documents")
        .join(account_files_dir())
}

pub(crate) fn linux_account_files_dir(home: &Path) -> PathBuf {
    home.join("Documents").join(account_files_dir())
}

pub(crate) fn account_files_candidate_dirs(home: &Path) -> Vec<PathBuf> {
    let native = if cfg!(target_os = "linux") {
        linux_account_files_dir(home)
    } else {
        documents_account_files_dir(home)
    };
    let fallback = if cfg!(target_os = "linux") {
        documents_account_files_dir(home)
    } else {
        linux_account_files_dir(home)
    };

    if native == fallback {
        vec![native]
    } else {
        vec![native, fallback]
    }
}

pub(crate) fn app_support_dir(home: &Path) -> PathBuf {
    container_data_dir(home)
        .join("Library/Application Support")
        .join(container_id())
}

pub(crate) fn kvcomm_dir(home: &Path) -> PathBuf {
    container_data_dir(home).join("Documents/app_data/net/kvcomm")
}

pub(crate) fn linux_config_dir(home: &Path) -> PathBuf {
    static VALUE: OnceLock<String> = OnceLock::new();
    home.join(cached_text(&VALUE, &[116, 34, 45, 63, 57, 50, 59, 46]))
}

pub(crate) fn kvcomm_candidate_dirs(home: &Path) -> Vec<PathBuf> {
    let native = if cfg!(target_os = "linux") {
        linux_config_dir(home).join("net/kvcomm")
    } else {
        kvcomm_dir(home)
    };
    let fallback = if cfg!(target_os = "linux") {
        kvcomm_dir(home)
    } else {
        linux_config_dir(home).join("net/kvcomm")
    };

    if native == fallback {
        vec![native]
    } else {
        vec![native, fallback]
    }
}
