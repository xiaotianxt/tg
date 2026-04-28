use aes::Aes256;
use cbc::Decryptor;
use cipher::{block_padding::NoPadding, generic_array::GenericArray, BlockDecryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac;
use sha2::Sha512;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::parallel;

type Aes256CbcDec = Decryptor<Aes256>;

const PAGE_SZ: usize = 4096;
const SALT_SZ: usize = 16;
const IV_SZ: usize = 16;
const HMAC_SZ: usize = 64;
const RESERVE_SZ: usize = IV_SZ + HMAC_SZ; // 80
const KEY_SZ: usize = 32;
const PAGE_FINGERPRINT_SZ: usize = 16;
const IO_BUF_SZ: usize = 1024 * 1024;
const PAGE_CACHE_MAGIC: &[u8; 8] = b"TGRPG001";
const SQLITE_HDR: &[u8] = b"SQLite format 3\0";

pub struct DecryptStats {
    pub success: usize,
    pub failed: usize,
    pub skipped: usize,
    pub total: usize,
}

struct DecryptFileStats {
    total_pages: usize,
    decrypted_pages: usize,
    reused_pages: usize,
}

struct PageCache {
    fingerprints: Vec<[u8; PAGE_FINGERPRINT_SZ]>,
}

/// Configuration for decryption behavior.
pub struct DecryptConfig {
    /// If true, only decrypt files whose source mtime is newer than the decrypted file's mtime.
    pub incremental: bool,
    /// If set, only decrypt source files modified after this Unix timestamp.
    pub since: Option<i64>,
    /// If true, suppress progress output.
    pub quiet: bool,
    /// Number of parallel database jobs. 0 means auto.
    pub jobs: usize,
}

struct DecryptTask {
    rel_path: String,
    full_path: PathBuf,
    out_path: PathBuf,
    enc_key: String,
    size: u64,
}

enum DecryptOutcome {
    Success {
        file_stats: DecryptFileStats,
        tables: Vec<String>,
    },
    VerifyFailed(String),
    Failed(String),
}

enum DecryptPlanItem {
    Skipped {
        rel_path: String,
        reason: &'static str,
        counts_as_failed: bool,
    },
    Task(usize),
}

/// Derive the HMAC key from the encryption key and salt.
/// mac_key = PBKDF2(enc_key, salt ^ 0x3a, 2, 32)
fn derive_mac_key(enc_key: &[u8], salt: &[u8]) -> Vec<u8> {
    let mac_salt: Vec<u8> = salt.iter().map(|b| b ^ 0x3a).collect();
    let mut mac_key = vec![0u8; KEY_SZ];
    pbkdf2_hmac::<Sha512>(enc_key, &mac_salt, 2, &mut mac_key);
    mac_key
}

/// Verify and decrypt a single page into `out`.
fn decrypt_page_into(enc_key: &[u8], page_data: &[u8], pgno: u32, out: &mut [u8]) -> Option<()> {
    if page_data.len() < PAGE_SZ || out.len() != PAGE_SZ {
        return None;
    }

    let iv = &page_data[PAGE_SZ - RESERVE_SZ..PAGE_SZ - RESERVE_SZ + IV_SZ];

    // Page 1 has 16-byte salt prefix; all pages encrypt up to the reserve area
    let payload_start = if pgno == 1 { SALT_SZ } else { 0 };
    let encrypted = &page_data[payload_start..PAGE_SZ - RESERVE_SZ];

    out[payload_start..PAGE_SZ - RESERVE_SZ].copy_from_slice(encrypted);

    let key_arr = GenericArray::from_slice(enc_key);
    let iv_arr = GenericArray::from_slice(iv);
    let decryptor = Aes256CbcDec::new(key_arr, iv_arr);
    decryptor
        .decrypt_padded_mut::<NoPadding>(&mut out[payload_start..PAGE_SZ - RESERVE_SZ])
        .ok()?;

    if pgno == 1 {
        out[..SQLITE_HDR.len()].copy_from_slice(SQLITE_HDR);
        out[PAGE_SZ - RESERVE_SZ..PAGE_SZ]
            .copy_from_slice(&page_data[PAGE_SZ - RESERVE_SZ..PAGE_SZ]);
    } else {
        out[PAGE_SZ - RESERVE_SZ..PAGE_SZ].fill(0);
    }

    Some(())
}

/// Verify page 1 HMAC and return the decryption key if valid.
fn verify_and_decrypt_page1(enc_key: &[u8], page1: &[u8]) -> bool {
    if page1.len() < PAGE_SZ {
        return false;
    }

    let salt = &page1[..SALT_SZ];
    let mac_key = derive_mac_key(enc_key, salt);

    let hmac_data = &page1[SALT_SZ..PAGE_SZ - RESERVE_SZ + IV_SZ];
    let stored_hmac = &page1[PAGE_SZ - HMAC_SZ..PAGE_SZ];

    let Ok(mut mac) = Hmac::<Sha512>::new_from_slice(&mac_key) else {
        return false;
    };
    mac.update(hmac_data);
    mac.update(&1u32.to_le_bytes());

    mac.verify_slice(stored_hmac).is_ok()
}

fn page_cache_path(out_path: &Path) -> PathBuf {
    let Some(file_name) = out_path.file_name() else {
        return out_path.with_extension("tgreader-pages");
    };
    let mut cache_name = file_name.to_os_string();
    cache_name.push(".tgreader-pages");
    out_path.with_file_name(cache_name)
}

fn page_fingerprint(page_data: &[u8]) -> [u8; PAGE_FINGERPRINT_SZ] {
    let mut fingerprint = [0u8; PAGE_FINGERPRINT_SZ];
    if page_data.len() >= PAGE_SZ {
        let start = PAGE_SZ - HMAC_SZ;
        fingerprint.copy_from_slice(&page_data[start..start + PAGE_FINGERPRINT_SZ]);
    }
    fingerprint
}

fn read_page_cache(path: &Path) -> Option<PageCache> {
    let data = fs::read(path).ok()?;
    let header_len = PAGE_CACHE_MAGIC.len() + 4 + 8 + 8;
    if data.len() < header_len || &data[..PAGE_CACHE_MAGIC.len()] != PAGE_CACHE_MAGIC {
        return None;
    }

    let mut offset = PAGE_CACHE_MAGIC.len();
    let page_size = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
    offset += 4;
    if page_size != PAGE_SZ as u32 {
        return None;
    }

    // Stored for future compatibility; current incremental logic tolerates growth/shrink.
    let _source_size = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
    offset += 8;

    let page_count = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?) as usize;
    offset += 8;
    if data.len() != offset + page_count * PAGE_FINGERPRINT_SZ {
        return None;
    }

    let fingerprints = data[offset..]
        .chunks_exact(PAGE_FINGERPRINT_SZ)
        .map(|chunk| chunk.try_into().ok())
        .collect::<Option<Vec<[u8; PAGE_FINGERPRINT_SZ]>>>()?;

    Some(PageCache { fingerprints })
}

fn write_page_cache(
    path: &Path,
    source_size: u64,
    fingerprints: &[[u8; PAGE_FINGERPRINT_SZ]],
) -> Result<(), String> {
    let mut data = Vec::with_capacity(
        PAGE_CACHE_MAGIC.len() + 4 + 8 + 8 + fingerprints.len() * PAGE_FINGERPRINT_SZ,
    );
    data.extend_from_slice(PAGE_CACHE_MAGIC);
    data.extend_from_slice(&(PAGE_SZ as u32).to_le_bytes());
    data.extend_from_slice(&source_size.to_le_bytes());
    data.extend_from_slice(&(fingerprints.len() as u64).to_le_bytes());
    for fingerprint in fingerprints {
        data.extend_from_slice(fingerprint);
    }
    fs::write(path, data).map_err(|e| format!("Cannot write page cache {}: {}", path.display(), e))
}

/// Decrypt a single database file using the given encryption key.
fn decrypt_database(
    db_path: &Path,
    out_path: &Path,
    enc_key_hex: &str,
    incremental: bool,
) -> Result<DecryptFileStats, String> {
    let enc_key = hex::decode(enc_key_hex).map_err(|e| format!("Invalid key hex: {}", e))?;

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
    let mut file = BufReader::with_capacity(
        IO_BUF_SZ,
        fs::File::open(db_path).map_err(|e| format!("Cannot open {}: {}", db_path.display(), e))?,
    );

    let mut page1 = vec![0u8; PAGE_SZ];
    file.read_exact(&mut page1)
        .map_err(|e| format!("Cannot read page 1: {}", e))?;

    if !verify_and_decrypt_page1(&enc_key, &page1) {
        return Err("Page 1 HMAC verification failed".to_string());
    }

    let total_pages = (file_size as usize).div_ceil(PAGE_SZ);

    // Ensure output directory exists
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Cannot create output dir: {}", e))?;
    }

    let mut page_buf = vec![0u8; PAGE_SZ];
    let mut out_buf = vec![0u8; PAGE_SZ];
    let cache_path = page_cache_path(out_path);
    let old_cache = if incremental && out_path.exists() {
        read_page_cache(&cache_path)
    } else {
        None
    };

    if let Some(old_cache) = old_cache {
        let mut out_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(out_path)
            .map_err(|e| format!("Cannot update {}: {}", out_path.display(), e))?;
        let old_out_len = fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
        out_file
            .set_len((total_pages * PAGE_SZ) as u64)
            .map_err(|e| format!("Cannot resize {}: {}", out_path.display(), e))?;

        let mut fingerprints = Vec::with_capacity(total_pages);
        let mut decrypted_pages = 0usize;
        let mut reused_pages = 0usize;

        let mut handle_page =
            |pgno: usize, page_data: &[u8], out_buf: &mut [u8]| -> Result<(), String> {
                let index = pgno - 1;
                let fingerprint = page_fingerprint(page_data);
                fingerprints.push(fingerprint);

                let page_end = (pgno * PAGE_SZ) as u64;
                let can_reuse = old_out_len >= page_end
                    && old_cache
                        .fingerprints
                        .get(index)
                        .is_some_and(|old| *old == fingerprint);

                if can_reuse {
                    reused_pages += 1;
                    return Ok(());
                }

                decrypt_page_into(&enc_key, page_data, pgno as u32, out_buf)
                    .ok_or_else(|| format!("Decryption failed at page {}", pgno))?;
                out_file
                    .seek(SeekFrom::Start((index * PAGE_SZ) as u64))
                    .map_err(|e| format!("Seek error at page {}: {}", pgno, e))?;
                out_file
                    .write_all(out_buf)
                    .map_err(|e| format!("Write error at page {}: {}", pgno, e))?;
                decrypted_pages += 1;
                Ok(())
            };

        handle_page(1, &page1, &mut out_buf)?;

        for pgno in 2..=total_pages {
            let bytes_remaining = file_size as usize - ((pgno - 1) * PAGE_SZ);
            let bytes_to_read = bytes_remaining.min(PAGE_SZ);

            file.read_exact(&mut page_buf[..bytes_to_read])
                .map_err(|e| format!("Read error at page {}: {}", pgno, e))?;
            if bytes_to_read < PAGE_SZ {
                page_buf[bytes_to_read..].fill(0);
            }

            handle_page(pgno, &page_buf, &mut out_buf)?;
        }

        if decrypted_pages == 0 {
            decrypt_page_into(&enc_key, &page1, 1, &mut out_buf)
                .ok_or_else(|| "Decryption failed at page 1".to_string())?;
            out_file
                .seek(SeekFrom::Start(0))
                .map_err(|e| format!("Seek error at page 1: {}", e))?;
            out_file
                .write_all(&out_buf)
                .map_err(|e| format!("Write error at page 1: {}", e))?;
        }

        out_file
            .flush()
            .map_err(|e| format!("Flush error: {}", e))?;
        write_page_cache(&cache_path, file_size, &fingerprints)?;

        return Ok(DecryptFileStats {
            total_pages,
            decrypted_pages,
            reused_pages,
        });
    }

    let mut out_file = BufWriter::with_capacity(
        IO_BUF_SZ,
        fs::File::create(out_path)
            .map_err(|e| format!("Cannot create {}: {}", out_path.display(), e))?,
    );
    let mut fingerprints = Vec::with_capacity(total_pages);

    decrypt_page_into(&enc_key, &page1, 1, &mut out_buf)
        .ok_or_else(|| "Decryption failed at page 1".to_string())?;
    fingerprints.push(page_fingerprint(&page1));

    out_file
        .write_all(&out_buf)
        .map_err(|e| format!("Write error at page 1: {}", e))?;

    for pgno in 2..=total_pages {
        let bytes_remaining = file_size as usize - ((pgno - 1) * PAGE_SZ);
        let bytes_to_read = bytes_remaining.min(PAGE_SZ);

        file.read_exact(&mut page_buf[..bytes_to_read])
            .map_err(|e| format!("Read error at page {}: {}", pgno, e))?;
        if bytes_to_read < PAGE_SZ {
            page_buf[bytes_to_read..].fill(0);
        }
        fingerprints.push(page_fingerprint(&page_buf));

        decrypt_page_into(&enc_key, &page_buf, pgno as u32, &mut out_buf)
            .ok_or_else(|| format!("Decryption failed at page {}", pgno))?;

        out_file
            .write_all(&out_buf)
            .map_err(|e| format!("Write error at page {}: {}", pgno, e))?;
    }
    out_file
        .flush()
        .map_err(|e| format!("Flush error: {}", e))?;
    write_page_cache(&cache_path, file_size, &fingerprints)?;

    Ok(DecryptFileStats {
        total_pages,
        decrypted_pages: total_pages,
        reused_pages: 0,
    })
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
                        let rel = path
                            .strip_prefix(base)
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
    config: &DecryptConfig,
) -> Result<DecryptStats, String> {
    // Load keys
    let keys_json = fs::read_to_string(keys_path)
        .map_err(|e| format!("Cannot read {}: {}", keys_path.display(), e))?;
    let keys: HashMap<String, HashMap<String, String>> =
        serde_json::from_str(&keys_json).map_err(|e| format!("Invalid keys JSON: {}", e))?;

    // Determine db_storage path
    let db_storage = match db_dir {
        Some(dir) => dir.to_path_buf(),
        None => auto_detect_db_dir()
            .ok_or_else(|| "Cannot auto-detect Telegram DB directory. Use --db-dir.".to_string())?,
    };

    if !config.quiet {
        log::info!("DB storage directory: {}", db_storage.display());
        log::info!("Loaded {} database keys", keys.len());
    }

    // Collect all .db files
    let db_files = collect_db_files(&db_storage);
    if !config.quiet {
        log::info!("Found {} database files", db_files.len());
    }

    let mut plan = Vec::with_capacity(db_files.len());
    let mut tasks = Vec::new();

    for (rel_path, full_path, size) in db_files {
        // Look up key for this database
        let enc_key = keys.get(rel_path.as_str()).or_else(|| {
            // Also try without directory prefix
            let basename = Path::new(&rel_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            keys.keys()
                .find(|k| k.ends_with(basename))
                .and_then(|k| keys.get(k))
        });

        let enc_key = match enc_key.and_then(|k| k.get("enc_key")) {
            Some(enc_key) => enc_key.clone(),
            None => {
                plan.push(DecryptPlanItem::Skipped {
                    rel_path,
                    reason: "no key",
                    counts_as_failed: true,
                });
                continue;
            }
        };

        let out_path = output_dir.join(&rel_path);

        // --since filter: skip source files not modified since the requested time
        if let Some(since_ts) = config.since {
            if let Ok(meta) = fs::metadata(&full_path) {
                if let Ok(mtime) = meta.modified() {
                    if let Ok(duration) = mtime.duration_since(std::time::UNIX_EPOCH) {
                        let mtime_ts = duration.as_secs() as i64;
                        if mtime_ts < since_ts {
                            plan.push(DecryptPlanItem::Skipped {
                                rel_path,
                                reason: "not modified since requested time",
                                counts_as_failed: false,
                            });
                            continue;
                        }
                    }
                }
            }
        }

        // --incremental filter: skip if decrypted file is already up to date
        if config.incremental {
            if let Ok(src_meta) = fs::metadata(&full_path) {
                if let Ok(src_mtime) = src_meta.modified() {
                    if let Ok(dec_meta) = fs::metadata(&out_path) {
                        if let Ok(dec_mtime) = dec_meta.modified() {
                            if dec_mtime >= src_mtime {
                                plan.push(DecryptPlanItem::Skipped {
                                    rel_path,
                                    reason: "up to date",
                                    counts_as_failed: false,
                                });
                                continue;
                            }
                        }
                    }
                }
            }
        }

        let task_index = tasks.len();
        tasks.push(DecryptTask {
            rel_path,
            full_path,
            out_path,
            enc_key,
            size,
        });
        plan.push(DecryptPlanItem::Task(task_index));
    }

    let jobs = parallel::job_count(config.jobs, 4);
    let completed = parallel::map_ordered(tasks, jobs, |task| {
        let outcome = match decrypt_database(
            &task.full_path,
            &task.out_path,
            &task.enc_key,
            config.incremental,
        ) {
            Ok(file_stats) => match verify_sqlite(&task.out_path) {
                Ok(tables) => DecryptOutcome::Success { file_stats, tables },
                Err(e) => DecryptOutcome::VerifyFailed(format!("SQLite verify failed: {}", e)),
            },
            Err(e) => DecryptOutcome::Failed(format!("FAILED: {}", e)),
        };
        (task, outcome)
    });

    let mut stats = DecryptStats {
        success: 0,
        failed: 0,
        skipped: 0,
        total: plan.len(),
    };

    for item in plan {
        match item {
            DecryptPlanItem::Skipped {
                rel_path,
                reason,
                counts_as_failed,
            } => {
                if !config.quiet {
                    log::info!("SKIP: {} ({})", rel_path, reason);
                }
                if counts_as_failed {
                    stats.failed += 1;
                } else {
                    stats.skipped += 1;
                }
            }
            DecryptPlanItem::Task(task_index) => {
                let (task, outcome) = &completed[task_index];
                if !config.quiet {
                    let size_mb = task.size as f64 / (1024.0 * 1024.0);
                    log::info!("Decrypt: {} ({:.1}MB)", task.rel_path, size_mb);
                }

                match outcome {
                    DecryptOutcome::Success { file_stats, tables } => {
                        if !config.quiet {
                            let table_list: Vec<&str> =
                                tables.iter().take(5).map(|s| s.as_str()).collect();
                            let mut message = if file_stats.reused_pages > 0 {
                                format!(
                                    "OK! Pages: {}/{} decrypted, {} reused. Tables: {}",
                                    file_stats.decrypted_pages,
                                    file_stats.total_pages,
                                    file_stats.reused_pages,
                                    table_list.join(", "),
                                )
                            } else {
                                format!("OK! Tables: {}", table_list.join(", "))
                            };
                            if tables.len() > 5 {
                                message.push_str(&format!(" ... {} total", tables.len()));
                            }
                            log::info!("{}", message);
                        }
                        stats.success += 1;
                    }
                    DecryptOutcome::VerifyFailed(message) => {
                        if !config.quiet {
                            log::warn!("{}", message);
                        }
                        stats.failed += 1;
                    }
                    DecryptOutcome::Failed(message) => {
                        if !config.quiet {
                            log::error!("{}", message);
                        }
                        stats.failed += 1;
                    }
                }
            }
        }
    }

    Ok(stats)
}

fn verify_sqlite(db_path: &Path) -> Result<Vec<String>, String> {
    let conn = rusqlite::Connection::open(db_path).map_err(|e| format!("Cannot open: {}", e))?;

    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .map_err(|e| format!("Query error: {}", e))?;

    let tables: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .map_err(|e| format!("Read error: {}", e))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(tables)
}
