//! Derive AES-128 and XOR keys for .dat media decryption from Telegram kvcomm files.
//!
//! Key derivation:
//!   code   = extracted from `key_<code>_*.statistic` filename
//!   xorKey = code & 0xff
//!   aesKey = md5("{code}{cleanAccountId}").hex_lower()[0..16] as ASCII bytes

use aes::cipher::{Block, BlockCipherDecrypt, KeyInit};
use md5::{Digest, Md5};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, Zeroizing};

use crate::{dictionary, media_decrypt};

pub struct MediaKeys {
    /// 16-byte AES-128 key stored as ASCII bytes (NOT hex-decoded).
    pub aes_key: [u8; 16],
    /// XOR byte for the tail section.
    pub xor_key: u8,
}

impl Drop for MediaKeys {
    fn drop(&mut self) {
        self.aes_key.zeroize();
        self.xor_key.zeroize();
    }
}

/// V2 files checked per candidate code. A wrong key matches a 2-byte image
/// signature about once in 65536 blocks, so a few samples separate the codes.
const MAX_VERIFY_SAMPLES: usize = 4;

/// Derive media decryption keys for the current Telegram account.
///
/// `telegram_base` should be the path returned by [`media::find_telegram_base_path()`].
/// `samples` are cached media files the caller is about to decrypt; the V2
/// files among them decide which kvcomm code is the live one.
pub fn find_media_keys<'a>(
    telegram_base: &Path,
    samples: impl IntoIterator<Item = &'a Path>,
) -> Result<MediaKeys, String> {
    let clean_account_id = extract_clean_account_id(telegram_base)?;
    let kvcomm_dir = find_kvcomm_dir()?;
    select_media_keys(&kvcomm_dir, &clean_account_id, samples)
}

/// Pick the kvcomm code whose derived key decrypts the sample media.
///
/// The client can keep `key_<code>_*` files for several codes at once, so every
/// distinct code is a candidate. Each candidate is scored by how many sample V2
/// heads decrypt to a known media signature; the highest score wins and ties go
/// to the smallest code, so directory order never matters. When no V2 sample
/// exists, a single code is used as is and several codes fall back to the
/// smallest one with a warning: nothing in that export needs the key.
fn select_media_keys<'a>(
    kvcomm_dir: &Path,
    account_id: &str,
    samples: impl IntoIterator<Item = &'a Path>,
) -> Result<MediaKeys, String> {
    let codes = find_codes_in_kvcomm(kvcomm_dir)?;
    let heads = read_v2_heads(samples)?;

    if heads.is_empty() {
        if codes.len() > 1 {
            log::warn!(
                "{} media key codes found and no V2 media to verify them; using the smallest",
                codes.len()
            );
        }
        return Ok(media_keys_for(codes[0], account_id));
    }

    let mut best: Option<(usize, MediaKeys)> = None;
    for &code in codes.iter() {
        let keys = media_keys_for(code, account_id);
        let score = heads
            .iter()
            .filter(|head| decrypts_head(head, &keys))
            .count();
        if score > best.as_ref().map_or(0, |(best_score, _)| *best_score) {
            best = Some((score, keys));
        }
    }

    best.map(|(_, keys)| keys).ok_or_else(|| {
        format!(
            "None of the {} media key codes in {} decrypts the cached media ({} V2 files checked)",
            codes.len(),
            kvcomm_dir.display(),
            heads.len()
        )
    })
}

fn media_keys_for(code: u64, account_id: &str) -> MediaKeys {
    MediaKeys {
        aes_key: derive_aes_key(code, account_id),
        xor_key: (code & 0xff) as u8,
    }
}

/// First AES block of each V2 sample, up to [`MAX_VERIFY_SAMPLES`].
fn read_v2_heads<'a>(samples: impl IntoIterator<Item = &'a Path>) -> Result<Vec<[u8; 16]>, String> {
    let mut heads = Vec::new();
    for path in samples {
        let mut file =
            fs::File::open(path).map_err(|e| format!("Read {}: {}", path.display(), e))?;
        let mut buf = [0u8; 31];
        let mut len = 0;
        while len < buf.len() {
            let n = file
                .read(&mut buf[len..])
                .map_err(|e| format!("Read {}: {}", path.display(), e))?;
            if n == 0 {
                break;
            }
            len += n;
        }
        // Shorter files cannot be decrypted with any key; export reports them.
        if len == buf.len() && media_decrypt::is_v2_media_header(&buf) {
            let mut head = [0u8; 16];
            head.copy_from_slice(&buf[15..]);
            heads.push(head);
            if heads.len() == MAX_VERIFY_SAMPLES {
                break;
            }
        }
    }
    Ok(heads)
}

fn decrypts_head(head: &[u8; 16], keys: &MediaKeys) -> bool {
    let cipher = aes::Aes128::new_from_slice(&keys.aes_key).expect("AES-128 key is 16 bytes");
    let mut block = Zeroizing::new(*head);
    let b: &mut Block<aes::Aes128> = (&mut *block).into();
    cipher.decrypt_block(b);
    media_decrypt::detect_ext(&block[..]) != "bin"
}

/// Derive the 16-byte AES key as ASCII bytes.
fn derive_aes_key(code: u64, account_id: &str) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update(Zeroizing::new(code.to_string()).as_bytes());
    hasher.update(account_id.as_bytes());
    let mut digest = hasher.finalize();
    let hex = Zeroizing::new(hex::encode(digest));
    digest.zeroize();
    let mut key = [0u8; 16];
    key.copy_from_slice(&hex.as_bytes()[..16]);
    key
}

/// Extract the clean account id from the Telegram base path directory name.
pub(crate) fn extract_clean_account_id(base: &Path) -> Result<String, String> {
    let dir_name = base
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "Cannot extract directory name from telegram base path".to_string())?;

    // The directory name ends with `_<random>` suffix; strip it
    let account_prefix = dictionary::account_id_prefix();
    if dir_name.starts_with(account_prefix) || dir_name.starts_with("gh_") {
        if let Some(pos) = dir_name.rfind('_') {
            let clean = &dir_name[..pos];
            if clean.starts_with(account_prefix) || clean.starts_with("gh_") {
                return Ok(clean.to_string());
            }
        }
    }
    // If no suffix pattern, use as-is
    if dir_name.starts_with(account_prefix) || dir_name.starts_with("gh_") {
        return Ok(dir_name.to_string());
    }

    Err(format!(
        "Cannot determine clean account id from path: {}",
        base.display()
    ))
}

fn find_kvcomm_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    let home = PathBuf::from(home);
    let candidates = dictionary::kvcomm_candidate_dirs(&home);

    for candidate in &candidates {
        if candidate.is_dir() {
            return Ok(candidate.clone());
        }
    }

    Err(format!(
        "kvcomm directory not found at {}",
        candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// Every distinct code from `key_<code>_*.statistic` names, ascending.
fn find_codes_in_kvcomm(kvcomm_dir: &Path) -> Result<Zeroizing<Vec<u64>>, String> {
    let entries = fs::read_dir(kvcomm_dir).map_err(|e| format!("Cannot read kvcomm dir: {}", e))?;

    let mut codes = Zeroizing::new(Vec::new());
    for entry in entries {
        let entry = entry.map_err(|e| format!("Cannot read kvcomm dir: {}", e))?;
        if let Some(code) = try_extract_code(&entry.file_name().to_string_lossy()) {
            codes.push(code);
        }
    }
    codes.sort_unstable();
    codes.dedup();

    if codes.is_empty() {
        return Err(format!(
            "No key_<code>_*.statistic file found in {}",
            kvcomm_dir.display()
        ));
    }
    Ok(codes)
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

    const PNG_HEAD: [u8; 16] = [
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, b'I', b'H', b'D', b'R',
    ];

    fn account_id() -> String {
        format!("{}1234567", dictionary::account_id_prefix())
    }

    fn kvcomm_with_codes(codes: &[u64]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.ini"), b"").unwrap();
        for (i, code) in codes.iter().enumerate() {
            let name = format!("key_{code}_{i}_1_1700000000_1_3600_input.statistic");
            fs::write(dir.path().join(name), b"").unwrap();
        }
        dir
    }

    /// A V2 file whose first AES block is a PNG head encrypted with `code`'s key.
    fn v2_sample(dir: &Path, name: &str, code: u64) -> PathBuf {
        use aes::cipher::BlockCipherEncrypt;

        let keys = media_keys_for(code, &account_id());
        let cipher = aes::Aes128::new_from_slice(&keys.aes_key).unwrap();
        let mut block = PNG_HEAD;
        cipher.encrypt_block((&mut block).into());
        let mut data = media_decrypt::V2_MAGIC.to_vec();
        data.extend_from_slice(&1024u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.push(1);
        data.extend_from_slice(&block);
        let path = dir.join(name);
        fs::write(&path, data).unwrap();
        path
    }

    fn select(kvcomm: &Path, samples: &[PathBuf]) -> Result<MediaKeys, String> {
        select_media_keys(kvcomm, &account_id(), samples.iter().map(PathBuf::as_path))
    }

    fn assert_keys_for(keys: &MediaKeys, code: u64) {
        let expected = media_keys_for(code, &account_id());
        assert_eq!(keys.aes_key, expected.aes_key);
        assert_eq!(keys.xor_key, expected.xor_key);
    }

    #[test]
    fn single_code_is_verified_against_v2_sample() {
        let kvcomm = kvcomm_with_codes(&[1111, 1111]);
        let media = tempfile::tempdir().unwrap();
        let sample = v2_sample(media.path(), "a.dat", 1111);

        assert_keys_for(&select(kvcomm.path(), &[sample]).unwrap(), 1111);
    }

    #[test]
    fn picks_the_code_that_decrypts_regardless_of_directory_order() {
        let media = tempfile::tempdir().unwrap();
        let plain = media.path().join("plain.jpg");
        fs::write(&plain, [0xFF, 0xD8, 0xFF, 0xE0]).unwrap();
        let samples = vec![
            plain,
            v2_sample(media.path(), "a.dat", 2222),
            v2_sample(media.path(), "b.dat", 2222),
        ];

        for codes in [[1111, 2222, 3333], [3333, 2222, 1111]] {
            let kvcomm = kvcomm_with_codes(&codes);
            assert_keys_for(&select(kvcomm.path(), &samples).unwrap(), 2222);
        }
    }

    #[test]
    fn fails_when_no_code_decrypts() {
        let kvcomm = kvcomm_with_codes(&[1111, 3333]);
        let media = tempfile::tempdir().unwrap();
        let sample = v2_sample(media.path(), "a.dat", 2222);

        let err = select(kvcomm.path(), &[sample]).err().unwrap();
        assert!(err.contains("None of the 2 media key codes"), "{err}");
        assert!(!err.contains("1111") && !err.contains("3333"), "{err}");
    }

    #[test]
    fn without_v2_sample_uses_single_code_or_smallest_of_several() {
        let media = tempfile::tempdir().unwrap();
        let plain = media.path().join("plain.jpg");
        fs::write(&plain, [0xFF, 0xD8, 0xFF, 0xE0]).unwrap();
        let samples = [plain];

        let single = kvcomm_with_codes(&[4444]);
        assert_keys_for(&select(single.path(), &samples).unwrap(), 4444);

        let several = kvcomm_with_codes(&[5555, 4444, 6666]);
        assert_keys_for(&select(several.path(), &[]).unwrap(), 4444);
    }

    #[test]
    fn fails_without_any_key_file() {
        let kvcomm = kvcomm_with_codes(&[]);
        let err = select(kvcomm.path(), &[]).err().unwrap();
        assert!(err.contains("No key_<code>_*.statistic file"), "{err}");
    }

    #[test]
    fn test_derive_aes_key() {
        let account_id = format!("{}7286922865011", dictionary::account_id_prefix());
        let key = derive_aes_key(1020215821, &account_id);
        let s = std::str::from_utf8(&key).unwrap();
        assert_eq!(s, "68ec773d54b0245b");
    }
}
