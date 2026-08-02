use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use rusqlite::{ffi, Connection, OpenFlags, MAIN_DB};
use zeroize::{Zeroize, Zeroizing};

use crate::{database_keys, paths};

const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

pub(crate) struct SourceStore {
    databases: BTreeMap<String, SourceEntry>,
    key_recovery: String,
}

struct SourceEntry {
    path: PathBuf,
    key: Option<Zeroizing<[u8; 32]>>,
    #[cfg(test)]
    plaintext: bool,
}

impl SourceStore {
    pub(crate) fn discover(db_dir: Option<&Path>, keys_path: &Path) -> Result<Self, String> {
        let root = match db_dir {
            Some(path) if path.is_dir() => path.to_path_buf(),
            Some(path) => {
                return Err(format!(
                    "Source database directory does not exist: {}",
                    path.display()
                ))
            }
            None => {
                let home = std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
                    .ok_or_else(|| "Cannot find home directory for source discovery".to_string())?;
                paths::find_db_storage_dir(&home).ok_or_else(|| {
                    "Cannot auto-detect the source database directory; use --db-dir".to_string()
                })?
            }
        };

        let key_recovery = paths::key_recovery_command(&root, keys_path);
        let mut raw_keys = database_keys::load_key_file(keys_path)
            .map_err(|error| format!("{error}; repair with: {key_recovery}"))?
            .ok_or_else(|| {
                format!(
                    "Database keys are missing at {}; run: {key_recovery}",
                    keys_path.display(),
                )
            })?;
        let decoded = decode_keys(&mut raw_keys);
        drop(raw_keys);
        let mut keys = decoded.map_err(|error| format!("{error}; repair with: {key_recovery}"))?;

        let databases = database_keys::collect_source_databases(&root)?
            .into_iter()
            .map(|source| {
                let key = keys.remove(&source.rel_path);
                (
                    source.rel_path,
                    SourceEntry {
                        path: source.full_path,
                        key,
                        #[cfg(test)]
                        plaintext: false,
                    },
                )
            })
            .collect();
        Ok(Self {
            databases,
            key_recovery,
        })
    }

    pub(crate) fn databases(&self) -> impl Iterator<Item = &str> {
        self.databases.keys().map(String::as_str)
    }

    pub(crate) fn open(&self, relative_path: &str) -> Result<Connection, String> {
        let entry = self.databases.get(relative_path).ok_or_else(|| {
            format!("Source database is not present in this command snapshot: {relative_path}")
        })?;
        #[cfg(test)]
        if entry.plaintext {
            let conn = Connection::open_with_flags(
                &entry.path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|error| format!("Cannot open {relative_path} read-only: {error}"))?;
            conn.pragma_update(None, "query_only", true)
                .map_err(|error| format!("Cannot make {relative_path} query-only: {error}"))?;
            conn.execute_batch("BEGIN").map_err(|error| {
                format!("Cannot begin read snapshot for {relative_path}: {error}")
            })?;
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema LIMIT 1)",
                [],
                |_| Ok(()),
            )
            .map_err(|error| format!("Cannot read schema for {relative_path}: {error}"))?;
            return Ok(conn);
        }
        let key = entry.key.as_ref().ok_or_else(|| {
            format!(
                "No key for source database {relative_path}; run: {}",
                self.key_recovery
            )
        })?;
        let page1 = database_keys::read_page1(&entry.path, relative_path)?;
        if &page1[..SQLITE_HEADER.len()] == SQLITE_HEADER {
            return Err(format!("Source database is not encrypted: {relative_path}"));
        }
        let salt: [u8; 16] = page1[..16]
            .try_into()
            .expect("page 1 always contains a 16-byte salt");

        let conn = Connection::open_with_flags(
            &entry.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| format!("Cannot open {relative_path} read-only: {error}"))?;
        apply_raw_key(&conn, key, &salt, relative_path)?;
        conn.pragma_update(None, "cipher_compatibility", 4)
            .map_err(|error| format!("Cannot configure SQLCipher for {relative_path}: {error}"))?;
        conn.pragma_update(None, "query_only", true)
            .map_err(|error| format!("Cannot make {relative_path} query-only: {error}"))?;
        conn.pragma_update(None, "temp_store", "MEMORY")
            .map_err(|error| {
                format!("Cannot keep temporary data in memory for {relative_path}: {error}")
            })?;
        if !conn
            .is_readonly(MAIN_DB)
            .map_err(|error| format!("Cannot verify read-only mode for {relative_path}: {error}"))?
        {
            return Err(format!(
                "Source database did not open read-only: {relative_path}"
            ));
        }
        conn.execute_batch("BEGIN")
            .map_err(|error| format!("Cannot begin read snapshot for {relative_path}: {error}"))?;
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema LIMIT 1)",
            [],
            |_| Ok(()),
        )
        .map_err(|error| {
            format!(
                "Cannot decrypt or read schema for {relative_path}: {error}; run: {}",
                self.key_recovery
            )
        })?;
        Ok(conn)
    }

    #[cfg(test)]
    pub(crate) fn from_plaintext(root: &Path) -> Result<Self, String> {
        Ok(Self::from_plaintext_paths(
            database_keys::collect_source_databases(root)?
                .into_iter()
                .map(|source| (source.rel_path, source.full_path)),
        ))
    }

    #[cfg(test)]
    pub(crate) fn from_plaintext_paths(paths: impl IntoIterator<Item = (String, PathBuf)>) -> Self {
        let databases = paths
            .into_iter()
            .map(|(rel_path, full_path)| {
                (
                    rel_path,
                    SourceEntry {
                        path: full_path,
                        key: None,
                        plaintext: true,
                    },
                )
            })
            .collect();
        Self {
            databases,
            key_recovery: "tg keys".to_string(),
        }
    }
}

fn decode_keys(
    raw_keys: &mut database_keys::DatabaseKeys,
) -> Result<HashMap<String, Zeroizing<[u8; 32]>>, String> {
    let mut decoded = HashMap::with_capacity(raw_keys.len());
    for (path, fields) in raw_keys.iter_mut() {
        let Some(value) = fields.get_mut("enc_key") else {
            continue;
        };
        let mut key = Zeroizing::new([0u8; 32]);
        if hex::decode_to_slice(value.as_bytes(), key.as_mut()).is_err() {
            value.zeroize();
            return Err(format!("Invalid 32-byte database key for {path}"));
        }
        value.zeroize();
        decoded.insert(path.clone(), key);
    }
    Ok(decoded)
}

fn apply_raw_key(
    conn: &Connection,
    key: &[u8; 32],
    salt: &[u8; 16],
    label: &str,
) -> Result<(), String> {
    let mut keyspec = Zeroizing::new([0u8; 99]);
    keyspec[0..2].copy_from_slice(b"x'");
    hex::encode_to_slice(key, &mut keyspec[2..66])
        .expect("64-byte key hex destination has the exact size");
    hex::encode_to_slice(salt, &mut keyspec[66..98])
        .expect("32-byte salt hex destination has the exact size");
    keyspec[98] = b'\'';

    // SAFETY: `conn` owns a live SQLite handle, and `keyspec` remains readable
    // for the full call. SQLCipher copies the provided 99 bytes before return.
    let rc =
        unsafe { ffi::sqlite3_key(conn.handle(), keyspec.as_ptr().cast(), keyspec.len() as i32) };
    if rc != ffi::SQLITE_OK {
        return Err(format!(
            "Cannot configure database key for {label} (SQLite code {rc})"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn write_keys(path: &Path, relative_path: &str, key: &[u8; 32], salt: &[u8; 16]) {
        let value = serde_json::json!({
            relative_path: {
                "enc_key": hex::encode(key),
                "salt": hex::encode(salt),
            }
        });
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn create_encrypted_writer(
        root: &Path,
        relative_path: &str,
        key: &[u8; 32],
        salt: &[u8; 16],
    ) -> Connection {
        let path = root.join(relative_path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(&path).unwrap();
        apply_raw_key(&conn, key, salt, relative_path).unwrap();
        conn.pragma_update(None, "cipher_compatibility", 4).unwrap();
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))
            .unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        conn.execute_batch("CREATE TABLE messages(body TEXT); PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        conn
    }

    fn sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut value = path.as_os_str().to_os_string();
        value.push(suffix);
        PathBuf::from(value)
    }

    fn file_state(path: &Path) -> (Vec<u8>, std::time::SystemTime) {
        (
            fs::read(path).unwrap(),
            fs::metadata(path).unwrap().modified().unwrap(),
        )
    }

    #[test]
    fn opens_encrypted_database_read_only_and_rejects_writes() {
        let dir = tempfile::tempdir().unwrap();
        let key = [7u8; 32];
        let salt = [9u8; 16];
        let writer = create_encrypted_writer(dir.path(), "session/session.db", &key, &salt);
        writer
            .execute("INSERT INTO messages(body) VALUES ('committed')", [])
            .unwrap();
        let keys_path = dir.path().join("all_keys.json");
        write_keys(&keys_path, "session/session.db", &key, &salt);

        let store = SourceStore::discover(Some(dir.path()), &keys_path).unwrap();
        assert_eq!(
            store.databases().collect::<Vec<_>>(),
            ["session/session.db"]
        );
        let reader = store.open("session/session.db").unwrap();
        assert!(reader.is_readonly(MAIN_DB).unwrap());
        assert_eq!(
            reader
                .query_row("SELECT body FROM messages", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "committed"
        );
        assert!(reader
            .execute("INSERT INTO messages(body) VALUES ('forbidden')", [])
            .is_err());
    }

    #[test]
    fn live_wal_is_visible_and_snapshot_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let key = [11u8; 32];
        let salt = [13u8; 16];
        let relative_path = "message/message_0.db";
        let writer = create_encrypted_writer(dir.path(), relative_path, &key, &salt);
        writer
            .execute("INSERT INTO messages(body) VALUES ('wal-only')", [])
            .unwrap();
        let keys_path = dir.path().join("all_keys.json");
        write_keys(&keys_path, relative_path, &key, &salt);
        let store = SourceStore::discover(Some(dir.path()), &keys_path).unwrap();

        let old_reader = store.open(relative_path).unwrap();
        assert_eq!(
            old_reader
                .query_row("SELECT count(*) FROM messages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        writer
            .execute("INSERT INTO messages(body) VALUES ('later')", [])
            .unwrap();
        assert_eq!(
            old_reader
                .query_row("SELECT count(*) FROM messages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let new_reader = store.open(relative_path).unwrap();
        assert_eq!(
            new_reader
                .query_row("SELECT count(*) FROM messages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );

        let db_path = dir.path().join(relative_path);
        let wal_path = sidecar(&db_path, "-wal");
        let db_state = file_state(&db_path);
        let wal_state = file_state(&wal_path);
        drop(new_reader);
        assert_eq!(file_state(&db_path), db_state);
        assert_eq!(file_state(&wal_path), wal_state);
    }

    #[test]
    fn eight_readers_see_the_live_wal_without_writing_source_files() {
        let dir = tempfile::tempdir().unwrap();
        let key = [37u8; 32];
        let salt = [41u8; 16];
        let relative_path = "message/message_0.db";
        let writer = create_encrypted_writer(dir.path(), relative_path, &key, &salt);
        writer
            .execute("INSERT INTO messages(body) VALUES ('wal-only')", [])
            .unwrap();
        let keys_path = dir.path().join("all_keys.json");
        write_keys(&keys_path, relative_path, &key, &salt);
        let store = SourceStore::discover(Some(dir.path()), &keys_path).unwrap();
        let db_path = dir.path().join(relative_path);
        let wal_path = sidecar(&db_path, "-wal");
        let db_state = file_state(&db_path);
        let wal_state = file_state(&wal_path);

        std::thread::scope(|scope| {
            let readers = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        store
                            .open(relative_path)
                            .unwrap()
                            .query_row("SELECT count(*) FROM messages", [], |row| {
                                row.get::<_, i64>(0)
                            })
                            .unwrap()
                    })
                })
                .collect::<Vec<_>>();
            for reader in readers {
                assert_eq!(reader.join().unwrap(), 1);
            }
        });

        assert_eq!(file_state(&db_path), db_state);
        assert_eq!(file_state(&wal_path), wal_state);
    }

    #[test]
    fn encrypted_provider_runs_compatible_ideograph_fts() {
        let dir = tempfile::tempdir().unwrap();
        let key = [43u8; 32];
        let salt = [47u8; 16];
        let relative_path = "message/message_fts.db";
        let writer = create_encrypted_writer(dir.path(), relative_path, &key, &salt);
        crate::fts::register_compatible_tokenizer(&writer).unwrap();
        writer
            .execute_batch(
                "CREATE VIRTUAL TABLE search USING fts5(
                    body, tokenize = 'MMFtsTokenizer disable_pinyin'
                 );
                 INSERT INTO search(body) VALUES ('开源计划');",
            )
            .unwrap();
        let keys_path = dir.path().join("all_keys.json");
        write_keys(&keys_path, relative_path, &key, &salt);
        let store = SourceStore::discover(Some(dir.path()), &keys_path).unwrap();
        let reader = store.open(relative_path).unwrap();
        crate::fts::register_compatible_tokenizer(&reader).unwrap();

        assert_eq!(
            reader
                .query_row(
                    "SELECT count(*) FROM search WHERE search MATCH '开源计划'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_unwritable_shm_never_returns_stale_base_data() {
        let dir = tempfile::tempdir().unwrap();
        let key = [29u8; 32];
        let salt = [31u8; 16];
        let relative_path = "message/message_0.db";
        let writer = create_encrypted_writer(dir.path(), relative_path, &key, &salt);
        writer
            .execute("INSERT INTO messages(body) VALUES ('wal-only')", [])
            .unwrap();
        let keys_path = dir.path().join("all_keys.json");
        write_keys(&keys_path, relative_path, &key, &salt);
        let store = SourceStore::discover(Some(dir.path()), &keys_path).unwrap();

        let db_path = dir.path().join(relative_path);
        let mut shm_path = db_path.as_os_str().to_os_string();
        shm_path.push("-shm");
        fs::remove_file(PathBuf::from(shm_path)).unwrap();
        let message_dir = db_path.parent().unwrap();
        let original_mode = fs::metadata(message_dir).unwrap().permissions().mode();
        fs::set_permissions(message_dir, fs::Permissions::from_mode(0o500)).unwrap();

        let opened = store.open(relative_path);
        fs::set_permissions(message_dir, fs::Permissions::from_mode(original_mode)).unwrap();
        if let Ok(reader) = opened {
            assert_eq!(
                reader
                    .query_row("SELECT count(*) FROM messages", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
    }

    #[test]
    fn wrong_key_and_plaintext_database_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let key = [17u8; 32];
        let salt = [19u8; 16];
        let relative_path = "session/session.db";
        let writer = create_encrypted_writer(dir.path(), relative_path, &key, &salt);
        writer
            .execute("INSERT INTO messages VALUES ('x')", [])
            .unwrap();
        drop(writer);
        let keys_path = dir.path().join("all_keys.json");
        write_keys(&keys_path, relative_path, &[23u8; 32], &salt);
        let store = SourceStore::discover(Some(dir.path()), &keys_path).unwrap();
        let error = store.open(relative_path).err().unwrap();
        assert!(error.contains("tg keys --db-dir"));
        assert!(error.contains("--output"));

        let plain_dir = tempfile::tempdir().unwrap();
        let plain_path = plain_dir.path().join(relative_path);
        fs::create_dir_all(plain_path.parent().unwrap()).unwrap();
        Connection::open(&plain_path)
            .unwrap()
            .execute("CREATE TABLE plain(value INTEGER)", [])
            .unwrap();
        let plain_keys = plain_dir.path().join("all_keys.json");
        write_keys(&plain_keys, relative_path, &key, &salt);
        let plain_store = SourceStore::discover(Some(plain_dir.path()), &plain_keys).unwrap();
        assert!(plain_store.open(relative_path).is_err());
    }

    #[test]
    fn missing_custom_keys_report_the_matching_repair_command() {
        let dir = tempfile::tempdir().unwrap();
        let keys_path = dir.path().join("custom keys.json");
        let error = SourceStore::discover(Some(dir.path()), &keys_path)
            .err()
            .unwrap();

        assert!(error.contains(&format!("--db-dir '{}'", dir.path().display())));
        assert!(error.contains(&format!("--output '{}'", keys_path.display())));

        fs::write(&keys_path, b"not json").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&keys_path, fs::Permissions::from_mode(0o600)).unwrap();
        let error = SourceStore::discover(Some(dir.path()), &keys_path)
            .err()
            .unwrap();
        assert!(error.contains("Invalid keys file"));
        assert!(error.contains("tg keys --db-dir"));

        #[cfg(unix)]
        {
            fs::write(&keys_path, b"{}").unwrap();
            fs::set_permissions(&keys_path, fs::Permissions::from_mode(0o644)).unwrap();
            let error = SourceStore::discover(Some(dir.path()), &keys_path)
                .err()
                .unwrap();
            assert!(error.contains("must not be accessible by group or others"));
            assert!(error.contains("tg keys --db-dir"));
        }
    }

    #[test]
    fn bundled_provider_has_sqlcipher_and_fts5() {
        let conn = Connection::open_in_memory().unwrap();
        let cipher: String = conn
            .query_row("PRAGMA cipher_version", [], |row| row.get(0))
            .unwrap();
        assert!(!cipher.is_empty());
        assert_eq!(
            conn.query_row(
                "SELECT sqlite_compileoption_used('ENABLE_FTS5')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        conn.execute_batch(
            "CREATE VIRTUAL TABLE search USING fts5(body);\
             INSERT INTO search(body) VALUES ('开源计划');",
        )
        .unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM search WHERE search MATCH '开源计划'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
    }
}
