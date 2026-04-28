use aes::Aes256;
use cbc::Decryptor;
use cipher::{KeyIvInit, block_padding::NoPadding, BlockDecryptMut, generic_array::GenericArray};
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac;
use sha2::Sha512;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::fs;

type Aes256CbcDec = Decryptor<Aes256>;

const PAGE_SZ: usize = 4096;
const SALT_SZ: usize = 16;
const IV_SZ: usize = 16;
const HMAC_SZ: usize = 64;
const RESERVE_SZ: usize = IV_SZ + HMAC_SZ; // 80
const KEY_SZ: usize = 32;
const SQLITE_HDR: &[u8] = b"SQLite format 3\0";

pub struct DecryptStats {
    pub success: usize,
    pub failed: usize,
    pub total: usize,
}

/// Derive the HMAC key from the encryption key and salt.
/// mac_key = PBKDF2(enc_key, salt ^ 0x3a, 2, 32)
fn derive_mac_key(enc_key: &[u8], salt: &[u8]) -> Vec<u8> {
    let mac_salt: Vec<u8> = salt.iter().map(|b| b ^ 0x3a).collect();
    let mut mac_key = vec![0u8; KEY_SZ];
    pbkdf2_hmac::<Sha512>(enc_key, &mac_salt, 2, &mut mac_key);
    mac_key
}

/// Verify and decrypt a single page.
/// Returns decrypted 4096-byte page.
fn decrypt_page(enc_key: &[u8], page_data: &[u8], pgno: u32) -> Option<Vec<u8>> {
    if page_data.len() < PAGE_SZ {
        return None;
    }

    // Extract IV from reserve area (last 80 bytes, first 16)
    let iv = &page_data[PAGE_SZ - RESERVE_SZ .. PAGE_SZ - RESERVE_SZ + IV_SZ];

    if pgno == 1 {
        // Page 1: first 16 bytes are salt, skip them
        let encrypted = &page_data[SALT_SZ .. PAGE_SZ - RESERVE_SZ];
        let mut buf = encrypted.to_vec();
        buf.resize(encrypted.len() + 16, 0); // room for padding

        let key_arr = GenericArray::from_slice(enc_key);
        let iv_arr = GenericArray::from_slice(iv);
        let decryptor = Aes256CbcDec::new(key_arr, iv_arr);
        match decryptor.decrypt_padded_mut::<NoPadding>(&mut buf) {
            Ok(decrypted) => {
                let mut page = Vec::with_capacity(PAGE_SZ);
                page.extend_from_slice(SQLITE_HDR);
                page.extend_from_slice(&decrypted[..encrypted.len()]);

                // Fill rest with zeros up to reserve
                page.resize(PAGE_SZ - RESERVE_SZ, 0);
                page.extend_from_slice(&page_data[PAGE_SZ - RESERVE_SZ..]);
                Some(page)
            }
            Err(_) => None,
        }
    } else {
        // Other pages: full encrypted content
        let encrypted = &page_data[.. PAGE_SZ - RESERVE_SZ];
        let mut buf = encrypted.to_vec();
        buf.resize(encrypted.len() + 16, 0);

        let key_arr = GenericArray::from_slice(enc_key);
        let iv_arr = GenericArray::from_slice(iv);
        let decryptor = Aes256CbcDec::new(key_arr, iv_arr);
        match decryptor.decrypt_padded_mut::<NoPadding>(&mut buf) {
            Ok(decrypted) => {
                let mut page = Vec::with_capacity(PAGE_SZ);
                page.extend_from_slice(&decrypted[..encrypted.len()]);
                page.resize(PAGE_SZ, 0);
                Some(page)
            }
            Err(_) => None,
        }
    }
}

/// Verify page 1 HMAC and return the decryption key if valid.
fn verify_and_decrypt_page1(enc_key: &[u8], page1: &[u8]) -> bool {
    if page1.len() < PAGE_SZ {
        return false;
    }

    let salt = &page1[..SALT_SZ];
    let mac_key = derive_mac_key(enc_key, salt);

    let hmac_data = &page1[SALT_SZ .. PAGE_SZ - RESERVE_SZ + IV_SZ];
    let stored_hmac = &page1[PAGE_SZ - HMAC_SZ .. PAGE_SZ];

    let Ok(mut mac) = Hmac::<Sha512>::new_from_slice(&mac_key) else {
        return false;
    };
    mac.update(hmac_data);
    mac.update(&1u32.to_le_bytes());

    mac.verify_slice(stored_hmac).is_ok()
}

/// Decrypt a single database file using the given encryption key.
fn decrypt_database(db_path: &Path, out_path: &Path, enc_key_hex: &str) -> Result<bool, String> {
    let enc_key = hex::decode(enc_key_hex)
        .map_err(|e| format!("Invalid key hex: {}", e))?;

    if enc_key.len() != KEY_SZ {
        return Err(format!("Invalid key length: {}", enc_key.len()));
    }

    let file_size = fs::metadata(db_path)
        .map_err(|e| format!("Cannot read {}: {}", db_path.display(), e))?
        .len();

    if file_size < PAGE_SZ as u64 {
        return Err(format!("File too small: {}", file_size));
    }

    // Read page 1 for HMAC verification
    let mut file = fs::File::open(db_path)
        .map_err(|e| format!("Cannot open {}: {}", db_path.display(), e))?;

    let mut page1 = vec![0u8; PAGE_SZ];
    file.read_exact(&mut page1)
        .map_err(|e| format!("Cannot read page 1: {}", e))?;

    if !verify_and_decrypt_page1(&enc_key, &page1) {
        return Err("Page 1 HMAC verification failed".to_string());
    }

    let total_pages = (file_size as usize + PAGE_SZ - 1) / PAGE_SZ;

    // Ensure output directory exists
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create output dir: {}", e))?;
    }

    let mut file = fs::File::open(db_path)
        .map_err(|e| format!("Cannot reopen {}: {}", db_path.display(), e))?;

    let mut out_file = fs::File::create(out_path)
        .map_err(|e| format!("Cannot create {}: {}", out_path.display(), e))?;

    use std::io::Write;
    let mut page_buf = vec![0u8; PAGE_SZ];

    for pgno in 1..=total_pages {
        let bytes_read = file.read(&mut page_buf)
            .map_err(|e| format!("Read error at page {}: {}", pgno, e))?;

        if bytes_read == 0 {
            break;
        }

        let page_data = if bytes_read < PAGE_SZ {
            let mut p = page_buf[..bytes_read].to_vec();
            p.resize(PAGE_SZ, 0);
            p
        } else {
            page_buf.clone()
        };

        let decrypted = decrypt_page(&enc_key, &page_data, pgno as u32)
            .ok_or_else(|| format!("Decryption failed at page {}", pgno))?;

        out_file.write_all(&decrypted)
            .map_err(|e| format!("Write error at page {}: {}", pgno, e))?;
    }

    Ok(true)
}

/// Auto-detect Telegram db_storage directory.
fn auto_detect_db_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;

    // Old path
    let old_path = PathBuf::from(&home)
        .join("Library/Containers/com.telegram.xinTelegram/Data/Documents/xtelegram_files");
    if old_path.exists() {
        // Find the first account directory with db_storage
        if let Ok(entries) = fs::read_dir(&old_path) {
            for entry in entries.flatten() {
                let db_storage = entry.path().join("db_storage");
                if db_storage.is_dir() {
                    return Some(db_storage);
                }
            }
        }
    }

    // New path (Telegram 4.0.5+)
    let new_base = PathBuf::from(&home)
        .join("Library/Containers/com.telegram.xinTelegram/Data/Library/Application Support/com.telegram.xinTelegram");
    if new_base.exists() {
        if let Ok(entries) = fs::read_dir(&new_base) {
            // Look for versioned subdirectories
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Ok(sub_entries) = fs::read_dir(&path) {
                        for sub_entry in sub_entries.flatten() {
                            let db_storage = sub_entry.path().join("db_storage");
                            if db_storage.is_dir() {
                                return Some(db_storage);
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

/// Collect all .db files in a directory tree.
fn collect_db_files(dir: &Path) -> Vec<(String, PathBuf, u64)> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return files;
    }

    fn walk(dir: &Path, base: &Path, files: &mut Vec<(String, PathBuf, u64)>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, base, files);
                } else if path.extension().and_then(|e| e.to_str()) == Some("db") {
                    // Skip WAL and SHM files
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if name.ends_with("-wal") || name.ends_with("-shm") {
                        continue;
                    }
                    if let Ok(meta) = fs::metadata(&path) {
                        let rel = path.strip_prefix(base)
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|_| name.to_string());
                        files.push((rel, path.clone(), meta.len()));
                    }
                }
            }
        }
    }

    walk(dir, dir, &mut files);
    files.sort_by_key(|(_, _, size)| *size);
    files
}

/// Decrypt all databases in the Telegram db_storage directory.
pub fn decrypt_all(
    keys_path: &Path,
    output_dir: &Path,
    db_dir: Option<&Path>,
) -> Result<DecryptStats, String> {
    // Load keys
    let keys_json = fs::read_to_string(keys_path)
        .map_err(|e| format!("Cannot read {}: {}", keys_path.display(), e))?;
    let keys: HashMap<String, HashMap<String, String>> = serde_json::from_str(&keys_json)
        .map_err(|e| format!("Invalid keys JSON: {}", e))?;

    // Determine db_storage path
    let db_storage = match db_dir {
        Some(dir) => dir.to_path_buf(),
        None => auto_detect_db_dir()
            .ok_or_else(|| "Cannot auto-detect Telegram DB directory. Use --db-dir.".to_string())?,
    };

    println!("DB storage directory: {}", db_storage.display());
    println!("Loaded {} database keys", keys.len());

    // Collect all .db files
    let db_files = collect_db_files(&db_storage);
    println!("Found {} database files\n", db_files.len());

    let mut stats = DecryptStats { success: 0, failed: 0, total: db_files.len() };

    for (rel_path, full_path, size) in &db_files {
        // Look up key for this database
        let enc_key = keys.get(rel_path.as_str())
            .or_else(|| {
                // Also try without directory prefix
                let basename = Path::new(rel_path).file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                keys.keys().find(|k| k.ends_with(basename))
                    .and_then(|k| keys.get(k))
            });

        let enc_key = match enc_key {
            Some(k) => k.get("enc_key"),
            None => {
                println!("SKIP: {} (no key)", rel_path);
                stats.failed += 1;
                continue;
            }
        };

        let out_path = output_dir.join(rel_path);
        let size_mb = *size as f64 / (1024.0 * 1024.0);
        print!("Decrypt: {} ({:.1}MB) ... ", rel_path, size_mb);
        std::io::Write::flush(&mut std::io::stdout()).ok();

        match decrypt_database(full_path, &out_path, enc_key.as_deref().map_or("", |v| v)) {
            Ok(true) => {
                // Verify with SQLite
                match verify_sqlite(&out_path) {
                    Ok(tables) => {
                        let table_list: Vec<&str> = tables.iter().take(5).map(|s| s.as_str()).collect();
                        println!("OK! Tables: {}", table_list.join(", "));
                        if tables.len() > 5 {
                            print!(" ... {} total", tables.len());
                        }
                        println!();
                        stats.success += 1;
                    }
                    Err(e) => {
                        println!("WARN: SQLite verify failed: {}", e);
                        stats.failed += 1;
                    }
                }
            }
            Ok(false) => {
                println!("FAILED (unknown error)");
                stats.failed += 1;
            }
            Err(e) => {
                println!("FAILED: {}", e);
                stats.failed += 1;
            }
        }
    }

    Ok(stats)
}

fn verify_sqlite(db_path: &Path) -> Result<Vec<String>, String> {
    let conn = rusqlite::Connection::open(db_path)
        .map_err(|e| format!("Cannot open: {}", e))?;

    let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .map_err(|e| format!("Query error: {}", e))?;

    let tables: Vec<String> = stmt.query_map([], |row| row.get(0))
        .map_err(|e| format!("Read error: {}", e))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(tables)
}
