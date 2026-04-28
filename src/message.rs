use crate::media;
use crate::media_pb;
use flate2::read::ZlibDecoder;
use std::fmt;
use std::io::Read;

/// Telegram消息类型枚举。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MessageType {
    Text,            // 1
    Image,           // 3
    Voice,           // 34
    Sticker,         // 47
    Video,           // 43
    Link,            // 49 — 链接/文件/小程序/音乐
    System,          // 10000
    RedEnvelope,     // 436207665
    Transfer,        // 536870918
    Location,        // 48
    File,            // 62
    Call,            // 50
    Music,           // 419430449
    Revoke,          // 10002 撤回消息
    Unknown(i32),    // 其他
}

impl fmt::Display for MessageType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            MessageType::Text => "文本",
            MessageType::Image => "图片",
            MessageType::Voice => "语音",
            MessageType::Sticker => "表情",
            MessageType::Video => "视频",
            MessageType::Link => "链接/文件/小程序",
            MessageType::System => "系统提示",
            MessageType::RedEnvelope => "红包",
            MessageType::Transfer => "转账",
            MessageType::Location => "位置",
            MessageType::File => "文件",
            MessageType::Call => "语音/视频通话",
            MessageType::Music => "音乐",
            MessageType::Revoke => "撤回消息",
            MessageType::Unknown(n) => return write!(f, "未知({})", n),
        };
        write!(f, "{}", name)
    }
}

impl From<i32> for MessageType {
    fn from(t: i32) -> Self {
        match t {
            1 => MessageType::Text,
            3 => MessageType::Image,
            34 => MessageType::Voice,
            43 => MessageType::Video,
            47 => MessageType::Sticker,
            48 => MessageType::Location,
            49 => MessageType::Link,
            50 => MessageType::Call,
            62 => MessageType::File,
            10000 => MessageType::System,
            10002 => MessageType::Revoke,
            419430449 => MessageType::Music,
            436207665 => MessageType::RedEnvelope,
            536870918 => MessageType::Transfer,
            n => MessageType::Unknown(n),
        }
    }
}

/// 解码后的消息。
#[derive(Debug, Clone)]
pub struct DecodedMessage {
    pub msg_type: MessageType,
    /// 人类可读的内容。
    pub content: String,
    /// 发送者显示名（群聊中会从 tgid 解析为昵称）。
    pub display_name: String,
}

/// 解码消息。
///
/// # Arguments
/// - `msg_type` — 原始消息类型值
/// - `raw_content` — 原始消息内容（可能为 TEXT 或 BLOB 转 String）
/// - `session_display_name` — 会话的显示名（私聊即对方昵称，群聊即群名）
/// - `wcdb_ct` — `WCDB_CT_message_content` 字段值
/// - `packed_info_data` — `packed_info_data` 字段二进制数据（Telegram 4.x 媒体元信息）
/// - `resolve_display_name` — 将 tgid 解析为显示名的函数
pub fn decode_message(
    msg_type: i32,
    raw_content: &str,
    session_display_name: &str,
    _wcdb_ct: Option<i64>,
    packed_info_data: &[u8],
    resolve_display_name: impl Fn(&str) -> String,
) -> DecodedMessage {
    let msg_type_enum: MessageType = msg_type.into();

    if msg_type == 10000 || msg_type == 10002 {
        return DecodedMessage {
            msg_type: msg_type_enum,
            content: raw_content.to_string(),
            display_name: "系统".to_string(),
        };
    }

    if is_media_type(msg_type) {
        let (sender_id, clean_content) = parse_sender_from_content(raw_content);
        let display_name = match sender_id {
            Some(id) => resolve_display_name(id),
            None => session_display_name.to_string(),
        };
        let content = decode_media_content(msg_type, clean_content, packed_info_data);
        return DecodedMessage {
            msg_type: msg_type_enum,
            content,
            display_name,
        };
    }

    if raw_content.is_empty() && msg_type != 1 {
        return DecodedMessage {
            msg_type: msg_type_enum,
            content: format!("[{}]", msg_type_enum),
            display_name: session_display_name.to_string(),
        };
    }

    let (sender_id, clean_content) = parse_sender_from_content(raw_content);
    let display_name = match sender_id {
        Some(id) => resolve_display_name(id),
        None => session_display_name.to_string(),
    };
    let content = decode_content_by_type(msg_type, clean_content, &resolve_display_name);

    DecodedMessage {
        msg_type: msg_type_enum,
        content,
        display_name,
    }
}

fn is_media_type(t: i32) -> bool {
    matches!(t, 3 | 34 | 43 | 47)
}

/// 解码媒体类消息：优先使用 packed_info_data 的 protobuf 元信息，回退到 XML 解析。
fn decode_media_content(msg_type: i32, raw_content: &str, packed_info: &[u8]) -> String {
    match msg_type {
        34 => {
            if let Some(audio) = extract_audio_meta(packed_info) {
                return audio;
            }
            let dur = extract_voice_duration(raw_content);
            format!("[语音{}]", dur)
        }
        47 => media::parse_sticker_info(raw_content).display(),
        43 => {
            if let Some(video) = extract_video_display(packed_info) {
                return video;
            }
            media::parse_video_info(raw_content).display()
        }
        3 => {
            if let Some(img) = extract_image_display(packed_info) {
                return img;
            }
            media::parse_image_info(raw_content).display()
        }
        _ => {
            let type_name: MessageType = msg_type.into();
            format!("[{}]", type_name)
        }
    }
}

fn extract_image_display(data: &[u8]) -> Option<String> {
    if data.is_empty() { return None; }
    if let Some(v2) = media_pb::parse_img2(data) {
        if let Some(img) = v2.image {
            return Some(media_pb::display_image(&img));
        }
    }
    if let Some(v1) = media_pb::parse_img(data) {
        if !v1.filename.is_empty() {
            return Some(format!("[图片] {}", v1.filename));
        }
    }
    None
}

fn extract_video_display(data: &[u8]) -> Option<String> {
    if data.is_empty() { return None; }
    if let Some(v2) = media_pb::parse_img2(data) {
        if let Some(vid) = v2.video {
            return Some(media_pb::display_video(&vid));
        }
    }
    None
}

fn extract_audio_meta(data: &[u8]) -> Option<String> {
    if data.is_empty() { return None; }
    if let Some(v2) = media_pb::parse_img2(data) {
        if let Some(audio) = v2.audio {
            if !audio.audio_text.is_empty() {
                return Some(format!("[语音] {}", audio.audio_text));
            }
        }
    }
    None
}

fn decode_content_by_type(
    msg_type: i32,
    content: &str,
    resolve_display_name: &impl Fn(&str) -> String,
) -> String {
    match msg_type {
        49 => decode_link_content(content, resolve_display_name),
        48 => decode_location_content(content),
        50 => decode_call_content(content),
        62 => decode_file_content(content),
        419430449 => decode_music_content(content),
        436207665 | 536870918 => format!("[{}]", MessageType::from(msg_type)),
        _ => content.to_string(),
    }
}

fn decode_link_content(content: &str, resolve_display_name: &impl Fn(&str) -> String) -> String {
    if !content.trim_start().starts_with('<') {
        if content.len() > 200 {
            return format!("[链接] {}", &content[..200]);
        }
        return format!("[链接] {}", content);
    }

    let sub_type = crate::media::extract_xml_tag_int(content, "type").unwrap_or(0);
    match sub_type {
        5 => media::parse_link_info(content)
            .as_ref()
            .map(media::LinkInfo::display)
            .unwrap_or_else(|| "[链接]".to_string()),
        33 => media::parse_mini_program_info(content)
            .as_ref()
            .map(media::MiniProgramInfo::display)
            .unwrap_or_else(|| "[小程序]".to_string()),
        3 => {
            let title = crate::media::extract_xml_tag(content, "title").unwrap_or_else(|| "未知歌曲".to_string());
            format!("[音乐] {}", title)
        }
        6 => {
            let name = crate::media::extract_xml_tag(content, "title").unwrap_or_else(|| "未知文件".to_string());
            format!("[文件] {}", name)
        }
        57 => decode_quote_content(content, resolve_display_name),
        51 => {
            let title = crate::media::extract_xml_tag(content, "title").unwrap_or_else(|| "聊天记录".to_string());
            format!("[引用: {}]", title)
        }
        _ => {
            let title = crate::media::extract_xml_tag(content, "title").unwrap_or_default();
            let desc = crate::media::extract_xml_tag(content, "des").unwrap_or_default();
            let title = if !title.is_empty() { title } else { "未知卡片".to_string() };
            if !desc.is_empty() {
                format!("[卡片] {} - {}", title, desc)
            } else {
                format!("[卡片] {}", title)
            }
        }
    }
}

fn decode_quote_content(content: &str, resolve_display_name: &impl Fn(&str) -> String) -> String {
    let reply = crate::media::extract_xml_tag(content, "title").unwrap_or_default();
    let quoted = crate::media::extract_xml_tag(content, "refermsg")
        .map(|refermsg| decode_refermsg_content(&refermsg, resolve_display_name))
        .unwrap_or_default();

    match (quoted.is_empty(), reply.is_empty()) {
        (false, false) => format!("> {}\n        {}", quoted, reply),
        (false, true) => format!("> {}", quoted),
        (true, false) => reply,
        (true, true) => "[引用]".to_string(),
    }
}

fn decode_refermsg_content(
    refermsg: &str,
    resolve_display_name: &impl Fn(&str) -> String,
) -> String {
    let ref_type = crate::media::extract_xml_tag_int(refermsg, "type")
        .map(|n| n as i32)
        .unwrap_or(1);
    let ref_content = crate::media::extract_xml_tag(refermsg, "content").unwrap_or_default();
    let (content_sender, clean_content) = parse_sender_from_content(&ref_content);

    let body = if clean_content.is_empty() {
        format!("[{}]", MessageType::from(ref_type))
    } else {
        match ref_type {
            1 => clean_content.to_string(),
            t if is_media_type(t) => decode_media_content(t, clean_content, &[]),
            49 if clean_content.trim_start().starts_with('<') => decode_link_content(clean_content, resolve_display_name),
            _ if clean_content.trim_start().starts_with('<') => format!("[{}]", MessageType::from(ref_type)),
            _ => clean_content.to_string(),
        }
    };

    match decode_refermsg_sender(refermsg, content_sender, resolve_display_name) {
        Some(sender) => format!("{}: {}", sender, body),
        None => body,
    }
}

fn decode_refermsg_sender(
    refermsg: &str,
    content_sender: Option<&str>,
    resolve_display_name: &impl Fn(&str) -> String,
) -> Option<String> {
    if let Some(name) = crate::media::extract_xml_tag(refermsg, "displayname") {
        return Some(name);
    }

    let sender_id = crate::media::extract_xml_tag(refermsg, "chatusr")
        .or_else(|| content_sender.map(|id| id.to_string()))
        .or_else(|| {
            crate::media::extract_xml_tag(refermsg, "fromusr")
                .filter(|id| !id.contains("@chatroom"))
        })?;
    let sender = resolve_display_name(&sender_id);
    if sender.is_empty() { None } else { Some(sender) }
}

fn decode_location_content(content: &str) -> String {
    if !content.trim_start().starts_with('<') {
        return format!("[位置] {}", content);
    }
    let label = crate::media::extract_xml_attr(content, "label")
        .or_else(|| crate::media::extract_xml_tag(content, "label"));
    let poiname = crate::media::extract_xml_attr(content, "poiname")
        .or_else(|| crate::media::extract_xml_tag(content, "poiname"));
    let location_name = poiname.as_deref().or(label.as_deref()).unwrap_or("未知位置");
    format!("[位置] {}", location_name)
}

fn decode_call_content(content: &str) -> String {
    if let Some(dur_secs) = crate::media::extract_xml_tag_int(content, "duration").or_else(|| {
        content.split_whitespace().find_map(|w| w.parse::<i64>().ok())
    }) {
        let mins = dur_secs / 60;
        let secs = dur_secs % 60;
        if mins > 0 {
            return format!("[通话] {}分{}秒", mins, secs);
        }
        return format!("[通话] {}秒", secs);
    }
    "[通话]".to_string()
}

fn decode_file_content(content: &str) -> String {
    let name = if content.contains('/') {
        content.rsplit('/').next().unwrap_or(content)
    } else if !content.is_empty() {
        content
    } else {
        return "[文件]".to_string();
    };
    if name.len() > 80 {
        format!("[文件] {}...", &name[..77])
    } else {
        format!("[文件] {}", name)
    }
}

fn decode_music_content(content: &str) -> String {
    if !content.is_empty() {
        format!("[音乐] {}", content)
    } else {
        "[音乐]".to_string()
    }
}

fn extract_voice_duration(content: &str) -> String {
    let dur = crate::media::extract_xml_tag_int(content, "voicelength")
        .or_else(|| crate::media::extract_xml_tag_int(content, "duration"))
        .or_else(|| {
            content.split(|c: char| !c.is_ascii_digit())
                .find_map(|s| s.parse::<i64>().ok())
        });
    match dur {
        Some(d) if d > 0 => format!(" {}秒", d),
        _ => String::new(),
    }
}

/// Decompress message content.
///
/// Tries ZSTD first (Telegram 4.x), then ZLIB (older), then falls back to raw UTF-8.
pub fn try_decompress(raw: &[u8]) -> Option<String> {
    if let Ok(s) = try_zstd(raw) {
        if !s.is_empty() { return Some(s); }
    }
    if let Ok(s) = try_zlib(raw) {
        if !s.is_empty() { return Some(s); }
    }
    String::from_utf8(raw.to_vec()).ok().filter(|s| !s.is_empty())
}

fn try_zstd(data: &[u8]) -> Result<String, ()> {
    let mut d = zstd::Decoder::new(data).map_err(|_| ())?;
    let mut s = String::new();
    d.read_to_string(&mut s).map_err(|_| ())?;
    if s.is_empty() { Err(()) } else { Ok(s) }
}

fn try_zlib(data: &[u8]) -> Result<String, ()> {
    let mut d = ZlibDecoder::new(data);
    let mut s = String::new();
    d.read_to_string(&mut s).map_err(|_| ())?;
    if s.is_empty() { Err(()) } else { Ok(s) }
}

/// Parse sender tgid from message content.
pub fn parse_sender_from_content(content: &str) -> (Option<&str>, &str) {
    for (i, c) in content.char_indices() {
        if c != ':' { continue; }
        if i == 0 { break; }
        let prefix = &content[..i];
        let is_id = prefix.starts_with("tgid_")
            || prefix.starts_with("gh_")
            || prefix.contains('@')
            || prefix.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if is_id && prefix.len() >= 3 {
            let after = &content[i + 1..];
            let after = after.trim_start_matches([' ', '\n']);
            return (Some(prefix), after);
        }
        break;
    }
    (None, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_type_display() {
        assert_eq!(MessageType::Text.to_string(), "文本");
        assert_eq!(MessageType::Image.to_string(), "图片");
    }

    #[test]
    fn test_message_type_from_i32() {
        assert_eq!(MessageType::from(1), MessageType::Text);
        assert_eq!(MessageType::from(42), MessageType::Unknown(42));
    }

    #[test]
    fn test_decode_text_message() {
        let d = decode_message(1, "Hello", "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, "Hello");
        assert_eq!(d.display_name, "Alice");
    }

    #[test]
    fn test_decode_system_message() {
        let d = decode_message(10000, "提示", "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, "提示");
        assert_eq!(d.display_name, "系统");
    }

    #[test]
    fn test_decode_red_envelope() {
        let d = decode_message(436207665, "", "Alice", None, &[], |id| id.to_string());
        assert!(d.content.contains("红包"));
    }

    #[test]
    fn test_decode_image_with_packed_info() {
        let mut packed = Vec::new();
        packed.push(8); packed.push(1);
        let img = [8, 0xb8, 0x08, 16, 0x80, 0x0f, 34, 8, 116, 101, 115, 116, 46, 106, 112, 103];
        packed.push(26);
        packed.push(img.len() as u8);
        packed.extend_from_slice(&img);
        let d = decode_message(3, "", "Alice", None, &packed, |id| id.to_string());
        assert_eq!(d.msg_type, MessageType::Image);
        assert!(d.content.contains("1920"));
        assert!(d.content.contains("1080"));
    }

    #[test]
    fn test_decode_group_chat_sender() {
        let d = decode_message(1, "tgid_abc:\nHello", "Group", None, &[], |id| {
            if id == "tgid_abc" { "Bob".into() } else { id.into() }
        });
        assert_eq!(d.content, "Hello");
        assert_eq!(d.display_name, "Bob");
    }

    #[test]
    fn test_decode_media_group_chat_sender() {
        let d = decode_message(3, "tgid_abc:\n", "Group", None, &[], |id| {
            if id == "tgid_abc" { "Bob".into() } else { id.into() }
        });
        assert_eq!(d.display_name, "Bob");
    }

    #[test]
    fn test_parse_sender() {
        let (id, c) = parse_sender_from_content("tgid_a:\nHi");
        assert_eq!(id, Some("tgid_a"));
        assert_eq!(c, "Hi");
        let (id, c) = parse_sender_from_content("plain text");
        assert_eq!(id, None);
        assert_eq!(c, "plain text");
    }

    #[test]
    fn test_decode_link() {
        let xml = r#"<?xml?><msg><appmsg><title>标题</title><type>5</type></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| id.to_string());
        assert!(d.content.contains("链接"));
        assert!(d.content.contains("标题"));
    }

    #[test]
    fn test_decode_quote_message() {
        let xml = r#"<msg><appmsg><title>回复内容</title><type>57</type><refermsg><type>1</type><displayname>Bob</displayname><content>引用内容</content></refermsg></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, "> Bob: 引用内容\n        回复内容");
    }

    #[test]
    fn test_decode_quoted_group_image() {
        let xml = r#"<msg><appmsg><title>回复图片</title><type>57</type><refermsg><type>3</type><chatusr>tgid_abc</chatusr><content>tgid_abc:
&lt;msg&gt;&lt;img cdnthumbwidth="180" cdnthumbheight="153" length="38186" /&gt;&lt;/msg&gt;</content></refermsg></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| {
            if id == "tgid_abc" { "Bob".into() } else { id.into() }
        });
        assert_eq!(d.content, "> Bob: [图片 180x153]\n        回复图片");
    }
}
