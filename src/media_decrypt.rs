//! Telegram 4.x V2 .dat media file decryption.
//!
//! # V2 file format (15-byte header)
//!
//! | Offset | Size | Field     |
//! |--------|------|-----------|
//! | 0      | 6    | magic     |
//! | 6      | 4    | aesLen    |  u32 LE
//! | 10     | 4    | xorLen    |  u32 LE
//! | 14     | 1    | flag      |  usually 0x01
//! | 15     |      | payload   |
//!
//! # Payload layout
//!
//! | Section  | Size                            | Description              |
//! |----------|---------------------------------|--------------------------|
//! | AES      | aesCipherLen                    | AES-128-ECB enciphered   |
//! | middle   | payload.len - aesCipher - xorLen | plaintext                |
//! | XOR tail | xorLen                          | each byte ^ xorKey       |
//!
//! aesCipherLen = if aesLen is block-aligned { aesLen + 16 } else { aesLen rounded up to 16 }

use crate::{dictionary, media_key::MediaKeys};
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// Magic bytes for V2 .dat files.
pub const V2_MAGIC: [u8; 6] = [0x07, 0x08, 0x56, 0x32, 0x08, 0x07];

/// Check whether a cached media file starts with the V2 magic bytes.
pub fn is_v2_media_header(header: &[u8]) -> bool {
    header.len() >= 6 && header[..6] == V2_MAGIC
}

/// Check a cached media file's header without relying on its extension.
pub fn file_has_v2_magic(src: &Path) -> Result<bool, String> {
    let mut file = fs::File::open(src).map_err(|e| format!("Read {}: {}", src.display(), e))?;
    let mut header = [0u8; 6];
    let len = file
        .read(&mut header)
        .map_err(|e| format!("Read {}: {}", src.display(), e))?;
    Ok(is_v2_media_header(&header[..len]))
}

/// Detect the output file extension from decrypted content magic bytes.
pub fn detect_ext(data: &[u8]) -> &'static str {
    let sticker_magic = dictionary::sticker_magic();
    if data.len() >= 4 && data[..4] == [0x89, 0x50, 0x4E, 0x47] {
        "png"
    } else if data.len() >= 2 && data[..2] == [0xFF, 0xD8] {
        "jpg"
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        "webp"
    } else if data.len() >= 12 && &data[4..8] == b"ftyp" {
        match &data[8..12] {
            b"avif" | b"avis" => "avif",
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1" => "heic",
            _ => "mp4",
        }
    } else if data.len() >= 4 && &data[..4] == b"GIF8" {
        "gif"
    } else if data.len() >= 2 && data[..2] == [0x42, 0x4D] {
        "bmp"
    } else if data.len() >= sticker_magic.len() && data[..sticker_magic.len()] == sticker_magic[..]
    {
        "tggf"
    } else {
        "bin"
    }
}

/// Decrypt a V2 .dat file and write the result to `dest`.
///
/// `dest` will be overwritten. On success, returns the detected extension (e.g. "jpg", "png").
pub fn decrypt_v2_dat(src: &Path, dest: &Path, keys: &MediaKeys) -> Result<&'static str, String> {
    let data = fs::read(src).map_err(|e| format!("Read {}: {}", src.display(), e))?;
    if data.len() < 15 {
        return Err(format!(
            "File too small for V2 header: {} bytes",
            data.len()
        ));
    }
    if !is_v2_media_header(&data) {
        return Err(format!("Not a V2 media file: {}", src.display()));
    }

    let aes_len = u32::from_le_bytes([data[6], data[7], data[8], data[9]]) as usize;
    let xor_len = u32::from_le_bytes([data[10], data[11], data[12], data[13]]) as usize;

    let aes_cipher_len = if aes_len.is_multiple_of(16) {
        aes_len + 16
    } else {
        aes_len.div_ceil(16) * 16
    };

    if 15 + aes_cipher_len > data.len() {
        return Err(format!(
            "AES cipher len {} exceeds file ({} bytes)",
            aes_cipher_len,
            data.len()
        ));
    }

    // AES-128-ECB decrypt
    let encrypted = &data[15..15 + aes_cipher_len];
    let decrypted = aes_ecb_decrypt(encrypted, &keys.aes_key);
    let unpadded = pkcs7_unpad(&decrypted);
    let plaintext_len = unpadded.len();
    let mut result = unpadded.to_vec();

    // Middle plaintext section (between AES cipher and XOR tail)
    let body_start = 15 + aes_cipher_len;
    let xor_start = data.len() - xor_len;
    if xor_start > body_start {
        result.extend_from_slice(&data[body_start..xor_start]);
    }

    // XOR tail
    let xored = &data[xor_start..];
    result.extend(xored.iter().map(|b| b ^ keys.xor_key));

    // Detect extension and write
    let mut ext = detect_ext(&result[..plaintext_len.min(result.len())]);
    let output = if ext == "tggf" {
        let jpg = convert_tggf_to_jpg(&result)?;
        ext = "jpg";
        jpg
    } else {
        result
    };

    let parent = dest.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| format!("Create dir {}: {}", parent.display(), e))?;
    fs::write(dest, &output).map_err(|e| format!("Write {}: {}", dest.display(), e))?;

    Ok(ext)
}

pub fn convert_tggf_to_jpg(data: &[u8]) -> Result<Vec<u8>, String> {
    let hevc = find_tggf_hevc_partition(data)
        .ok_or_else(|| "tggf HEVC partition not found".to_string())?;

    let ffmpeg = std::env::var("TG_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string());
    let mut child = Command::new(&ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "hevc",
            "-i",
            "pipe:0",
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("run ffmpeg for tggf: {}", e))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(hevc)
            .map_err(|e| format!("write tggf HEVC to ffmpeg: {}", e))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("wait for ffmpeg: {}", e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ffmpeg tggf decode failed: {}", stderr.trim()));
    }
    if detect_ext(&output.stdout) != "jpg" {
        return Err("ffmpeg tggf output is not JPEG".to_string());
    }

    Ok(output.stdout)
}

fn find_tggf_hevc_partition(data: &[u8]) -> Option<&[u8]> {
    if detect_ext(data) != "tggf" || data.len() < 8 {
        return None;
    }

    let header_len = data[4] as usize;
    let start_at = header_len.clamp(4, data.len());
    let mut best: Option<(usize, usize)> = None;
    let mut i = start_at;

    while i + 3 < data.len() {
        let is_start_code =
            data[i..].starts_with(&[0, 0, 0, 1]) || data[i..].starts_with(&[0, 0, 1]);
        if is_start_code && i >= 4 {
            let len =
                u32::from_be_bytes([data[i - 4], data[i - 3], data[i - 2], data[i - 1]]) as usize;
            if len > 0 && i + len <= data.len() {
                match best {
                    Some((_, best_len)) if best_len >= len => {}
                    _ => best = Some((i, len)),
                }
            }
        }
        i += 1;
    }

    if let Some((offset, len)) = best {
        Some(&data[offset..offset + len])
    } else {
        find_start_code(data, start_at).map(|offset| &data[offset..])
    }
}

fn find_start_code(data: &[u8], start_at: usize) -> Option<usize> {
    let mut i = start_at;
    while i + 3 < data.len() {
        if data[i..].starts_with(&[0, 0, 0, 1]) || data[i..].starts_with(&[0, 0, 1]) {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn aes_ecb_decrypt(data: &[u8], key: &[u8; 16]) -> Vec<u8> {
    use aes::cipher::{BlockDecrypt, KeyInit};
    use aes::Aes128;

    let cipher = Aes128::new_from_slice(key).expect("AES-128 key must be 16 bytes");
    let mut buf = data.to_vec();
    for block in buf.chunks_exact_mut(16) {
        let b = aes::cipher::generic_array::GenericArray::from_mut_slice(block);
        cipher.decrypt_block(b);
    }
    buf
}

fn pkcs7_unpad(data: &[u8]) -> &[u8] {
    if data.is_empty() {
        return data;
    }
    let pad_len = data[data.len() - 1] as usize;
    if pad_len == 0 || pad_len > 16 {
        return data; // not padded or invalid
    }
    // Verify all padding bytes
    for &b in data[data.len() - pad_len..].iter() {
        if b as usize != pad_len {
            return data; // invalid padding, return as-is
        }
    }
    &data[..data.len() - pad_len]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_v2_media_header() {
        assert!(is_v2_media_header(&[0x07, 0x08, 0x56, 0x32, 0x08, 0x07]));
        assert!(!is_v2_media_header(&[0x07, 0x08, 0x56, 0x31, 0x08, 0x07]));
        assert!(!is_v2_media_header(&[0; 6]));
    }

    #[test]
    fn test_file_has_v2_magic_does_not_require_extension() {
        let dir = tempfile::tempdir().unwrap();
        let extensionless = dir.path().join("cached-image");
        let plain = dir.path().join("plain.dat");
        std::fs::write(&extensionless, V2_MAGIC).unwrap();
        std::fs::write(&plain, b"not-v2").unwrap();

        assert!(file_has_v2_magic(&extensionless).unwrap());
        assert!(!file_has_v2_magic(&plain).unwrap());
    }

    #[test]
    fn test_detect_ext() {
        assert_eq!(detect_ext(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]), "jpg");
        assert_eq!(detect_ext(&[0x89, 0x50, 0x4E, 0x47, 0x0D]), "png");
        assert_eq!(detect_ext(b"GIF89a"), "gif");
        assert_eq!(detect_ext(b"RIFF\x00\x00\x00\x00WEBP"), "webp");
        assert_eq!(detect_ext(b"\x00\x00\x00\x18ftypmp42"), "mp4");
        assert_eq!(detect_ext(b"\x00\x00\x00\x18ftypavif"), "avif");
        let mut data = dictionary::sticker_magic().to_vec();
        data.extend_from_slice(b"\x13\x00\x00\x00");
        assert_eq!(detect_ext(&data), "tggf");
    }

    #[test]
    fn test_pkcs7_unpad() {
        assert_eq!(pkcs7_unpad(b"hello\x03\x03\x03"), b"hello");
        assert_eq!(pkcs7_unpad(b"no padding  "), b"no padding  "); // 0x20 != 16
        assert_eq!(pkcs7_unpad(b"hello\x01"), b"hello");
        assert_eq!(pkcs7_unpad(b""), b"");
    }

    #[test]
    fn test_find_tggf_hevc_partition() {
        let hevc = b"\x00\x00\x00\x01\x40\x01\x0c\x01";
        let mut data = dictionary::sticker_magic().to_vec();
        data.extend_from_slice(b"\x08abc");
        data.extend_from_slice(&(hevc.len() as u32).to_be_bytes());
        data.extend_from_slice(hevc);
        data.extend_from_slice(b"tail");

        assert_eq!(find_tggf_hevc_partition(&data), Some(&hevc[..]));
    }
}
