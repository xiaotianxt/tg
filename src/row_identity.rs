use md5::{Digest, Md5};
use std::fmt;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SourceFingerprint {
    pub(crate) mtime_ns: i64,
    pub(crate) size: i64,
}

impl SourceFingerprint {
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        let mtime_ns = meta
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos()
            .try_into()
            .ok()?;
        Some(Self {
            mtime_ns,
            size: meta.len().try_into().ok()?,
        })
    }

    pub(crate) fn shrank_from(self, previous: Self) -> bool {
        self.size < previous.size
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ContactFingerprint(String);

impl fmt::Display for ContactFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ContactFingerprintEntry<'a> {
    pub(crate) account: &'a str,
    pub(crate) nick_name: &'a str,
    pub(crate) remark: &'a str,
    pub(crate) alias: &'a str,
    pub(crate) is_stranger: bool,
}

pub(crate) fn contact_fingerprint(
    entries: &mut [ContactFingerprintEntry<'_>],
) -> ContactFingerprint {
    if entries.is_empty() {
        return ContactFingerprint(String::new());
    }

    entries.sort_by(|left, right| left.account.cmp(right.account));

    let mut hasher = Md5::new();
    for entry in entries {
        hash_text(&mut hasher, entry.account);
        hash_text(&mut hasher, entry.nick_name);
        hash_text(&mut hasher, entry.remark);
        hash_text(&mut hasher, entry.alias);
        hasher.update([u8::from(entry.is_stranger)]);
    }
    ContactFingerprint(format!("{:x}", hasher.finalize()))
}

fn hash_text(hasher: &mut Md5, value: &str) {
    hasher.update(value.len().to_le_bytes());
    hasher.update(value.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contact_fingerprint_is_order_independent() {
        let mut left = vec![
            ContactFingerprintEntry {
                account: "b",
                nick_name: "Bee",
                remark: "",
                alias: "",
                is_stranger: false,
            },
            ContactFingerprintEntry {
                account: "a",
                nick_name: "Aye",
                remark: "",
                alias: "",
                is_stranger: true,
            },
        ];
        let mut right = vec![left[1], left[0]];

        assert_eq!(
            contact_fingerprint(&mut left),
            contact_fingerprint(&mut right)
        );
    }

    #[test]
    fn contact_fingerprint_changes_on_display_fields() {
        let mut original = vec![ContactFingerprintEntry {
            account: "a",
            nick_name: "Aye",
            remark: "",
            alias: "",
            is_stranger: false,
        }];
        let mut changed = vec![ContactFingerprintEntry {
            account: "a",
            nick_name: "Aye",
            remark: "Remark",
            alias: "",
            is_stranger: false,
        }];

        assert_ne!(
            contact_fingerprint(&mut original),
            contact_fingerprint(&mut changed)
        );
    }

    #[test]
    fn source_fingerprint_detects_shrink() {
        let previous = SourceFingerprint {
            mtime_ns: 10,
            size: 100,
        };
        let current = SourceFingerprint {
            mtime_ns: 11,
            size: 90,
        };

        assert!(current.shrank_from(previous));
    }
}
