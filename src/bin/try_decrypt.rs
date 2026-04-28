//! V2 .dat decryptor: derives key from kvcomm and decrypts a .dat file.
//!
//! Usage:
//!   cargo run --bin try_decrypt <path_to.dat>
//!
//! Derives the AES key from Telegram kvcomm files, decrypts, and writes to /tmp/.

use std::fs;
use aes::cipher::{BlockDecrypt, KeyInit};
use aes::Aes128;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: try_decrypt <path_to.dat>");
        std::process::exit(1);
    }
    let dat_path = &args[1];
    let data = fs::read(dat_path).expect("read file");

    // Parse V2 header
    let magic = &data[..6];
    let aes_len = u32::from_le_bytes([data[6], data[7], data[8], data[9]]) as usize;
    let xor_len = u32::from_le_bytes([data[10], data[11], data[12], data[13]]) as usize;
    let flag = data[14];

    let aes_cipher_len = if aes_len % 16 == 0 { aes_len + 16 } else { (aes_len + 15) / 16 * 16 };

    println!("File: {} ({} bytes)", dat_path, data.len());
    println!("Magic: {:02x?}", magic);
    println!("AES len: {}, XOR len: {}, flag: {}, AES cipher: {}", aes_len, xor_len, flag, aes_cipher_len);
    println!("First encrypted block: {:02x?}", &data[15..31]);

    // Try to derive keys from kvcomm
    let home = std::env::var("HOME").unwrap_or_default();
    let kvcomm = format!("{}/Library/Containers/com.telegram.xinTelegram/Data/Documents/app_data/net/kvcomm", home);
    let kvcomm_path = std::path::Path::new(&kvcomm);

    if kvcomm_path.is_dir() {
        if let Ok(entries) = fs::read_dir(kvcomm_path) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(code) = extract_code(&name) {
                    // Derive key and test
                    let tgid = match find_tgid() {
                        Some(id) => id,
                        None => continue,
                    };
                    let aes_key_str = derive_key(code, &tgid);
                    let aes_key = aes_key_str.as_bytes();
                    let xor_key = (code & 0xff) as u8;

                    println!("\nTrying: code={}, tgid={}", code, tgid);
                    println!("  AES key (ASCII): {}", aes_key_str);
                    println!("  XOR key: {:#04x}", xor_key);

                    // Validate: decrypt first block
                    let cipher = Aes128::new_from_slice(aes_key).unwrap();
                    let mut block = data[15..31].to_vec();
                    cipher.decrypt_block(aes::cipher::generic_array::GenericArray::from_mut_slice(&mut block));
                    let is_jpeg = block[0] == 0xFF && block[1] == 0xD8;
                    let is_png = block[0] == 0x89 && block[1] == 0x50 && block[2] == 0x4E && block[3] == 0x47;
                    println!("  Validate: {:02x?} JPEG={} PNG={}", &block[..4], is_jpeg, is_png);

                    if is_jpeg || is_png {
                        // Full decrypt
                        let encrypted = &data[15..15 + aes_cipher_len];
                        let mut decrypted = encrypted.to_vec();
                        for chunk in decrypted.chunks_exact_mut(16) {
                            let mut b = aes::cipher::generic_array::GenericArray::from_mut_slice(chunk);
                            cipher.decrypt_block(&mut b);
                        }
                        // PKCS7 unpad
                        let pad = decrypted.last().copied().unwrap_or(0) as usize;
                        if pad > 0 && pad <= 16 {
                            decrypted.truncate(decrypted.len() - pad);
                        }

                        // Middle
                        let body_start = 15 + aes_cipher_len;
                        let xor_start = data.len() - xor_len;
                        if xor_start > body_start {
                            decrypted.extend_from_slice(&data[body_start..xor_start]);
                        }

                        // XOR tail
                        let xored = &data[xor_start..];
                        decrypted.extend(xored.iter().map(|b| b ^ xor_key));

                        // Detect extension
                        let ext = if decrypted.len() >= 4 && decrypted[..4] == [0x89, 0x50, 0x4E, 0x47] { "png" }
                            else if decrypted.len() >= 2 && decrypted[..2] == [0xFF, 0xD8] { "jpg" }
                            else { "bin" };

                        let out_name = format!("/tmp/decrypted_{}.{}", name.replace(|c: char| !c.is_alphanumeric(), "_"), ext);
                        fs::write(&out_name, &decrypted).ok();
                        println!("  Saved: {} ({} bytes, {})", out_name, decrypted.len(), ext);
                    }
                }
            }
        }
    } else {
        eprintln!("kvcomm dir not found at {}", kvcomm);
    }
}

fn extract_code(filename: &str) -> Option<u64> {
    if !filename.starts_with("key_") { return None; }
    let rest = filename.strip_prefix("key_")?;
    let end = rest.find('_')?;
    rest[..end].parse::<u64>().ok()
}

fn derive_key(code: u64, tgid: &str) -> String {
    use md5::{Md5, Digest};
    let mut hasher = Md5::new();
    hasher.update(code.to_string().as_bytes());
    hasher.update(tgid.as_bytes());
    format!("{:x}", &hasher.finalize())[..16].to_string()
}

fn find_tgid() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let base = std::path::Path::new(&home)
        .join("Library/Containers/com.telegram.xinTelegram/Data/Documents/xtelegram_files");
    let entries = fs::read_dir(base).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("tgid_") || name.starts_with("gh_") {
            if let Some(pos) = name.rfind('_') {
                let clean = &name[..pos];
                if clean.starts_with("tgid_") || clean.starts_with("gh_") {
                    return Some(clean.to_string());
                }
            }
        }
    }
    None
}
