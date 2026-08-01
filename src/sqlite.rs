use rusqlite::{Connection, OpenFlags, MAIN_DB};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub(crate) fn open_immutable(path: &Path) -> Result<Connection, String> {
    let uri = format!("file:{}?immutable=1", uri_path(path));
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("Cannot open {} read-only: {}", path.display(), e))?;
    if !conn
        .is_readonly(MAIN_DB)
        .map_err(|e| format!("Cannot verify read-only mode: {}", e))?
    {
        return Err(format!("{} did not open read-only", path.display()));
    }
    Ok(conn)
}

fn uri_path(path: &Path) -> String {
    let mut encoded = String::new();
    for byte in path.as_os_str().as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' => {
                encoded.push(*byte as char)
            }
            _ => encoded.push_str(&format!("%{:02X}", byte)),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immutable_open_does_not_create_sqlite_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test db.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE test (value INTEGER)", [])
            .unwrap();
        conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0))
            .unwrap();
        drop(conn);
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(crate::decrypt::sqlite_sidecar_path(&path, suffix));
        }

        let conn = open_immutable(&path).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM test", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(conn);

        assert!(!crate::decrypt::sqlite_sidecar_path(&path, "-wal").exists());
        assert!(!crate::decrypt::sqlite_sidecar_path(&path, "-shm").exists());
    }
}
