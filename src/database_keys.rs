use std::collections::{BTreeMap, HashMap};

use pbkdf2::pbkdf2_hmac;
use sha2::Sha512;
use zeroize::Zeroize;

use crate::{decrypt, decrypt::DatabaseKeys, parallel};

const DATABASE_KDF_ROUNDS: u32 = 256_000;

fn derive_encryption_key(account_material: &[u8; 32], salt: &[u8; 16]) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha512>(account_material, salt, DATABASE_KDF_ROUNDS, &mut key);
    key
}

pub(crate) struct EncryptedDatabasePage {
    pub(crate) rel_path: String,
    pub(crate) page1: Vec<u8>,
}

pub(crate) struct DerivationOutcome {
    pub(crate) keys: DatabaseKeys,
    pub(crate) total_databases: usize,
    pub(crate) derived_databases: usize,
    pub(crate) derived_salts: usize,
    pub(crate) reused_databases: usize,
    pub(crate) missing_paths: Vec<String>,
}

impl DerivationOutcome {
    pub(crate) fn is_complete(&self) -> bool {
        self.total_databases > 0 && self.missing_paths.is_empty()
    }
}

struct SaltGroup {
    salt: [u8; 16],
    sources: Vec<EncryptedDatabasePage>,
}

fn derive_for_sources(
    account_material: &[u8; 32],
    sources: Vec<EncryptedDatabasePage>,
    existing: &DatabaseKeys,
    requested_jobs: usize,
) -> DerivationOutcome {
    let all_paths = sources
        .iter()
        .map(|source| source.rel_path.clone())
        .collect::<Vec<_>>();
    let total_databases = all_paths.len();
    let mut grouped = BTreeMap::<[u8; 16], Vec<EncryptedDatabasePage>>::new();
    let mut keys = DatabaseKeys::new();
    let mut reused_databases = 0;
    for source in sources {
        let Some(salt_bytes) = source.page1.get(..16) else {
            continue;
        };
        let mut salt = [0u8; 16];
        salt.copy_from_slice(salt_bytes);

        if let Some(mut key) = valid_existing_key(existing, &source) {
            keys.insert(
                source.rel_path,
                HashMap::from([
                    ("enc_key".to_string(), hex::encode(key)),
                    ("salt".to_string(), hex::encode(salt)),
                ]),
            );
            key.zeroize();
            reused_databases += 1;
            continue;
        }
        grouped.entry(salt).or_default().push(source);
    }

    let groups = grouped
        .into_iter()
        .map(|(salt, sources)| SaltGroup { salt, sources })
        .collect::<Vec<_>>();
    let jobs = parallel::job_count(requested_jobs, 8);
    let derived = parallel::map_ordered(groups, jobs, |group| {
        let key = derive_encryption_key(account_material, &group.salt);
        (group, key)
    });

    let mut derived_salts = 0;
    for (group, mut key) in derived {
        let key_hex = hex::encode(key);
        let salt_hex = hex::encode(group.salt);
        let mut matched = false;
        for source in group.sources {
            if !decrypt::key_bytes_match_page1(&key, &source.page1) {
                continue;
            }
            matched = true;
            keys.insert(
                source.rel_path,
                HashMap::from([
                    ("enc_key".to_string(), key_hex.clone()),
                    ("salt".to_string(), salt_hex.clone()),
                ]),
            );
        }
        derived_salts += usize::from(matched);
        key.zeroize();
    }

    let derived_databases = keys.len().saturating_sub(reused_databases);
    let missing_paths = all_paths
        .into_iter()
        .filter(|path| !keys.contains_key(path))
        .collect();
    DerivationOutcome {
        keys,
        total_databases,
        derived_databases,
        derived_salts,
        reused_databases,
        missing_paths,
    }
}

fn valid_existing_key(existing: &DatabaseKeys, source: &EncryptedDatabasePage) -> Option<[u8; 32]> {
    let key_hex = decrypt::database_key_entry(existing, &source.rel_path)?.get("enc_key")?;
    let decoded = hex::decode(key_hex).ok()?;
    let key: [u8; 32] = decoded.try_into().ok()?;
    decrypt::key_bytes_match_page1(&key, &source.page1).then_some(key)
}

pub(crate) fn derive_from_storage(
    account_material: &[u8; 32],
    db_storage: &std::path::Path,
    existing: &DatabaseKeys,
    requested_jobs: usize,
) -> Result<DerivationOutcome, String> {
    let sources = load_encrypted_database_pages(db_storage)?;
    Ok(derive_for_sources(
        account_material,
        sources,
        existing,
        requested_jobs,
    ))
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) fn validates_against_storage(
    account_material: &[u8; 32],
    db_storage: &std::path::Path,
) -> Result<bool, String> {
    for source in load_encrypted_database_pages(db_storage)? {
        let Some(salt_bytes) = source.page1.get(..16) else {
            continue;
        };
        let mut salt = [0u8; 16];
        salt.copy_from_slice(salt_bytes);
        let mut key = derive_encryption_key(account_material, &salt);
        let matches = decrypt::key_bytes_match_page1(&key, &source.page1);
        key.zeroize();
        if matches {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn load_encrypted_database_pages(
    db_storage: &std::path::Path,
) -> Result<Vec<EncryptedDatabasePage>, String> {
    let mut sources = Vec::new();
    for source in decrypt::collect_db_files(db_storage) {
        if let Some(page1) = decrypt::read_encrypted_page1(&source.full_path)? {
            sources.push(EncryptedDatabasePage {
                rel_path: source.rel_path,
                page1,
            });
        }
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_known_sqlcipher_key_from_account_material_and_database_salt() {
        let passphrase: [u8; 32] = std::array::from_fn(|index| index as u8);
        let salt: [u8; 16] = std::array::from_fn(|index| (index + 16) as u8);

        let key = derive_encryption_key(&passphrase, &salt);

        assert_eq!(
            hex::encode(key),
            "699f2ac9232d8ea2f443137cb626202ca98943df031715298614a5251ee7d973"
        );
    }

    #[test]
    fn derives_once_per_unique_salt_and_maps_every_matching_database() {
        let passphrase: [u8; 32] = std::array::from_fn(|index| index as u8);
        let salt: [u8; 16] = std::array::from_fn(|index| (index + 16) as u8);
        let stored_hmac = hex::decode(
            "69b73d4edb08e3de8aff631726fe184a47a03973c5128b8fb9abb0d111cfd78e\
             3a47f06f9aab559022c82da91ad72a3e144396e808b78783a8d5244bcc6f195d",
        )
        .unwrap();
        let mut page1 = vec![0u8; 4096];
        page1[..16].copy_from_slice(&salt);
        page1[4096 - 64..].copy_from_slice(&stored_hmac);
        let sources = vec![
            EncryptedDatabasePage {
                rel_path: "message/message_0.db".to_string(),
                page1: page1.clone(),
            },
            EncryptedDatabasePage {
                rel_path: "session/session.db".to_string(),
                page1,
            },
        ];

        let outcome = derive_for_sources(&passphrase, sources, &DatabaseKeys::new(), 4);

        assert_eq!(outcome.derived_salts, 1);
        assert_eq!(outcome.keys.len(), 2);
        for path in ["message/message_0.db", "session/session.db"] {
            assert_eq!(
                outcome.keys[path].get("enc_key").map(String::as_str),
                Some("699f2ac9232d8ea2f443137cb626202ca98943df031715298614a5251ee7d973")
            );
        }
    }

    #[test]
    fn reuses_a_valid_existing_key_without_running_database_kdf() {
        let passphrase = [0xff; 32];
        let salt: [u8; 16] = std::array::from_fn(|index| (index + 16) as u8);
        let stored_hmac = hex::decode(
            "69b73d4edb08e3de8aff631726fe184a47a03973c5128b8fb9abb0d111cfd78e\
             3a47f06f9aab559022c82da91ad72a3e144396e808b78783a8d5244bcc6f195d",
        )
        .unwrap();
        let mut page1 = vec![0u8; 4096];
        page1[..16].copy_from_slice(&salt);
        page1[4096 - 64..].copy_from_slice(&stored_hmac);
        let sources = vec![EncryptedDatabasePage {
            rel_path: "session/session.db".to_string(),
            page1,
        }];
        let mut existing = DatabaseKeys::new();
        existing.insert(
            "session/session.db".to_string(),
            HashMap::from([(
                "enc_key".to_string(),
                "699f2ac9232d8ea2f443137cb626202ca98943df031715298614a5251ee7d973".to_string(),
            )]),
        );

        let outcome = derive_for_sources(&passphrase, sources, &existing, 4);

        assert_eq!(outcome.reused_databases, 1);
        assert_eq!(outcome.derived_salts, 0);
        assert_eq!(outcome.keys.len(), 1);
    }

    #[test]
    fn derives_verified_keys_from_a_database_storage_tree() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        let salt: [u8; 16] = std::array::from_fn(|index| (index + 16) as u8);
        let stored_hmac = hex::decode(
            "69b73d4edb08e3de8aff631726fe184a47a03973c5128b8fb9abb0d111cfd78e\
             3a47f06f9aab559022c82da91ad72a3e144396e808b78783a8d5244bcc6f195d",
        )
        .unwrap();
        let mut page1 = vec![0u8; 4096];
        page1[..16].copy_from_slice(&salt);
        page1[4096 - 64..].copy_from_slice(&stored_hmac);
        std::fs::write(session_dir.join("session.db"), page1).unwrap();
        let passphrase: [u8; 32] = std::array::from_fn(|index| index as u8);

        let outcome =
            derive_from_storage(&passphrase, dir.path(), &DatabaseKeys::new(), 4).unwrap();

        assert!(outcome.is_complete());
        assert_eq!(outcome.total_databases, 1);
        assert_eq!(outcome.derived_databases, 1);
        assert!(outcome.missing_paths.is_empty());
        assert!(validates_against_storage(&passphrase, dir.path()).unwrap());
    }
}
