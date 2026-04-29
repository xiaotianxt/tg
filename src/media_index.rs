use md5::{Digest, Md5};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::parallel;

const SCHEMA_VERSION: i64 = 2;

pub(crate) struct MediaIndex {
    conn: Connection,
    base_path: String,
}

#[derive(Debug, Clone)]
struct MediaDirSpec {
    path: PathBuf,
    category: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirFingerprint {
    modified_secs: i64,
    modified_nanos: u32,
    len: u64,
}

#[derive(Debug, Clone)]
struct ScannedMediaDir {
    spec: MediaDirSpec,
    fingerprint: DirFingerprint,
    files: Vec<ScannedMediaFile>,
}

#[derive(Debug, Clone)]
struct ScannedMediaFile {
    relative_path: String,
    lower_name: String,
    lower_stem: String,
    len: u64,
    is_thumb: bool,
}

impl MediaIndex {
    pub(crate) fn load(
        base_path: &Path,
        session_tgid: &str,
        categories: &[&str],
        jobs: usize,
    ) -> Self {
        Self::load_with_cache_dir(base_path, session_tgid, categories, jobs, None)
    }

    fn load_with_cache_dir(
        base_path: &Path,
        session_tgid: &str,
        categories: &[&str],
        jobs: usize,
        cache_dir: Option<&Path>,
    ) -> Self {
        let base_path_string = base_path.to_string_lossy().to_string();
        let mut conn = open_cache_connection(base_path, cache_dir);
        if let Err(e) = init_schema(&conn) {
            log::warn!(
                "Media index cache unavailable, using in-memory index: {}",
                e
            );
            conn = Connection::open_in_memory().expect("open in-memory media index");
            init_schema(&conn).expect("init in-memory media index schema");
        }

        if let Err(e) = refresh_index(
            &mut conn,
            base_path,
            &base_path_string,
            session_tgid,
            categories,
            jobs,
        ) {
            log::warn!("Media index refresh failed: {}", e);
        }

        Self {
            conn,
            base_path: base_path_string,
        }
    }

    pub(crate) fn find(&self, category: &str, identifier: &str) -> Option<PathBuf> {
        let lower = identifier.trim().to_lowercase();
        if lower.is_empty() {
            return None;
        }

        self.query_path(
            "SELECT dirs.path, files.relative_path FROM files \
             JOIN dirs ON dirs.id = files.dir_id \
             WHERE dirs.base_path = ?1 AND dirs.category = ?2 \
               AND (files.lower_name = ?3 OR files.lower_stem = ?3) \
             ORDER BY files.is_thumb ASC, files.len DESC LIMIT 1",
            category,
            &lower,
        )
        .or_else(|| {
            let prefix = format!("{}%", escape_like(&lower));
            self.query_path(
                "SELECT dirs.path, files.relative_path FROM files \
                 JOIN dirs ON dirs.id = files.dir_id \
                 WHERE dirs.base_path = ?1 AND dirs.category = ?2 \
                   AND files.lower_name LIKE ?3 ESCAPE '\\' \
                 ORDER BY files.is_thumb ASC, files.len DESC LIMIT 1",
                category,
                &prefix,
            )
        })
        .or_else(|| {
            let contains = format!("%{}%", escape_like(&lower));
            self.query_path(
                "SELECT dirs.path, files.relative_path FROM files \
                 JOIN dirs ON dirs.id = files.dir_id \
                 WHERE dirs.base_path = ?1 AND dirs.category = ?2 \
                   AND files.lower_name LIKE ?3 ESCAPE '\\' \
                 ORDER BY files.is_thumb ASC, files.len DESC LIMIT 1",
                category,
                &contains,
            )
        })
    }

    fn query_path(&self, sql: &str, category: &str, needle: &str) -> Option<PathBuf> {
        self.conn
            .query_row(sql, params![self.base_path, category, needle], |row| {
                let dir_path: String = row.get(0)?;
                let relative_path: String = row.get(1)?;
                Ok(PathBuf::from(dir_path).join(relative_path))
            })
            .optional()
            .ok()
            .flatten()
            .filter(|path| path.is_file())
    }
}

fn refresh_index(
    conn: &mut Connection,
    base_path: &Path,
    base_path_string: &str,
    session_tgid: &str,
    categories: &[&str],
    jobs: usize,
) -> Result<(), String> {
    let dirs = discover_media_dirs(base_path, session_tgid, categories);
    let run_id = current_run_id();
    let mut reusable_dirs = Vec::new();
    let mut scan_jobs = Vec::new();

    for spec in dirs {
        let Some(fingerprint) = dir_fingerprint(&spec.path) else {
            continue;
        };

        match cached_dir_fingerprint(conn, base_path_string, &spec)? {
            Some(cached) if cached == fingerprint => {
                reusable_dirs.push(spec);
            }
            _ => scan_jobs.push((spec, fingerprint)),
        }
    }

    let scan_job_count = parallel::job_count(jobs, 8);
    let scanned_dirs = parallel::map_ordered(scan_jobs, scan_job_count, |(spec, fingerprint)| {
        scan_media_dir(spec, fingerprint)
    });

    let tx = conn
        .transaction()
        .map_err(|e| format!("Start media index transaction: {}", e))?;
    {
        let mut mark_seen = tx
            .prepare(
                "UPDATE dirs SET last_seen = ?1 \
                 WHERE base_path = ?2 AND path = ?3 AND category = ?4",
            )
            .map_err(|e| format!("Prepare media dir mark-seen: {}", e))?;
        for spec in reusable_dirs {
            mark_seen
                .execute(params![
                    run_id,
                    base_path_string,
                    spec.path.to_string_lossy(),
                    spec.category,
                ])
                .map_err(|e| format!("Mark media dir seen: {}", e))?;
        }
    }
    for scanned in scanned_dirs {
        write_scanned_dir(&tx, base_path_string, run_id, scanned)?;
    }
    for category in categories {
        tx.execute(
            "DELETE FROM dirs WHERE base_path = ?1 AND category = ?2 AND last_seen != ?3",
            params![base_path_string, category, run_id],
        )
        .map_err(|e| format!("Prune stale media dirs: {}", e))?;
    }
    tx.commit()
        .map_err(|e| format!("Commit media index transaction: {}", e))?;

    Ok(())
}

fn init_schema(conn: &Connection) -> Result<(), String> {
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| format!("Enable foreign keys: {}", e))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| format!("Enable WAL: {}", e))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| format!("Set synchronous: {}", e))?;
    let existing_version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|e| format!("Read schema version: {}", e))?;
    if existing_version != SCHEMA_VERSION {
        conn.execute_batch(
            "DROP TABLE IF EXISTS files;
             DROP TABLE IF EXISTS dirs;",
        )
        .map_err(|e| format!("Reset old media index schema: {}", e))?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS dirs (
            id INTEGER PRIMARY KEY,
            base_path TEXT NOT NULL,
            path TEXT NOT NULL,
            category TEXT NOT NULL,
            modified_secs INTEGER NOT NULL,
            modified_nanos INTEGER NOT NULL,
            len INTEGER NOT NULL,
            last_seen INTEGER NOT NULL,
            UNIQUE(base_path, path, category)
        );
        CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY,
            dir_id INTEGER NOT NULL REFERENCES dirs(id) ON DELETE CASCADE,
            relative_path TEXT NOT NULL,
            lower_name TEXT NOT NULL,
            lower_stem TEXT NOT NULL,
            len INTEGER NOT NULL,
            is_thumb INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_dirs_lookup ON dirs(base_path, path, category);
        CREATE INDEX IF NOT EXISTS idx_dirs_prune ON dirs(base_path, category, last_seen);
        CREATE INDEX IF NOT EXISTS idx_files_exact_name ON files(lower_name);
        CREATE INDEX IF NOT EXISTS idx_files_exact_stem ON files(lower_stem);
        CREATE INDEX IF NOT EXISTS idx_files_dir ON files(dir_id);",
    )
    .map_err(|e| format!("Create media index schema: {}", e))?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(|e| format!("Set schema version: {}", e))
}

fn cached_dir_fingerprint(
    conn: &Connection,
    base_path: &str,
    spec: &MediaDirSpec,
) -> Result<Option<DirFingerprint>, String> {
    conn.query_row(
        "SELECT modified_secs, modified_nanos, len FROM dirs \
         WHERE base_path = ?1 AND path = ?2 AND category = ?3",
        params![base_path, spec.path.to_string_lossy(), spec.category],
        |row| {
            Ok(DirFingerprint {
                modified_secs: row.get(0)?,
                modified_nanos: row.get::<_, i64>(1)? as u32,
                len: row.get::<_, i64>(2)? as u64,
            })
        },
    )
    .optional()
    .map_err(|e| format!("Read cached media dir fingerprint: {}", e))
}

fn write_scanned_dir(
    tx: &rusqlite::Transaction<'_>,
    base_path: &str,
    run_id: i64,
    scanned: ScannedMediaDir,
) -> Result<(), String> {
    let dir_path = scanned.spec.path.to_string_lossy().to_string();
    tx.execute(
        "INSERT INTO dirs (
            base_path, path, category, modified_secs, modified_nanos, len, last_seen
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(base_path, path, category) DO UPDATE SET
            modified_secs = excluded.modified_secs,
            modified_nanos = excluded.modified_nanos,
            len = excluded.len,
            last_seen = excluded.last_seen",
        params![
            base_path,
            dir_path,
            scanned.spec.category,
            scanned.fingerprint.modified_secs,
            i64::from(scanned.fingerprint.modified_nanos),
            scanned.fingerprint.len as i64,
            run_id,
        ],
    )
    .map_err(|e| format!("Upsert media dir: {}", e))?;

    let dir_id: i64 = tx
        .query_row(
            "SELECT id FROM dirs WHERE base_path = ?1 AND path = ?2 AND category = ?3",
            params![base_path, dir_path, scanned.spec.category],
            |row| row.get(0),
        )
        .map_err(|e| format!("Read media dir id: {}", e))?;

    tx.execute("DELETE FROM files WHERE dir_id = ?1", params![dir_id])
        .map_err(|e| format!("Replace media dir files: {}", e))?;

    let mut stmt = tx
        .prepare(
            "INSERT INTO files (
            dir_id, relative_path, lower_name, lower_stem, len, is_thumb
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .map_err(|e| format!("Prepare media file insert: {}", e))?;
    for file in scanned.files {
        stmt.execute(params![
            dir_id,
            file.relative_path,
            file.lower_name,
            file.lower_stem,
            file.len as i64,
            if file.is_thumb { 1 } else { 0 },
        ])
        .map_err(|e| format!("Insert media file: {}", e))?;
    }
    Ok(())
}

fn open_cache_connection(base_path: &Path, cache_dir: Option<&Path>) -> Connection {
    if let Some(path) = cache_file_path(base_path, cache_dir) {
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                log::warn!(
                    "Cannot create media index cache dir {}: {}",
                    parent.display(),
                    e
                );
            } else {
                match Connection::open(&path) {
                    Ok(conn) => return conn,
                    Err(e) => log::warn!("Cannot open media index cache {}: {}", path.display(), e),
                }
            }
        }
    }

    Connection::open_in_memory().expect("open in-memory media index")
}

fn cache_file_path(base_path: &Path, cache_dir: Option<&Path>) -> Option<PathBuf> {
    let cache_dir = match cache_dir {
        Some(path) => path.to_path_buf(),
        None => std::env::var("TG_CACHE_DIR")
            .map(PathBuf::from)
            .ok()
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|home| PathBuf::from(home).join("Library/Caches/tg"))
            })?,
    };

    let mut hasher = Md5::new();
    hasher.update(base_path.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    Some(cache_dir.join(format!("media-index-{:x}.sqlite3", digest)))
}

fn discover_media_dirs(
    base_path: &Path,
    session_tgid: &str,
    categories: &[&str],
) -> Vec<MediaDirSpec> {
    let mut dirs = Vec::new();

    let msg_temp = base_path.join("Message/MessageTemp").join(session_tgid);
    for category in categories {
        let legacy_dir = msg_temp.join(category);
        if legacy_dir.is_dir() {
            dirs.push(MediaDirSpec {
                path: legacy_dir,
                category: (*category).to_string(),
            });
        }
    }

    if categories.contains(&"Image") {
        let attach_dir = base_path.join("msg/attach");
        if attach_dir.is_dir() {
            discover_attach_image_dirs(&attach_dir, &mut dirs);
        }
    }

    if categories.contains(&"Video") {
        let video_dir = base_path.join("msg/video");
        if video_dir.is_dir() {
            if let Ok(months) = fs::read_dir(video_dir) {
                for month in months.flatten() {
                    let path = month.path();
                    if path.is_dir() {
                        dirs.push(MediaDirSpec {
                            path,
                            category: "Video".to_string(),
                        });
                    }
                }
            }
        }
    }

    dirs.sort_by(|a, b| a.path.cmp(&b.path).then(a.category.cmp(&b.category)));
    dirs.dedup_by(|a, b| a.path == b.path && a.category == b.category);
    dirs
}

fn discover_attach_image_dirs(attach_dir: &Path, dirs: &mut Vec<MediaDirSpec>) {
    let Ok(accounts) = fs::read_dir(attach_dir) else {
        return;
    };

    for account in accounts.flatten() {
        let account_path = account.path();
        if !account_path.is_dir() {
            continue;
        }
        let Ok(months) = fs::read_dir(account_path) else {
            continue;
        };
        for month in months.flatten() {
            let img_dir = month.path().join("Img");
            if img_dir.is_dir() {
                dirs.push(MediaDirSpec {
                    path: img_dir,
                    category: "Image".to_string(),
                });
            }
        }
    }
}

fn scan_media_dir(spec: MediaDirSpec, fingerprint: DirFingerprint) -> ScannedMediaDir {
    let mut files = Vec::new();
    scan_files(&spec.path, &spec.path, &mut files);
    ScannedMediaDir {
        spec,
        fingerprint,
        files,
    }
}

fn scan_files(root: &Path, dir: &Path, files: &mut Vec<ScannedMediaFile>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            scan_files(root, &path, files);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        let name = entry.file_name();
        let lower_name = name.to_string_lossy().to_lowercase();
        let lower_stem = Path::new(&lower_name)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("")
            .to_string();
        let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let relative_path = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        files.push(ScannedMediaFile {
            relative_path,
            is_thumb: is_thumb_name(&lower_name),
            lower_name,
            lower_stem,
            len,
        });
    }
}

fn dir_fingerprint(path: &Path) -> Option<DirFingerprint> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some(DirFingerprint {
        modified_secs: duration.as_secs() as i64,
        modified_nanos: duration.subsec_nanos(),
        len: metadata.len(),
    })
}

fn current_run_id() -> i64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    duration.as_nanos().min(i64::MAX as u128) as i64
}

fn is_thumb_name(lower_name: &str) -> bool {
    lower_name.contains(".pic_thm") || lower_name.contains("thumb") || lower_name.contains("_thumb")
}

fn escape_like(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '%' | '_' | '\\' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_and_finds_cached_image() {
        let temp = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let img_dir = temp.path().join("msg/attach/account/2026-04/Img");
        fs::create_dir_all(&img_dir).unwrap();
        fs::write(img_dir.join("abc_thumb.dat"), b"x").unwrap();
        fs::write(img_dir.join("abc.dat"), b"full-size").unwrap();

        let index = MediaIndex::load_with_cache_dir(
            temp.path(),
            "session",
            &["Image"],
            1,
            Some(cache.path()),
        );

        assert_eq!(index.find("Image", "abc").unwrap(), img_dir.join("abc.dat"));
    }

    #[test]
    fn discovers_telegram4_image_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let img_dir = temp.path().join("msg/attach/account/2026-04/Img");
        fs::create_dir_all(&img_dir).unwrap();

        let dirs = discover_media_dirs(temp.path(), "session", &["Image"]);

        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].path, img_dir);
        assert_eq!(dirs[0].category, "Image");
    }

    #[test]
    fn escapes_like_wildcards() {
        assert_eq!(escape_like(r"a%b_c\d"), r"a\%b\_c\\d");
    }
}
