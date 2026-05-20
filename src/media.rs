use std::fs;
use std::path::{Path, PathBuf};

use crate::dictionary;

/// Parsed info about an image message (type 3) from XML.
#[derive(Debug, Clone, Default)]
pub struct ImageInfo {
    pub aes_key: String,
    pub cdn_thumb_url: String,
    pub cdn_midi_url: String,
    pub cdn_big_url: String,
    pub thumb_width: u32,
    pub thumb_height: u32,
    pub raw_length: u64,
}

impl ImageInfo {
    pub fn identifier(&self) -> Option<&str> {
        non_empty(&self.aes_key).or_else(|| non_empty(&self.cdn_thumb_url))
    }

    pub fn display(&self) -> String {
        image_tag(self.identifier())
    }

    #[allow(dead_code)]
    pub fn display_verbose(&self) -> String {
        let dims = if self.thumb_width > 0 && self.thumb_height > 0 {
            format!(" {}x{}", self.thumb_width, self.thumb_height)
        } else {
            String::new()
        };
        let size = if self.raw_length > 0 {
            if self.raw_length > 1024 * 1024 {
                format!(" {:.1}MB", self.raw_length as f64 / (1024.0 * 1024.0))
            } else if self.raw_length > 1024 {
                format!(" {}KB", self.raw_length / 1024)
            } else {
                format!(" {}B", self.raw_length)
            }
        } else {
            String::new()
        };
        format!("[图片{}{}]", dims, size)
    }
}

pub(crate) fn image_tag(identifier: Option<&str>) -> String {
    match identifier.and_then(non_empty) {
        Some(id) => format!("[img:{}]", id),
        None => "[img]".to_string(),
    }
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Parsed info about a video message (type 43) from XML.
#[derive(Debug, Clone, Default)]
pub struct VideoInfo {
    pub aes_key: String,
    pub cdn_video_url: String,
    pub cdn_thumb_url: String,
    pub thumb_width: u32,
    pub thumb_height: u32,
    pub play_length: u32,
    pub raw_video_length: u64,
}

impl VideoInfo {
    pub fn display(&self) -> String {
        let dur = if self.play_length > 0 {
            let m = self.play_length / 60;
            let s = self.play_length % 60;
            format!(" {}\u{2032}{:02}\u{2033}", m, s)
        } else {
            String::new()
        };
        let dims = if self.thumb_width > 0 && self.thumb_height > 0 {
            format!(" {}x{}", self.thumb_width, self.thumb_height)
        } else {
            String::new()
        };
        format!("[视频{}{}]", dur, dims)
    }
}

/// Parsed info about a sticker message (type 47).
#[derive(Debug, Clone, Default)]
pub struct StickerInfo {
    pub md5: String,
    pub aes_key: String,
    pub product_id: String,
    pub url: String,
    pub cdn_url: String,
    pub encrypt_url: String,
    pub extern_url: String,
    pub extern_md5: String,
    pub thumb_url: String,
    pub pack_name: String,
    pub pack_url: String,
    pub len: u64,
    pub width: u32,
    pub height: u32,
    pub has_emojibuf: bool,
}

impl StickerInfo {
    pub fn identifier(&self) -> Option<&str> {
        non_empty(&self.md5)
            .or_else(|| non_empty(&self.extern_md5))
            .or_else(|| non_empty(&self.cdn_url))
            .or_else(|| non_empty(&self.extern_url))
            .or_else(|| non_empty(&self.url))
            .or_else(|| non_empty(&self.thumb_url))
            .or_else(|| non_empty(&self.encrypt_url))
    }

    pub fn display(&self) -> String {
        if let Some(id) = self.identifier() {
            return sticker_tag(Some(id));
        }

        let name = if !self.pack_name.is_empty() {
            format!(" {}", self.pack_name)
        } else if !self.product_id.is_empty() {
            let short = self
                .product_id
                .rsplit('.')
                .next()
                .unwrap_or(&self.product_id);
            if short.len() <= 30 {
                format!(" {}", short)
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        if self.has_emojibuf {
            format!("[表情{} (含图)]", name)
        } else {
            format!("[表情{}]", name)
        }
    }
}

pub(crate) fn sticker_tag(identifier: Option<&str>) -> String {
    match identifier.and_then(non_empty) {
        Some(id) => format!("[sticker:{}]", id),
        None => "[表情]".to_string(),
    }
}

/// Parsed info about a link (type 49, subtype 5).
#[derive(Debug, Clone, Default)]
pub struct LinkInfo {
    pub title: String,
    pub description: String,
    pub url: String,
}

impl LinkInfo {
    pub fn display(&self) -> String {
        let mut parts = vec!["[链接]".to_string()];
        if !self.title.is_empty() {
            parts.push(self.title.clone());
        }
        if !self.description.is_empty() {
            parts.push(format!("- {}", self.description));
        }
        if !self.url.is_empty() && self.url.len() < 120 {
            parts.push(format!("\n  {}", self.url));
        }
        parts.join(" ")
    }
}

/// Parsed info about a mini program (type 49, subtype 33).
#[derive(Debug, Clone, Default)]
pub struct MiniProgramInfo {
    pub title: String,
    pub app_name: String,
    pub page_path: String,
}

impl MiniProgramInfo {
    pub fn display(&self) -> String {
        let app = if !self.app_name.is_empty() {
            format!(" - {}", self.app_name)
        } else {
            String::new()
        };
        let title = if !self.title.is_empty() {
            self.title.clone()
        } else {
            "小程序".to_string()
        };
        let path = if !self.page_path.is_empty() && self.page_path.len() < 80 {
            format!("\n  path: {}", self.page_path)
        } else {
            String::new()
        };
        format!("[小程序] {}{}{}", title, app, path)
    }
}

/// Parsed info about a file attachment (type 49, subtype 6).
#[derive(Debug, Clone, Default)]
pub struct FileInfo {
    pub title: String,
    pub total_len: u64,
    pub file_ext: String,
}

impl FileInfo {
    pub fn display(&self, fallback_name: Option<&str>) -> String {
        let name = non_empty(&self.title)
            .or_else(|| fallback_name.and_then(non_empty))
            .map(ToString::to_string)
            .unwrap_or_else(|| {
                let ext = self.file_ext.trim().trim_start_matches('.');
                if ext.is_empty() {
                    "未知文件".to_string()
                } else {
                    format!("未知文件.{}", ext)
                }
            });
        let size = format_file_size(self.total_len);
        if size.is_empty() {
            format!("[文件] {}", name)
        } else {
            format!("[文件] {} ({})", name, size)
        }
    }
}

pub(crate) fn message_media_type(local_type: i64, raw_body: &str) -> Option<&'static str> {
    if is_pat_app_message(raw_body) {
        return None;
    }

    match local_type_low32(local_type) {
        3 => Some("image"),
        34 => Some("voice"),
        43 => Some("video"),
        47 => Some("sticker"),
        62 => Some("file"),
        49 if is_file_app_message(local_type, raw_body) => Some("file"),
        _ => None,
    }
}

pub(crate) fn local_type_low32(local_type: i64) -> i64 {
    ((local_type as u64) & 0xffff_ffff) as i64
}

fn is_file_app_message(local_type: i64, raw_body: &str) -> bool {
    !is_pat_app_message(raw_body)
        && (matches!(app_message_subtype(local_type), Some(6 | 62))
            || matches!(extract_xml_tag_int(raw_body, "type"), Some(6 | 62)))
}

fn app_message_subtype(local_type: i64) -> Option<i64> {
    let encoded = local_type as u64;
    if encoded & 0xffff_ffff != 49 {
        return None;
    }
    let subtype = encoded >> 32;
    (subtype != 0).then_some(subtype as i64)
}

// ===== XML parsing =====

pub(crate) fn parse_image_info(xml: &str) -> ImageInfo {
    let mut info = ImageInfo::default();
    if let Some(v) = extract_xml_attr(xml, "aeskey") {
        info.aes_key = v;
    }
    if let Some(v) = extract_xml_attr(xml, "cdnthumburl") {
        info.cdn_thumb_url = v;
    }
    if let Some(v) = extract_xml_attr(xml, "cdnmidiurl") {
        info.cdn_midi_url = v;
    }
    if let Some(v) = extract_xml_attr(xml, "cdndisplaybackupurl") {
        info.cdn_big_url = v;
    }
    if let Some(v) = extract_xml_attr(xml, "cdnthumbwidth").and_then(|s| s.parse().ok()) {
        info.thumb_width = v;
    }
    if let Some(v) = extract_xml_attr(xml, "cdnthumbheight").and_then(|s| s.parse().ok()) {
        info.thumb_height = v;
    }
    if let Some(v) = extract_xml_attr(xml, "rawlength").and_then(|s| s.parse().ok()) {
        info.raw_length = v;
    }
    if info.raw_length == 0 {
        if let Some(v) = extract_xml_attr(xml, "cdnmidimagerawlength").and_then(|s| s.parse().ok())
        {
            info.raw_length = v;
        }
    }
    info
}

pub(crate) fn parse_video_info(xml: &str) -> VideoInfo {
    let mut info = VideoInfo::default();
    if let Some(v) = extract_xml_attr(xml, "aeskey") {
        info.aes_key = v;
    }
    if let Some(v) = extract_xml_attr(xml, "cdnvideourl") {
        info.cdn_video_url = v;
    }
    if let Some(v) = extract_xml_attr(xml, "cdnthumburl") {
        info.cdn_thumb_url = v;
    }
    if let Some(v) = extract_xml_attr(xml, "cdnthumbwidth").and_then(|s| s.parse().ok()) {
        info.thumb_width = v;
    }
    if let Some(v) = extract_xml_attr(xml, "cdnthumbheight").and_then(|s| s.parse().ok()) {
        info.thumb_height = v;
    }
    if let Some(v) = extract_xml_attr(xml, "playlength").and_then(|s| s.parse().ok()) {
        info.play_length = v;
    }
    if let Some(v) = extract_xml_attr(xml, "rawvideolength").and_then(|s| s.parse().ok()) {
        info.raw_video_length = v;
    }
    info
}

pub(crate) fn parse_sticker_info(xml: &str) -> StickerInfo {
    let mut info = StickerInfo::default();
    if let Some(v) = extract_xml_attr(xml, "md5") {
        info.md5 = v;
    }
    if let Some(v) = extract_xml_attr(xml, "aeskey") {
        info.aes_key = v;
    }
    if let Some(v) = extract_xml_attr(xml, "productid") {
        info.product_id = v;
    }
    if let Some(v) = extract_xml_attr(xml, "url") {
        info.url = v;
    }
    if let Some(v) = extract_xml_attr(xml, "cdnurl") {
        info.cdn_url = v;
    }
    if let Some(v) = extract_xml_attr(xml, "encrypturl") {
        info.encrypt_url = v;
    }
    if let Some(v) = extract_xml_attr(xml, "externurl") {
        info.extern_url = v;
    }
    if let Some(v) = extract_xml_attr(xml, "externmd5") {
        info.extern_md5 = v;
    }
    if let Some(v) = extract_xml_attr(xml, "thumburl") {
        info.thumb_url = v;
    }
    if let Some(v) = extract_xml_tag(xml, "packname") {
        info.pack_name = v;
    }
    if let Some(v) = extract_xml_attr(xml, "packurl") {
        info.pack_url = v;
    }
    if let Some(v) = extract_xml_attr(xml, "len").and_then(|s| s.parse().ok()) {
        info.len = v;
    }
    if let Some(v) = extract_xml_attr(xml, "width").and_then(|s| s.parse().ok()) {
        info.width = v;
    }
    if let Some(v) = extract_xml_attr(xml, "height").and_then(|s| s.parse().ok()) {
        info.height = v;
    }
    info.has_emojibuf = xml.contains("<emojibuf>");
    info
}

pub(crate) fn parse_link_info(xml: &str) -> Option<LinkInfo> {
    if !xml.contains("<type>5</type>") {
        return None;
    }
    Some(LinkInfo {
        title: extract_xml_tag(xml, "title").unwrap_or_default(),
        description: extract_xml_tag(xml, "des").unwrap_or_default(),
        url: extract_xml_tag(xml, "url").unwrap_or_default(),
    })
}

pub(crate) fn parse_mini_program_info(xml: &str) -> Option<MiniProgramInfo> {
    if !xml.contains("<type>33</type>") {
        return None;
    }
    Some(MiniProgramInfo {
        title: extract_xml_tag(xml, "title").unwrap_or_default(),
        app_name: extract_xml_tag(xml, "appname").unwrap_or_default(),
        page_path: extract_xml_tag(xml, "pagepath").unwrap_or_default(),
    })
}

pub(crate) fn parse_file_info(xml: &str) -> Option<FileInfo> {
    if is_pat_app_message(xml) {
        return None;
    }
    if !xml.contains("<type>6</type>") && !xml.contains("<type>62</type>") {
        return None;
    }
    Some(FileInfo {
        title: extract_xml_tag(xml, "title").unwrap_or_default(),
        total_len: extract_xml_tag(xml, "totallen")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        file_ext: extract_xml_tag(xml, "fileext").unwrap_or_default(),
    })
}

pub(crate) fn is_pat_app_message(xml: &str) -> bool {
    xml.contains("<patinfo")
}

pub(crate) fn format_file_size(bytes: u64) -> String {
    if bytes == 0 {
        return String::new();
    }
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}GB", bytes as f64 / GB)
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / MB)
    } else if bytes >= 1024 {
        format!("{}KB", (bytes as f64 / KB).round() as u64)
    } else {
        format!("{}B", bytes)
    }
}

// ===== Telegram base path detection =====

/// Find the Telegram account data directory (base for media cache lookups).
///
/// Telegram 3.x: account files with `Message/MessageTemp` subdir
/// Telegram 4.x: account files with `msg/` subdir
pub fn find_telegram_base_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let home = PathBuf::from(home);
    for docs_base in dictionary::account_files_candidate_dirs(&home) {
        if !docs_base.is_dir() {
            continue;
        }

        let Ok(entries) = fs::read_dir(&docs_base) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path.join("Message/MessageTemp").is_dir() {
                return Some(path);
            }
            if path.join("msg").is_dir() {
                return Some(path);
            }
        }
    }
    None
}

// ===== Sticker cache search =====

pub fn find_cached_sticker(base_path: &Path, md5: &str) -> Option<PathBuf> {
    let md5 = md5.trim().to_lowercase();
    if md5.len() < 2 {
        return None;
    }

    let cache_dir = base_path.join("cache");
    if !cache_dir.is_dir() {
        return None;
    }

    let prefix = &md5[..2];
    if let Ok(months) = fs::read_dir(&cache_dir) {
        for month in months.flatten() {
            let candidate = month.path().join("Emoticon").join(prefix).join(&md5);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    find_file_named(&cache_dir, &md5)
}

fn find_file_named(dir: &Path, target: &str) -> Option<PathBuf> {
    fn walk(dir: &Path, target: &str) -> Option<PathBuf> {
        let entries = fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = walk(&path, target) {
                    return Some(found);
                }
            } else {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if name == target {
                    return Some(path);
                }
            }
        }
        None
    }

    walk(dir, target)
}

pub fn decrypt_sticker_aes_cbc(data: &[u8], aes_key_hex: &str) -> Option<Vec<u8>> {
    use aes::Aes128;
    use cbc::Decryptor;
    use cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};

    let key = hex::decode(aes_key_hex).ok()?;
    if key.len() != 16 || data.is_empty() || !data.len().is_multiple_of(16) {
        return None;
    }

    let mut buf = data.to_vec();
    let cipher = Decryptor::<Aes128>::new_from_slices(&key, &key).ok()?;
    let plaintext = cipher.decrypt_padded_mut::<Pkcs7>(&mut buf).ok()?;
    Some(plaintext.to_vec())
}

pub fn export_media_file(
    src: &Path,
    output_dir: &Path,
    session_name: &str,
    msg_type_name: &str,
    index: usize,
) -> Result<PathBuf, String> {
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("bin");
    let filename = format!(
        "{}_{}_{:04}.{}",
        sanitize_filename(session_name),
        msg_type_name,
        index,
        ext
    );
    let dest = output_dir.join(&filename);

    fs::create_dir_all(output_dir).map_err(|e| format!("Cannot create media dir: {}", e))?;
    fs::copy(src, &dest).map_err(|e| format!("Cannot copy media file: {}", e))?;

    Ok(dest)
}

pub(crate) fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ===== XML helpers (reused from message.rs) =====

pub(crate) fn extract_xml_attr(xml: &str, attr: &str) -> Option<String> {
    let mut search_from = 0;
    let bytes = xml.as_bytes();

    while let Some(relative_start) = xml[search_from..].find(attr) {
        let start = search_from + relative_start;
        let has_attr_boundary = xml[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c == '<' || c.is_whitespace());
        if !has_attr_boundary {
            search_from = start + 1;
            continue;
        }

        let mut pos = start + attr.len();
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() || bytes[pos] != b'=' {
            search_from = start + attr.len();
            continue;
        }
        pos += 1;
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() || (bytes[pos] != b'"' && bytes[pos] != b'\'') {
            search_from = start + attr.len();
            continue;
        }
        let quote = bytes[pos] as char;
        let value_start = pos + 1;
        let rest = &xml[value_start..];
        let end = rest.find(quote)?;
        let value = decode_xml_entities(&rest[..end]);
        return if value.is_empty() { None } else { Some(value) };
    }

    None
}

pub(crate) fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)?;
    let value_start = start + open.len();
    if value_start >= xml.len() {
        return None;
    }
    let rest = &xml[value_start..];
    let value_end = rest.find(&close)?;
    let value = decode_xml_entities(rest[..value_end].trim());
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

pub(crate) fn extract_xml_tag_int(xml: &str, tag: &str) -> Option<i64> {
    let text = extract_xml_tag(xml, tag)?;
    text.parse::<i64>().ok()
}

fn decode_xml_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_xml_attr_exact_name_and_empty_value() {
        let xml =
            r#"<emoji productid="" cdnurl="http://x.test/a?m=1&amp;n=2" externurl=""></emoji>"#;
        assert_eq!(extract_xml_attr(xml, "productid"), None);
        assert_eq!(extract_xml_attr(xml, "url"), None);
        assert_eq!(
            extract_xml_attr(xml, "cdnurl").as_deref(),
            Some("http://x.test/a?m=1&n=2")
        );
    }

    #[test]
    fn test_extract_xml_attr_accepts_spaced_equals() {
        let xml = r#"<emoji cdnurl = "http://x.test/a?m=1&amp;n=2" aeskey= 'abc123'></emoji>"#;
        assert_eq!(
            extract_xml_attr(xml, "cdnurl").as_deref(),
            Some("http://x.test/a?m=1&n=2")
        );
        assert_eq!(extract_xml_attr(xml, "aeskey").as_deref(), Some("abc123"));
    }

    #[test]
    fn test_parse_sticker_info_core_fields() {
        let xml = r#"<emoji md5="abc123" len="4963" cdnurl="http://x.test/a.gif" encrypturl="http://x.test/e" aeskey="00112233445566778899aabbccddeeff" width="48" height="47"></emoji>"#;
        let info = parse_sticker_info(xml);
        assert_eq!(info.md5, "abc123");
        assert_eq!(info.aes_key, "00112233445566778899aabbccddeeff");
        assert_eq!(info.cdn_url, "http://x.test/a.gif");
        assert_eq!(info.encrypt_url, "http://x.test/e");
        assert_eq!(info.len, 4963);
        assert_eq!(info.width, 48);
        assert_eq!(info.height, 47);
        assert_eq!(info.identifier(), Some("abc123"));
        assert_eq!(info.display(), "[sticker:abc123]");
    }

    #[test]
    fn test_sticker_display_without_identifier_keeps_summary() {
        let info = parse_sticker_info(
            r#"<emoji productid="com.test.pack"><packname>fun</packname><emojibuf>abc</emojibuf></emoji>"#,
        );
        assert_eq!(info.identifier(), None);
        assert_eq!(info.display(), "[表情 fun (含图)]");

        let info = parse_sticker_info(r#"<emoji><emojibuf>abc</emojibuf></emoji>"#);
        assert_eq!(info.identifier(), None);
        assert_eq!(info.display(), "[表情 (含图)]");
    }

    #[test]
    fn test_image_display_compact_identifier_and_verbose_details() {
        let xml = r#"<msg><img aeskey="abc123" cdnthumbwidth="180" cdnthumbheight="153" rawlength="38186" /></msg>"#;
        let info = parse_image_info(xml);
        assert_eq!(info.identifier(), Some("abc123"));
        assert_eq!(info.display(), "[img:abc123]");
        assert_eq!(info.display_verbose(), "[图片 180x153 37KB]");
    }

    #[test]
    fn test_image_display_without_identifier() {
        let info =
            parse_image_info(r#"<msg><img cdnthumbwidth="180" cdnthumbheight="153" /></msg>"#);
        assert_eq!(info.identifier(), None);
        assert_eq!(info.display(), "[img]");
    }

    #[test]
    fn test_parse_file_info_display() {
        let xml = r#"<msg><appmsg><title>report.pdf</title><type>6</type><appattach><totallen>1536000</totallen><fileext>pdf</fileext><md5>abc</md5></appattach></appmsg></msg>"#;
        let info = parse_file_info(xml).unwrap();
        assert_eq!(info.title, "report.pdf");
        assert_eq!(info.file_ext, "pdf");
        assert_eq!(info.total_len, 1536000);
        assert_eq!(info.display(None), "[文件] report.pdf (1.5MB)");
    }

    #[test]
    fn test_message_media_type_normalizes_structured_media() {
        assert_eq!(message_media_type(34, ""), Some("voice"));
        assert_eq!(message_media_type(3, ""), Some("image"));
        assert_eq!(message_media_type(47, ""), Some("sticker"));
        assert_eq!(message_media_type(43, ""), Some("video"));
        assert_eq!(message_media_type(62, ""), Some("file"));
        assert_eq!(message_media_type((6_i64 << 32) | 49, ""), Some("file"));
        assert_eq!(
            message_media_type(
                49,
                "<msg><appmsg><title>report.pdf</title><type>6</type></appmsg></msg>"
            ),
            Some("file")
        );
        assert_eq!(
            message_media_type(49, "<msg><appmsg><type>5</type></appmsg></msg>"),
            None
        );
    }

    #[test]
    fn test_pat_app_message_is_not_file_media() {
        let xml = r#"<msg><appmsg><title>我拍了拍 "Bob"</title><type>62</type><appattach><totallen>0</totallen></appattach><patinfo><template>我拍了拍 "${tgid_bob}"</template></patinfo></appmsg></msg>"#;

        assert!(parse_file_info(xml).is_none());
        assert_eq!(message_media_type((62_i64 << 32) | 49, xml), None);
    }
}
