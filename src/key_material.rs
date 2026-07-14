use std::fs;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use std::fs::OpenOptions;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use std::io::Write;
use std::io::{ErrorKind, Read};
use std::path::Path;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use std::time::{SystemTime, UNIX_EPOCH};
use zeroize::Zeroize;

const FILE_MAGIC: &[u8; 8] = b"TGKM\0\0\0\x01";
const MATERIAL_LEN: usize = 32;

pub(crate) struct AccountKeyMaterial {
    bytes: [u8; MATERIAL_LEN],
}

impl AccountKeyMaterial {
    pub(crate) fn from_bytes(bytes: [u8; MATERIAL_LEN]) -> Self {
        Self { bytes }
    }

    pub(crate) fn as_bytes(&self) -> &[u8; MATERIAL_LEN] {
        &self.bytes
    }
}

impl Drop for AccountKeyMaterial {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

pub(crate) fn load(path: &Path) -> Result<Option<AccountKeyMaterial>, String> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Cannot open key material {}: {e}", path.display())),
    };
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .map_err(|e| format!("Cannot read key material {}: {e}", path.display()))?;
    if data.len() != FILE_MAGIC.len() + MATERIAL_LEN || !data.starts_with(FILE_MAGIC) {
        let error = format!(
            "Invalid key material file {}; remove it and capture again",
            path.display()
        );
        data.zeroize();
        return Err(error);
    }

    let mut bytes = [0u8; MATERIAL_LEN];
    bytes.copy_from_slice(&data[FILE_MAGIC.len()..]);
    data.zeroize();
    Ok(Some(AccountKeyMaterial::from_bytes(bytes)))
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) fn store(path: &Path, material: &AccountKeyMaterial) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| {
        format!(
            "Cannot determine key material directory for {}",
            path.display()
        )
    })?;
    crate::paths::ensure_private_dir(parent).map_err(|e| {
        format!(
            "Cannot create key material directory {}: {e}",
            parent.display()
        )
    })?;

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp_path = parent.join(format!(
        ".key-material.{}.{}.tmp",
        std::process::id(),
        nonce
    ));
    let write_result = (|| -> Result<(), String> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp_path).map_err(|e| {
            format!(
                "Cannot create temporary key material {}: {e}",
                temp_path.display()
            )
        })?;
        file.write_all(FILE_MAGIC)
            .and_then(|()| file.write_all(material.as_bytes()))
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("Cannot write key material {}: {e}", temp_path.display()))?;
        fs::rename(&temp_path, path).map_err(|e| {
            format!(
                "Cannot install key material {} at {}: {e}",
                temp_path.display(),
                path.display()
            )
        })?;
        restrict_permissions(path).map_err(|e| {
            format!(
                "Cannot restrict key material permissions for {}: {e}",
                path.display()
            )
        })?;
        crate::paths::restore_invoking_user_ownership(path).map_err(|e| {
            format!(
                "Cannot restore key material ownership for {}: {e}",
                path.display()
            )
        })?;
        Ok(())
    })();
    if let Err(primary_error) = write_result {
        return match fs::remove_file(&temp_path) {
            Ok(()) => Err(primary_error),
            Err(cleanup_error) if cleanup_error.kind() == ErrorKind::NotFound => Err(primary_error),
            Err(cleanup_error) => Err(format!(
                "{primary_error}; failed to remove temporary key material {}: {cleanup_error}",
                temp_path.display()
            )),
        };
    }
    Ok(())
}

#[cfg(all(any(target_os = "macos", target_os = "linux", test), unix))]
fn restrict_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(all(any(target_os = "macos", target_os = "linux", test), not(unix)))]
fn restrict_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_store_round_trips_key_material() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key-material.bin");
        let material = AccountKeyMaterial::from_bytes([0x5a; 32]);

        store(&path, &material).unwrap();
        let loaded = load(&path).unwrap().unwrap();

        assert_eq!(loaded.as_bytes(), &[0x5a; 32]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn private_store_rejects_unversioned_or_truncated_material() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key-material.bin");
        std::fs::write(&path, [0x5a; 32]).unwrap();

        assert!(load(&path).is_err());
    }
}
