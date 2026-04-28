//! Derive AES-128 and XOR keys for .dat media decryption from Telegram kvcomm files.
//!
//! Key derivation:
//!   code   = extracted from `key_<code>_*.statistic` filename
//!   xorKey = code & 0xff
//!   aesKey = md5("{code}{cleanTgid}").hex_lower()[0..16] as ASCII bytes

use md5::{Digest, Md5};
use std::fs;
use std::path::{Path, PathBuf};

const KVCOMM_REL: &str = "Documents/app_data/net/kvcomm";

pub struct MediaKeys {
    /// 16-byte AES-128 key stored as ASCII bytes (NOT hex-decoded).
    pub aes_key: [u8; 16],
    /// XOR byte for the tail section.
    pub xor_key: u8,
}

/// Derive media decryption keys for the current Telegram account.
///
/// `telegram_base` should be the path returned by [`media::find_telegram_base_path()`],
/// e.g. `.../xtelegram_files/tgid_XXXX_e4d5/`.
pub fn find_media_keys(telegram_base: &Path) -> Result<MediaKeys, String> {
    let clean_tgid = extract_clean_tgid(telegram_base)?;
    let kvcomm_dir = find_kvcomm_dir()?;
    let code = find_code_in_kvcomm(&kvcomm_dir)?;

    let xor_key = (code & 0xff) as u8;
    let aes_key = derive_aes_key(code, &clean_tgid);

    Ok(MediaKeys { aes_key, xor_key })
}

/// Derive the 16-byte AES key as ASCII bytes.
fn derive_aes_key(code: u64, tgid: &str) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update(code.to_string().as_bytes());
    hasher.update(tgid.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{:x}", digest);
    let hex16 = &hex[..16];
    let mut key = [0u8; 16];
    key.copy_from_slice(hex16.as_bytes());
    key
}

/// Extract the clean tgid from the telegram base path directory name.
///
/// Input:  `.../xtelegram_files/tgid_7286922865011_e4d5/`
/// Output: `tgid_7286922865011`
fn extract_clean_tgid(base: &Path) -> Result<String, String> {
    let dir_name = base
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "Cannot extract directory name from telegram base path".to_string())?;

    // The directory name ends with `_<random>` suffix; strip it
    if dir_name.starts_with("tgid_") || dir_name.starts_with("gh_") {
        if let Some(pos) = dir_name.rfind('_') {
            let clean = &dir_name[..pos];
            if clean.starts_with("tgid_") || clean.starts_with("gh_") {
                return Ok(clean.to_string());
            }
        }
    }
    // If no suffix pattern, use as-is
    if dir_name.starts_with("tgid_") || dir_name.starts_with("gh_") {
        return Ok(dir_name.to_string());
    }

    Err(format!(
        "Cannot determine clean tgid from path: {}",
        base.display()
    ))
}

fn find_kvcomm_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    let candidate = PathBuf::from(&home)
        .join("Library/Containers/com.telegram.xinTelegram/Data")
        .join(KVCOMM_REL);

    if candidate.is_dir() {
        return Ok(candidate);
    }
    Err(format!(
        "kvcomm directory not found at {}",
        candidate.display()
    ))
}

/// Find the `key_<code>_*.statistic` file and extract the code.
fn find_code_in_kvcomm(kvcomm_dir: &Path) -> Result<u64, String> {
    let entries = fs::read_dir(kvcomm_dir).map_err(|e| format!("Cannot read kvcomm dir: {}", e))?;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy().to_string();
        if let Some(code) = try_extract_code(&name) {
            return Ok(code);
        }
    }

    Err(format!(
        "No key_<code>_*.statistic file found in {}",
        kvcomm_dir.display()
    ))
}

/// Try to extract the numeric code from a filename like `key_1020215821_4066646301_1_..._input.statistic`.
fn try_extract_code(filename: &str) -> Option<u64> {
    if !filename.starts_with("key_") {
        return None;
    }
    let rest = filename.strip_prefix("key_")?;
    let end = rest.find('_')?;
    rest[..end].parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_code() {
        assert_eq!(
            try_extract_code(
                "key_1020215821_4066646301_1_1777357465_1581567597_3600_input.statistic"
            ),
            Some(1020215821)
        );
        assert_eq!(try_extract_code("key_reportnow_1020215821_..."), None); // non-numeric after key_
        assert_eq!(try_extract_code("config.ini"), None);
        assert_eq!(try_extract_code("monitordata_1020215821_20571"), None);
    }

    #[test]
    fn test_derive_aes_key() {
        // Known sample: code=1020215821, tgid=tgid_7286922865011
        let key = derive_aes_key(1020215821, "tgid_7286922865011");
        let s = std::str::from_utf8(&key).unwrap();
        assert_eq!(s, "68ec773d54b0245b");
    }
}
