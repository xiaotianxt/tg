use aes::Aes256;
use cbc::Decryptor;
use cipher::{block_padding::NoPadding, generic_array::GenericArray, BlockDecryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac;
use sha2::Sha512;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{dictionary, parallel, paths};

type Aes256CbcDec = Decryptor<Aes256>;
pub(crate) type DatabaseKeys = HashMap<String, HashMap<String, String>>;

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
const REFRESH_LOCK_FILE: &str = ".tg-refresh.lock";
// Used only when an old lock has no usable pid; live pids are not stolen.
const REFRESH_LOCK_STALE_AFTER: Duration = Duration::from_secs(12 * 60 * 60);

pub struct DecryptStats {
    pub success: usize,
    pub failed: usize,
    pub skipped: usize,
    pub total: usize,
    pub failed_paths: Vec<String>,
}

struct DecryptFileStats {
    total_pages: usize,
    decrypted_pages: usize,
    reused_pages: usize,
}

struct DecryptFileResult {
    stats: DecryptFileStats,
    tables: Vec<String>,
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
    /// Which source databases are eligible for this run.
    pub scope: DecryptScope,
    /// If set, incremental mode skips outputs refreshed within this duration.
    pub recent_output_grace: Option<Duration>,
    /// If true, suppress progress output.
    pub quiet: bool,
    /// Number of parallel database jobs. 0 means auto.
    pub jobs: usize,
}

#[derive(Clone, Copy)]
pub enum DecryptScope {
    All,
    Messages,
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

pub(crate) struct SourceDbFile {
    pub(crate) rel_path: String,
    pub(crate) full_path: PathBuf,
    pub(crate) size: u64,
    pub(crate) freshness_mtime: Option<SystemTime>,
}

struct RefreshLock {
    path: PathBuf,
    owner: String,
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        if refresh_lock_owner(&self.path).as_deref() == Some(self.owner.as_str()) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct TempPathGuard {
    path: PathBuf,
    remove_on_drop: bool,
}

impl TempPathGuard {
    fn disarm(&mut self) {
        self.remove_on_drop = false;
    }
}

impl Drop for TempPathGuard {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn acquire_refresh_lock(output_dir: &Path) -> Result<RefreshLock, String> {
    paths::ensure_private_dir(output_dir)
        .map_err(|e| format!("Cannot create output dir {}: {}", output_dir.display(), e))?;

    let lock_path = output_dir.join(REFRESH_LOCK_FILE);

    for _ in 0..3 {
        match create_refresh_lock_file(&lock_path) {
            Ok(owner) => {
                return Ok(RefreshLock {
                    path: lock_path,
                    owner,
                });
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if !refresh_lock_is_stale(&lock_path)? {
                    return Err(format!(
                        "{} for {} (lock: {}). \
                         If no tg process is refreshing, remove the lock file.",
                        REFRESH_LOCK_BUSY_PREFIX,
                        output_dir.display(),
                        lock_path.display()
                    ));
                }

                match fs::remove_file(&lock_path) {
                    Ok(()) => continue,
                    Err(remove_err) if remove_err.kind() == ErrorKind::NotFound => continue,
                    Err(remove_err) => {
                        return Err(format!(
                            "Cannot remove stale refresh lock {}: {}",
                            lock_path.display(),
                            remove_err
                        ));
                    }
                }
            }
            Err(e) => {
                return Err(format!(
                    "Cannot acquire refresh lock {}: {}",
                    lock_path.display(),
                    e
                ));
            }
        }
    }

    Err(format!(
        "Cannot acquire refresh lock {} after removing stale lock",
        lock_path.display()
    ))
}

const REFRESH_LOCK_BUSY_PREFIX: &str = "Decrypted cache refresh is already running";

pub(crate) fn is_refresh_lock_busy_error(error: &str) -> bool {
    error.contains(REFRESH_LOCK_BUSY_PREFIX)
}

fn create_refresh_lock_file(lock_path: &Path) -> std::io::Result<String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock_path)?;
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let owner = format!("{}-{}-{}", std::process::id(), created, nonce);
    let write_result = (|| -> std::io::Result<()> {
        writeln!(file, "pid={}", std::process::id())?;
        writeln!(file, "created_unix={}", created)?;
        writeln!(file, "owner={}", owner)?;
        file.flush()
    })();
    if let Err(e) = write_result {
        let _ = fs::remove_file(lock_path);
        return Err(e);
    }
    Ok(owner)
}

fn refresh_lock_is_stale(lock_path: &Path) -> Result<bool, String> {
    if let Some(pid) = refresh_lock_pid(lock_path) {
        match process_is_running(pid) {
            Some(false) => return Ok(true),
            Some(true) => return Ok(false),
            None => {}
        }
    }

    let meta = fs::metadata(lock_path)
        .map_err(|e| format!("Cannot inspect refresh lock {}: {}", lock_path.display(), e))?;
    let modified = meta.modified().map_err(|e| {
        format!(
            "Cannot read refresh lock mtime {}: {}",
            lock_path.display(),
            e
        )
    })?;
    Ok(match SystemTime::now().duration_since(modified) {
        Ok(age) => age >= REFRESH_LOCK_STALE_AFTER,
        Err(_) => false,
    })
}

fn refresh_lock_pid(lock_path: &Path) -> Option<u32> {
    refresh_lock_value(lock_path, "pid").and_then(|value| value.parse().ok())
}

fn refresh_lock_owner(lock_path: &Path) -> Option<String> {
    refresh_lock_value(lock_path, "owner")
}

fn refresh_lock_value(lock_path: &Path, key: &str) -> Option<String> {
    let content = fs::read_to_string(lock_path).ok()?;
    let prefix = format!("{}=", key);
    content.lines().find_map(|line| {
        line.strip_prefix(&prefix)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> Option<bool> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Some(false);
    }

    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    let rc = unsafe { kill(pid as i32, 0) };
    if rc == 0 {
        return Some(true);
    }

    match std::io::Error::last_os_error().raw_os_error() {
        Some(1) => Some(true),
        Some(3) => Some(false),
        _ => None,
    }
}

#[cfg(not(unix))]
fn process_is_running(_pid: u32) -> Option<bool> {
    None
}

fn create_temp_file_for(dest: &Path) -> Result<(PathBuf, fs::File, TempPathGuard), String> {
    let parent = dest
        .parent()
        .ok_or_else(|| format!("Cannot determine parent dir for {}", dest.display()))?;
    paths::ensure_private_dir(parent)
        .map_err(|e| format!("Cannot create output dir {}: {}", parent.display(), e))?;

    let file_name = dest
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "database".into());
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    for attempt in 0..100 {
        let tmp_path = parent.join(format!(
            ".{}.{}.{}.{}.tmp",
            file_name,
            std::process::id(),
            nonce,
            attempt
        ));
        match OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => {
                return Ok((
                    tmp_path.clone(),
                    file,
                    TempPathGuard {
                        path: tmp_path,
                        remove_on_drop: true,
                    },
                ));
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(format!(
                    "Cannot create temp output for {}: {}",
                    dest.display(),
                    e
                ));
            }
        }
    }

    Err(format!(
        "Cannot create unique temp output for {}",
        dest.display()
    ))
}

fn finish_temp_writer(writer: BufWriter<fs::File>, tmp_path: &Path) -> Result<(), String> {
    let file = writer
        .into_inner()
        .map_err(|e| format!("Flush error for {}: {}", tmp_path.display(), e.into_error()))?;
    file.sync_all()
        .map_err(|e| format!("Sync error for {}: {}", tmp_path.display(), e))
}

fn replace_with_temp(tmp_path: &Path, dest: &Path, mut guard: TempPathGuard) -> Result<(), String> {
    fs::rename(tmp_path, dest).map_err(|e| {
        format!(
            "Cannot replace {} with temp output {}: {}",
            dest.display(),
            tmp_path.display(),
            e
        )
    })?;
    guard.disarm();
    Ok(())
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
        return out_path.with_extension("tg-pages");
    };
    let mut cache_name = file_name.to_os_string();
    cache_name.push(".tg-pages");
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

    // Reserved metadata; current incremental logic tolerates growth and shrink.
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
    write_bytes_atomic(path, &data)
        .map_err(|e| format!("Cannot write page cache {}: {}", path.display(), e))
}

fn write_bytes_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    let (tmp_path, mut file, guard) = create_temp_file_for(path)?;
    file.write_all(data)
        .map_err(|e| format!("Write error for {}: {}", tmp_path.display(), e))?;
    file.flush()
        .map_err(|e| format!("Flush error for {}: {}", tmp_path.display(), e))?;
    file.sync_all()
        .map_err(|e| format!("Sync error for {}: {}", tmp_path.display(), e))?;
    drop(file);
    replace_with_temp(&tmp_path, path, guard)
}

struct ReuseSource {
    cache: PageCache,
    file: BufReader<fs::File>,
    len: u64,
    next_index: usize,
}

impl ReuseSource {
    fn open(out_path: &Path, cache_path: &Path) -> Option<Self> {
        let cache = read_page_cache(cache_path)?;
        let file = fs::File::open(out_path).ok()?;
        let len = file.metadata().ok()?.len();
        Some(Self {
            cache,
            file: BufReader::with_capacity(IO_BUF_SZ, file),
            len,
            next_index: 0,
        })
    }

    fn read_page_if_fresh(
        &mut self,
        index: usize,
        fingerprint: [u8; PAGE_FINGERPRINT_SZ],
        out: &mut [u8],
    ) -> bool {
        let page_end = ((index + 1) * PAGE_SZ) as u64;
        let cache_matches = self
            .cache
            .fingerprints
            .get(index)
            .is_some_and(|old| *old == fingerprint);
        if self.len < page_end || !cache_matches {
            return false;
        }

        if self.next_index != index
            && self
                .file
                .seek(SeekFrom::Start((index * PAGE_SZ) as u64))
                .is_err()
        {
            return false;
        }
        if self.file.read_exact(out).is_ok() {
            self.next_index = index + 1;
            true
        } else {
            false
        }
    }
}

/// Decrypt a single database file using the given encryption key.
fn decrypt_database(
    db_path: &Path,
    out_path: &Path,
    enc_key_hex: &str,
    incremental: bool,
) -> Result<DecryptFileResult, String> {
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
    if incremental {
        if let Some(result) =
            decrypt_database_in_place(db_path, out_path, &enc_key, file_size, total_pages)?
        {
            return Ok(result);
        }
    }

    // Ensure output directory exists
    if let Some(parent) = out_path.parent() {
        paths::ensure_private_dir(parent)
            .map_err(|e| format!("Cannot create output dir: {}", e))?;
    }

    let (tmp_path, tmp_file, temp_guard) = create_temp_file_for(out_path)?;
    let mut out_file = BufWriter::with_capacity(IO_BUF_SZ, tmp_file);
    let mut fingerprints = Vec::with_capacity(total_pages);
    let mut page_buf = vec![0u8; PAGE_SZ];
    let mut out_buf = vec![0u8; PAGE_SZ];
    let mut decrypted_pages = 0usize;
    let mut reused_pages = 0usize;
    let cache_path = page_cache_path(out_path);
    let mut reuse_source = if incremental && out_path.exists() {
        ReuseSource::open(out_path, &cache_path)
    } else {
        None
    };

    {
        let mut write_page =
            |pgno: usize, page_data: &[u8], out_buf: &mut [u8]| -> Result<(), String> {
                let index = pgno - 1;
                let fingerprint = page_fingerprint(page_data);
                fingerprints.push(fingerprint);

                if let Some(reuse) = reuse_source.as_mut() {
                    if reuse.read_page_if_fresh(index, fingerprint, out_buf) {
                        out_file
                            .write_all(out_buf)
                            .map_err(|e| format!("Write error at page {}: {}", pgno, e))?;
                        reused_pages += 1;
                        return Ok(());
                    }
                }

                decrypt_page_into(&enc_key, page_data, pgno as u32, out_buf)
                    .ok_or_else(|| format!("Decryption failed at page {}", pgno))?;

                out_file
                    .write_all(out_buf)
                    .map_err(|e| format!("Write error at page {}: {}", pgno, e))?;
                decrypted_pages += 1;
                Ok(())
            };

        write_page(1, &page1, &mut out_buf)?;

        for pgno in 2..=total_pages {
            let bytes_remaining = file_size as usize - ((pgno - 1) * PAGE_SZ);
            let bytes_to_read = bytes_remaining.min(PAGE_SZ);

            file.read_exact(&mut page_buf[..bytes_to_read])
                .map_err(|e| format!("Read error at page {}: {}", pgno, e))?;
            if bytes_to_read < PAGE_SZ {
                page_buf[bytes_to_read..].fill(0);
            }

            write_page(pgno, &page_buf, &mut out_buf)?;
        }
    }

    finish_temp_writer(out_file, &tmp_path)?;
    let tables = verify_sqlite(&tmp_path).map_err(|e| format!("SQLite verify failed: {}", e))?;
    replace_with_temp(&tmp_path, out_path, temp_guard)?;
    write_page_cache(&cache_path, file_size, &fingerprints)?;

    Ok(DecryptFileResult {
        stats: DecryptFileStats {
            total_pages,
            decrypted_pages,
            reused_pages,
        },
        tables,
    })
}

fn decrypt_database_in_place(
    db_path: &Path,
    out_path: &Path,
    enc_key: &[u8],
    file_size: u64,
    total_pages: usize,
) -> Result<Option<DecryptFileResult>, String> {
    let cache_path = page_cache_path(out_path);
    let Some(cache) = read_page_cache(&cache_path) else {
        return Ok(None);
    };
    if cache.fingerprints.is_empty() || total_pages < cache.fingerprints.len() {
        return Ok(None);
    }

    let out_meta = match fs::metadata(out_path) {
        Ok(meta) => meta,
        Err(_) => return Ok(None),
    };
    let reusable_pages = cache.fingerprints.len().min(total_pages);
    if out_meta.len() < (reusable_pages * PAGE_SZ) as u64 {
        return Ok(None);
    }

    let mut source = BufReader::with_capacity(
        IO_BUF_SZ,
        fs::File::open(db_path).map_err(|e| format!("Cannot open {}: {}", db_path.display(), e))?,
    );
    let mut output = OpenOptions::new()
        .read(true)
        .write(true)
        .open(out_path)
        .map_err(|e| format!("Cannot open output {}: {}", out_path.display(), e))?;

    let output_len = (total_pages * PAGE_SZ) as u64;
    if out_meta.len() != output_len {
        output
            .set_len(output_len)
            .map_err(|e| format!("Cannot resize output {}: {}", out_path.display(), e))?;
    }

    let mut fingerprints = Vec::with_capacity(total_pages);
    let mut page_buf = vec![0u8; PAGE_SZ];
    let mut out_buf = vec![0u8; PAGE_SZ];
    let mut decrypted_pages = 0usize;
    let mut reused_pages = 0usize;

    for pgno in 1..=total_pages {
        let bytes_remaining = file_size as usize - ((pgno - 1) * PAGE_SZ);
        let bytes_to_read = bytes_remaining.min(PAGE_SZ);
        source
            .read_exact(&mut page_buf[..bytes_to_read])
            .map_err(|e| format!("Read error at page {}: {}", pgno, e))?;
        if bytes_to_read < PAGE_SZ {
            page_buf[bytes_to_read..].fill(0);
        }

        let index = pgno - 1;
        let fingerprint = page_fingerprint(&page_buf);
        fingerprints.push(fingerprint);
        if cache
            .fingerprints
            .get(index)
            .is_some_and(|old| *old == fingerprint)
        {
            reused_pages += 1;
            continue;
        }

        decrypt_page_into(enc_key, &page_buf, pgno as u32, &mut out_buf)
            .ok_or_else(|| format!("Decryption failed at page {}", pgno))?;
        output
            .seek(SeekFrom::Start((index * PAGE_SZ) as u64))
            .map_err(|e| format!("Seek error at page {}: {}", pgno, e))?;
        output
            .write_all(&out_buf)
            .map_err(|e| format!("Write error at page {}: {}", pgno, e))?;
        decrypted_pages += 1;
    }

    output
        .sync_all()
        .map_err(|e| format!("Sync error for {}: {}", out_path.display(), e))?;
    drop(output);

    let tables = verify_sqlite(out_path).map_err(|e| format!("SQLite verify failed: {}", e))?;
    write_page_cache(&cache_path, file_size, &fingerprints)?;

    Ok(Some(DecryptFileResult {
        stats: DecryptFileStats {
            total_pages,
            decrypted_pages,
            reused_pages,
        },
        tables,
    }))
}

/// Auto-detect Telegram db_storage directory.
pub(crate) fn auto_detect_db_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let home = PathBuf::from(home);

    let old_path = dictionary::documents_account_files_dir(&home);
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

    let new_base = dictionary::app_support_dir(&home);
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

pub(crate) fn sqlite_sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut path = db_path.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn source_freshness_mtime(_db_path: &Path, db_meta: &fs::Metadata) -> Option<SystemTime> {
    // The decryptor reads the base .db file only. Treating hot WAL/SHM mtimes as
    // cache freshness forces expensive no-op decrypts without making WAL content
    // visible. Doctor reports hot WAL files separately.
    db_meta.modified().ok()
}

fn path_in_decrypt_scope(scope: DecryptScope, rel_path: &str) -> bool {
    match scope {
        DecryptScope::All => true,
        DecryptScope::Messages => {
            rel_path == "contact/contact.db" || is_numbered_message_rel_path(rel_path)
        }
    }
}

fn is_numbered_message_rel_path(rel_path: &str) -> bool {
    let Some(stem) = rel_path
        .strip_prefix("message/message_")
        .and_then(|value| value.strip_suffix(".db"))
    else {
        return false;
    };

    !stem.is_empty() && stem.chars().all(|ch| ch.is_ascii_digit())
}

/// Collect all .db files in a directory tree.
pub(crate) fn collect_db_files(dir: &Path) -> Vec<SourceDbFile> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return files;
    }

    fn walk(dir: &Path, base: &Path, files: &mut Vec<SourceDbFile>) {
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
                        files.push(SourceDbFile {
                            rel_path: rel,
                            full_path: path.clone(),
                            size: meta.len(),
                            freshness_mtime: source_freshness_mtime(&path, &meta),
                        });
                    }
                }
            }
        }
    }

    walk(dir, dir, &mut files);
    files.sort_by_key(|file| file.size);
    files
}

pub(crate) fn database_key_entry<'a>(
    keys: &'a DatabaseKeys,
    rel_path: &str,
) -> Option<&'a HashMap<String, String>> {
    keys.get(rel_path).or_else(|| {
        let basename = Path::new(rel_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        keys.keys()
            .find(|key_path| key_path.ends_with(basename))
            .and_then(|key_path| keys.get(key_path))
    })
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
    let keys: DatabaseKeys =
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

    let _refresh_lock = acquire_refresh_lock(output_dir)?;

    // Collect all .db files
    let db_files = collect_db_files(&db_storage);
    if !config.quiet {
        log::info!("Found {} database files", db_files.len());
    }

    let mut plan = Vec::with_capacity(db_files.len());
    let mut tasks = Vec::new();
    let now = SystemTime::now();

    for source in db_files {
        let SourceDbFile {
            rel_path,
            full_path,
            size,
            freshness_mtime,
        } = source;

        if !path_in_decrypt_scope(config.scope, &rel_path) {
            continue;
        }

        // Look up key for this database
        let enc_key = match database_key_entry(&keys, &rel_path).and_then(|k| k.get("enc_key")) {
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
            if let Some(mtime) = freshness_mtime {
                if let Ok(duration) = mtime.duration_since(UNIX_EPOCH) {
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

        // --incremental filter: skip if decrypted file is already up to date
        if config.incremental {
            if let Some(src_mtime) = freshness_mtime {
                if let Ok(dec_meta) = fs::metadata(&out_path) {
                    if let Ok(dec_mtime) = dec_meta.modified() {
                        if config.recent_output_grace.is_some_and(|grace| {
                            now.duration_since(dec_mtime).is_ok_and(|age| age < grace)
                        }) {
                            plan.push(DecryptPlanItem::Skipped {
                                rel_path,
                                reason: "recently refreshed",
                                counts_as_failed: false,
                            });
                            continue;
                        }
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
            Ok(result) => DecryptOutcome::Success {
                file_stats: result.stats,
                tables: result.tables,
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
        failed_paths: Vec::new(),
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
                    stats.failed_paths.push(rel_path);
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
                    DecryptOutcome::Failed(message) => {
                        if !config.quiet {
                            log::error!("{}", message);
                        }
                        stats.failed += 1;
                        stats.failed_paths.push(task.rel_path.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_lock_blocks_second_owner_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let lock = acquire_refresh_lock(dir.path()).unwrap();

        let second = acquire_refresh_lock(dir.path());
        assert!(second.is_err());
        assert!(second.err().unwrap().contains("already running"));

        drop(lock);
        assert!(acquire_refresh_lock(dir.path()).is_ok());
    }

    #[test]
    fn refresh_lock_reclaims_dead_pid_owner() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(REFRESH_LOCK_FILE);
        let dead_pid = (900_000..1_000_000)
            .find(|pid| process_is_running(*pid) == Some(false))
            .expect("expected an unused pid in high test range");
        fs::write(
            &lock_path,
            format!("pid={}\ncreated_unix=1\nowner=dead-owner\n", dead_pid),
        )
        .unwrap();

        let lock = acquire_refresh_lock(dir.path()).unwrap();
        assert_ne!(lock.owner, "dead-owner");
        assert_eq!(refresh_lock_pid(&lock_path), Some(std::process::id()));
    }

    #[test]
    fn refresh_lock_drop_does_not_remove_another_owner() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(REFRESH_LOCK_FILE);
        let lock = acquire_refresh_lock(dir.path()).unwrap();

        fs::write(
            &lock_path,
            format!(
                "pid={}\ncreated_unix=1\nowner=other-owner\n",
                std::process::id()
            ),
        )
        .unwrap();
        drop(lock);

        assert!(lock_path.exists());
        assert_eq!(
            refresh_lock_owner(&lock_path).as_deref(),
            Some("other-owner")
        );
    }

    #[test]
    fn collect_db_files_ignores_wal_and_shm_mtime_for_freshness() {
        let dir = tempfile::tempdir().unwrap();
        let message_dir = dir.path().join("message");
        fs::create_dir_all(&message_dir).unwrap();
        let db_path = message_dir.join("message_0.db");
        let wal_path = sqlite_sidecar_path(&db_path, "-wal");
        let shm_path = sqlite_sidecar_path(&db_path, "-shm");
        fs::write(&db_path, b"db").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&wal_path, b"wal").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&shm_path, b"shm").unwrap();

        let db_mtime = fs::metadata(&db_path).unwrap().modified().unwrap();
        let files = collect_db_files(dir.path());
        let source = files
            .iter()
            .find(|file| file.rel_path == "message/message_0.db")
            .unwrap();
        let freshness = source.freshness_mtime.unwrap();

        assert_eq!(freshness, db_mtime);
    }

    #[test]
    fn message_decrypt_scope_includes_only_contact_and_numbered_message_dbs() {
        assert!(path_in_decrypt_scope(
            DecryptScope::Messages,
            "contact/contact.db"
        ));
        assert!(path_in_decrypt_scope(
            DecryptScope::Messages,
            "message/message_0.db"
        ));
        assert!(path_in_decrypt_scope(
            DecryptScope::Messages,
            "message/message_42.db"
        ));
        assert!(!path_in_decrypt_scope(
            DecryptScope::Messages,
            "message/message_fts.db"
        ));
        assert!(!path_in_decrypt_scope(
            DecryptScope::Messages,
            "message/message_resource.db"
        ));
        assert!(!path_in_decrypt_scope(
            DecryptScope::Messages,
            "session/session.db"
        ));
    }

    #[test]
    fn database_key_entry_matches_exact_path_or_basename() {
        let mut keys = DatabaseKeys::new();
        let mut exact = HashMap::new();
        exact.insert("enc_key".to_string(), "exact".to_string());
        keys.insert("message/message_0.db".to_string(), exact);
        let mut basename = HashMap::new();
        basename.insert("enc_key".to_string(), "basename".to_string());
        keys.insert("other/message_1.db".to_string(), basename);

        assert_eq!(
            database_key_entry(&keys, "message/message_0.db")
                .unwrap()
                .get("enc_key"),
            Some(&"exact".to_string())
        );
        assert_eq!(
            database_key_entry(&keys, "message/message_1.db")
                .unwrap()
                .get("enc_key"),
            Some(&"basename".to_string())
        );
    }
}
