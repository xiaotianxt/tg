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
//! aesCipherLen = if aesLen % 16 == 0 { aesLen + 16 } else { (aesLen + 15) / 16 * 16 }

use std::fs;
use std::path::Path;
use crate::media_key::MediaKeys;

/// Magic bytes for V2 .dat files.
pub const V2_MAGIC: [u8; 6] = [0x07, 0x08, 0x56, 0x32, 0x08, 0x07];

/// Check whether a .dat file uses the V2 format.
pub fn is_v2_dat(header: &[u8]) -> bool {
    header.len() >= 6 && header[..6] == V2_MAGIC
}

/// Detect the output file extension from decrypted content magic bytes.
pub fn detect_ext(data: &[u8]) -> &'static str {
    if data.len() >= 4 && data[..4] == [0x89, 0x50, 0x4E, 0x47] {
        "png"
    } else if data.len() >= 2 && data[..2] == [0xFF, 0xD8] {
        "jpg"
    } else if data.len() >= 4 && data[..4] == [0x52, 0x49, 0x46, 0x46] {
        "webp"
    } else if data.len() >= 4 && data[..4] == [0x00, 0x00, 0x00, 0x18] {
        "mp4"  // ftypmp4
    } else if data.len() >= 4 && data[..4] == [0x66, 0x74, 0x79, 0x70] {
        "mp4"  // ftyp
    } else if data.len() >= 4 && data[..4] == [0x47, 0x49, 0x46, 0x38] {
        "gif"
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
        return Err(format!("File too small for V2 header: {} bytes", data.len()));
    }
    if !is_v2_dat(&data) {
        return Err(format!("Not a V2 .dat file: {}", src.display()));
    }

    let aes_len = u32::from_le_bytes([data[6], data[7], data[8], data[9]]) as usize;
    let xor_len = u32::from_le_bytes([data[10], data[11], data[12], data[13]]) as usize;

    let aes_cipher_len = if aes_len % 16 == 0 {
        aes_len + 16
    } else {
        (aes_len + 15) / 16 * 16
    };

    if 15 + aes_cipher_len > data.len() {
        return Err(format!("AES cipher len {} exceeds file ({} bytes)", aes_cipher_len, data.len()));
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
    let ext = detect_ext(&result[..plaintext_len.min(16)]);
    let parent = dest.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| format!("Create dir {}: {}", parent.display(), e))?;
    fs::write(dest, &result).map_err(|e| format!("Write {}: {}", dest.display(), e))?;

    Ok(ext)
}

fn aes_ecb_decrypt(data: &[u8], key: &[u8; 16]) -> Vec<u8> {
    use aes::cipher::{BlockDecrypt, KeyInit};
    use aes::Aes128;

    let cipher = Aes128::new_from_slice(key).expect("AES-128 key must be 16 bytes");
    let mut buf = data.to_vec();
    for block in buf.chunks_exact_mut(16) {
        let mut b = aes::cipher::generic_array::GenericArray::from_mut_slice(block);
        cipher.decrypt_block(&mut b);
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

/// Quick validation: decrypt the first AES block of a _t.dat file and check for image magic.
pub fn validate_key(data: &[u8], keys: &MediaKeys) -> bool {
    if data.len() < 31 {
        return false;
    }
    let encrypted = &data[15..31]; // first 16-byte AES block
    let decrypted = aes_ecb_decrypt(encrypted, &keys.aes_key);
    decrypted.len() >= 2 && decrypted[0] == 0xFF && decrypted[1] == 0xD8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_v2_dat() {
        assert!(is_v2_dat(&[0x07, 0x08, 0x56, 0x32, 0x08, 0x07]));
        assert!(!is_v2_dat(&[0x07, 0x08, 0x56, 0x31, 0x08, 0x07]));
        assert!(!is_v2_dat(&[0; 6]));
    }

    #[test]
    fn test_detect_ext() {
        assert_eq!(detect_ext(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]), "jpg");
        assert_eq!(detect_ext(&[0x89, 0x50, 0x4E, 0x47, 0x0D]), "png");
    }

    #[test]
    fn test_pkcs7_unpad() {
        assert_eq!(pkcs7_unpad(b"hello\x03\x03\x03"), b"hello");
        assert_eq!(pkcs7_unpad(b"no padding  "), b"no padding  "); // 0x20 != 16
        assert_eq!(pkcs7_unpad(b"hello\x01"), b"hello");
        assert_eq!(pkcs7_unpad(b""), b"");
    }
}
