//! Load one cached image or video cover by its cache id, for viewers that ask
//! for media one item at a time instead of exporting a whole chat.
//!
//! The id is the 32-hex cache name that message rendering already shows
//! (`[img:<id>]`, or the name in `[视频 WxH <id>]`). Images live under the chat's
//! own attach directory; video covers live in the shared video directory.

use std::cell::OnceCell;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};

use crate::media;
use crate::media_decrypt;
use crate::media_key::{self, MediaKeys};
use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaSize {
    /// The client's standard-size copy, falling back to the thumbnail.
    Preview,
    /// The full-resolution copy when it was downloaded.
    Original,
}

pub(crate) struct CachedImage {
    pub mime: &'static str,
    /// File extension matching `mime`.
    pub ext: &'static str,
    pub data: Vec<u8>,
    /// Which cached copy was used, e.g. `standard` or `thumbnail`.
    pub variant: &'static str,
}

/// Cache ids are 32 lowercase hex characters; anything else is rejected before
/// it can reach a path.
pub(crate) fn is_media_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The id in a bare id or in the whole `[img:<id>]` tag, which people and
/// models often copy as is.
pub(crate) fn parse_media_id(raw: &str) -> Result<&str, String> {
    let id = raw.trim().trim_start_matches("[img:").trim_end_matches(']');
    if is_media_id(id) {
        Ok(id)
    } else {
        Err(format!(
            "'{raw}' is not a media id; pass the 32-character id from [img:<id>]"
        ))
    }
}

/// Where `tg media` writes decrypted copies unless told otherwise. The system
/// temp directory is per user on macOS and is cleaned up by the OS.
pub(crate) fn default_output_dir() -> PathBuf {
    std::env::temp_dir().join("tg-media")
}

/// Write `image` to `<dir>/<id>.<ext>`, readable only by the owner, replacing
/// an earlier copy of the same id.
pub(crate) fn write_private(dir: &Path, id: &str, image: &CachedImage) -> Result<PathBuf, String> {
    paths::ensure_private_dir(dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let path = dir.join(format!("{id}.{}", image.ext));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(&path)
        .and_then(|mut file| file.write_all(&image.data))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(path)
}

pub(crate) struct MediaCache {
    base: PathBuf,
    /// `msg/attach/<md5(chat)>/<YYYY-MM>/Img`, newest month first.
    image_dirs: Vec<PathBuf>,
    /// `msg/video/<YYYY-MM>`, newest month first.
    video_dirs: Vec<PathBuf>,
    keys: OnceCell<Result<MediaKeys, String>>,
}

impl MediaCache {
    /// Open the media cache for one chat. With `min_month` (`YYYY-MM`), month
    /// directories before it are invisible.
    pub(crate) fn open(username: &str, min_month: Option<&str>) -> Result<Self, String> {
        let base = media::find_telegram_base_path()
            .ok_or("Local media cache not found; images are only available on this computer")?;
        Ok(Self::at(base, username, min_month))
    }

    fn at(base: PathBuf, username: &str, min_month: Option<&str>) -> Self {
        let chat_dir = base
            .join("msg/attach")
            .join(hex::encode(Md5::digest(username.as_bytes())));
        let image_dirs = month_dirs(&chat_dir, min_month)
            .into_iter()
            .map(|dir| dir.join("Img"))
            .collect();
        let video_dirs = month_dirs(&base.join("msg/video"), min_month);
        Self {
            base,
            image_dirs,
            video_dirs,
            keys: OnceCell::new(),
        }
    }

    /// The image (or video cover) cached under `id`, as JPEG, PNG, GIF or WebP.
    pub(crate) fn load(&self, id: &str, size: MediaSize) -> Result<CachedImage, String> {
        assert!(is_media_id(id), "media id must be validated by the caller");
        let (path, variant) = self
            .find(id, size)
            .ok_or("not in the local cache (never downloaded, or cleared)")?;
        let raw = fs::read(&path).map_err(|error| format!("cannot read cached file: {error}"))?;
        let data = if media_decrypt::is_v2_media_header(&raw) {
            let keys = self
                .keys
                .get_or_init(|| media_key::find_media_keys(&self.base, [path.as_path()]))
                .as_ref()
                .map_err(|error| format!("cannot derive media keys: {error}"))?;
            let decrypted = media_decrypt::decrypt_v2(&path, keys)?;
            if decrypted.ext == "tggf" {
                media_decrypt::convert_tggf_to_jpg(&decrypted.data)?
            } else {
                decrypted.data
            }
        } else {
            raw
        };
        let (mime, ext) = match media_decrypt::detect_ext(&data) {
            "jpg" => ("image/jpeg", "jpg"),
            "png" => ("image/png", "png"),
            "gif" => ("image/gif", "gif"),
            "webp" => ("image/webp", "webp"),
            other => return Err(format!("unsupported image format: {other}")),
        };
        Ok(CachedImage {
            mime,
            ext,
            data,
            variant,
        })
    }

    fn find(&self, id: &str, size: MediaSize) -> Option<(PathBuf, &'static str)> {
        let image = match size {
            MediaSize::Preview => [("", "standard"), ("_t", "thumbnail"), ("_h", "original")],
            MediaSize::Original => [("_h", "original"), ("", "standard"), ("_t", "thumbnail")],
        };
        let video = match size {
            MediaSize::Preview => [("_thumb.jpg", "video cover"), (".jpg", "large video cover")],
            MediaSize::Original => [(".jpg", "large video cover"), ("_thumb.jpg", "video cover")],
        };
        let images = image
            .iter()
            .map(|(suffix, variant)| (&self.image_dirs, format!("{id}{suffix}.dat"), *variant));
        let videos = video
            .iter()
            .map(|(suffix, variant)| (&self.video_dirs, format!("{id}{suffix}"), *variant));
        images.chain(videos).find_map(|(dirs, name, variant)| {
            dirs.iter()
                .map(|dir| dir.join(&name))
                .find(|path| path.is_file())
                .map(|path| (path, variant))
        })
    }
}

/// `YYYY-MM` subdirectories of `dir`, newest first, from `min_month` on.
fn month_dirs(dir: &Path, min_month: Option<&str>) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut months: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            is_month_name(&name).then(|| (name, entry.path()))
        })
        .filter(|(name, _)| min_month.is_none_or(|min| name.as_str() >= min))
        .collect();
    months.sort_by(|a, b| b.0.cmp(&a.0));
    months.into_iter().map(|(_, path)| path).collect()
}

fn is_month_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 7
        && bytes[4] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "00437c16256fa6f2316db8cffcc7dce0";

    fn cache_with(files: &[&str]) -> (tempfile::TempDir, MediaCache) {
        let temp = tempfile::tempdir().unwrap();
        let chat = hex::encode(Md5::digest(b"room@chatroom"));
        for file in files {
            let path = temp
                .path()
                .join(file.replace("{chat}", &chat).replace("{id}", ID));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, [0xFF, 0xD8, 0xFF, 0xE0]).unwrap();
        }
        let cache = MediaCache::at(temp.path().to_path_buf(), "room@chatroom", None);
        (temp, cache)
    }

    #[test]
    fn ids_must_be_lowercase_hex_of_fixed_length() {
        assert!(is_media_id(ID));
        assert!(!is_media_id(&ID.to_uppercase()));
        assert!(!is_media_id("../../../../etc/passwd"));
        assert!(!is_media_id(&ID[..31]));
    }

    #[test]
    fn parse_media_id_accepts_the_bare_id_or_the_whole_tag() {
        assert_eq!(parse_media_id(ID).unwrap(), ID);
        assert_eq!(parse_media_id(&format!(" [img:{ID}] ")).unwrap(), ID);
        assert!(parse_media_id("[img:../../etc/passwd]").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn write_private_names_the_file_by_id_and_hides_it_from_others() {
        use std::os::unix::fs::PermissionsExt;
        let (temp, cache) = cache_with(&["msg/attach/{chat}/2026-11/Img/{id}.dat"]);
        let image = cache.load(ID, MediaSize::Original).unwrap();
        let dir = temp.path().join("out");
        let path = write_private(&dir, ID, &image).unwrap();
        assert_eq!(path, dir.join(format!("{ID}.jpg")));
        assert_eq!(fs::read(&path).unwrap(), image.data);
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        // A second save of the same id replaces the copy instead of failing.
        write_private(&dir, ID, &image).unwrap();
    }

    #[test]
    fn preview_prefers_the_standard_copy_and_original_the_hd_one() {
        let (_temp, cache) = cache_with(&[
            "msg/attach/{chat}/2026-11/Img/{id}.dat",
            "msg/attach/{chat}/2026-11/Img/{id}_t.dat",
            "msg/attach/{chat}/2026-11/Img/{id}_h.dat",
        ]);
        assert_eq!(cache.find(ID, MediaSize::Preview).unwrap().1, "standard");
        assert_eq!(cache.find(ID, MediaSize::Original).unwrap().1, "original");
        let image = cache.load(ID, MediaSize::Preview).unwrap();
        assert_eq!(image.mime, "image/jpeg");
    }

    #[test]
    fn falls_back_to_the_thumbnail_and_to_video_covers() {
        let (_temp, cache) = cache_with(&["msg/attach/{chat}/2026-10/Img/{id}_t.dat"]);
        assert_eq!(cache.find(ID, MediaSize::Original).unwrap().1, "thumbnail");

        let (_temp, cache) = cache_with(&["msg/video/2026-11/{id}_thumb.jpg"]);
        assert_eq!(cache.find(ID, MediaSize::Preview).unwrap().1, "video cover");
        assert!(cache
            .find("ffffffffffffffffffffffffffffffff", MediaSize::Preview)
            .is_none());
    }

    #[test]
    fn months_before_the_floor_are_invisible() {
        let temp = tempfile::tempdir().unwrap();
        let chat = hex::encode(Md5::digest(b"room@chatroom"));
        let img = temp.path().join(format!("msg/attach/{chat}/2026-01/Img"));
        fs::create_dir_all(&img).unwrap();
        fs::write(img.join(format!("{ID}.dat")), [0xFF, 0xD8]).unwrap();
        let base = temp.path().to_path_buf();
        assert!(
            MediaCache::at(base.clone(), "room@chatroom", Some("2026-02"))
                .find(ID, MediaSize::Preview)
                .is_none()
        );
        assert!(MediaCache::at(base, "room@chatroom", Some("2026-01"))
            .find(ID, MediaSize::Preview)
            .is_some());
    }
}
