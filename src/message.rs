use crate::media;
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
    #[allow(dead_code)]
    ChatHistory,     // 引用聊天记录 (type 49 subtype)
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
            MessageType::ChatHistory => "引用消息",
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
/// - `resolve_display_name` — 将 tgid 解析为显示名的函数（群聊场景）
pub fn decode_message(
    msg_type: i32,
    raw_content: &str,
    session_display_name: &str,
    _wcdb_ct: Option<i64>,
    resolve_display_name: impl Fn(&str) -> String,
) -> DecodedMessage {
    // 派生消息类型
    let msg_type_enum: MessageType = msg_type.into();

    // 系统消息和撤回消息使用原始内容
    if msg_type == 10000 || msg_type == 10002 {
        return DecodedMessage {
            msg_type: msg_type_enum,
            content: raw_content.to_string(),
            display_name: "系统".to_string(),
        };
    }

    // 媒体类消息：不依赖 content
    if is_media_type(msg_type) {
        let content = decode_media_content(msg_type, raw_content);
        return DecodedMessage {
            msg_type: msg_type_enum,
            content,
            display_name: session_display_name.to_string(),
        };
    }

    // 空内容非文本消息
    if raw_content.is_empty() && msg_type != 1 {
        return DecodedMessage {
            msg_type: msg_type_enum,
            content: format!("[{}]", msg_type_enum),
            display_name: session_display_name.to_string(),
        };
    }

    // 一般内容消息：先解析 sender，再解码内容
    let (sender_id, clean_content) = parse_sender_from_content(raw_content);
    let display_name = match sender_id {
        Some(id) => resolve_display_name(id),
        None => session_display_name.to_string(),
    };
    let content = decode_content_by_type(msg_type, clean_content);

    DecodedMessage {
        msg_type: msg_type_enum,
        content,
        display_name,
    }
}

/// 判断是否为媒体类型（内容本身不重要，只需显示类型标记）。
fn is_media_type(t: i32) -> bool {
    matches!(t, 3 | 34 | 43 | 47)
}

/// 解码媒体类消息的显示内容。
fn decode_media_content(msg_type: i32, raw_content: &str) -> String {
    let type_name: MessageType = msg_type.into();
    match msg_type {
        34 => {
            let dur = extract_voice_duration(raw_content);
            format!("[语音{}]", dur)
        }
        47 => {
            media::parse_sticker_info(raw_content).display()
        }
        43 => {
            media::parse_video_info(raw_content).display()
        }
        3 => {
            media::parse_image_info(raw_content).display()
        }
        _ => format!("[{}]", type_name),
    }
}

/// 解码需要解析 content 字段的消息类型。
fn decode_content_by_type(msg_type: i32, content: &str) -> String {
    match msg_type {
        49 => decode_link_content(content),
        48 => decode_location_content(content),
        50 => decode_call_content(content),
        62 => decode_file_content(content),
        419430449 => decode_music_content(content),
        436207665 | 536870918 => format!("[{}]", MessageType::from(msg_type)),
        _ => content.to_string(), // 文本或其他直接显示
    }
}

/// 对 type 49（链接/卡片）解码。尝试解析 XML 提取标题、描述、URL。
fn decode_link_content(content: &str) -> String {
    if !content.trim_start().starts_with('<') {
        if content.len() > 200 {
            return format!("[链接] {}", &content[..200]);
        }
        return format!("[链接] {}", content);
    }

    let sub_type = extract_xml_tag_int(content, "type").unwrap_or(0);

    match sub_type {
        5 => {
            media::parse_link_info(content)
                .as_ref()
                .map(media::LinkInfo::display)
                .unwrap_or_else(|| "[链接]".to_string())
        }
        33 => {
            media::parse_mini_program_info(content)
                .as_ref()
                .map(media::MiniProgramInfo::display)
                .unwrap_or_else(|| "[小程序]".to_string())
        }
        3 => {
            // 音乐分享
            let title = extract_xml_tag(content, "title").unwrap_or_else(|| "未知歌曲".to_string());
            format!("[音乐] {}", title)
        }
        6 => {
            // 文件
            let name = extract_xml_tag(content, "title").unwrap_or_else(|| "未知文件".to_string());
            format!("[文件] {}", name)
        }
        51 => {
            // 引用/聊天记录
            let title = extract_xml_tag(content, "title").unwrap_or_else(|| "聊天记录".to_string());
            format!("[引用: {}]", title)
        }
        _ => {
            let title = extract_xml_tag(content, "title").unwrap_or_default();
            let desc = extract_xml_tag(content, "des").unwrap_or_default();
            let title = if !title.is_empty() { title } else { "未知卡片".to_string() };
            if !desc.is_empty() {
                format!("[卡片] {} - {}", title, desc)
            } else {
                format!("[卡片] {}", title)
            }
        }
    }
}

/// 解码位置消息（type 48）。
/// content 可能为 XML，包含 label/poiname 信息。
fn decode_location_content(content: &str) -> String {
    if !content.trim_start().starts_with('<') {
        return format!("[位置] {}", content);
    }

    let label = extract_xml_attr(content, "label").or_else(|| extract_xml_tag(content, "label"));
    let poiname = extract_xml_attr(content, "poiname").or_else(|| extract_xml_tag(content, "poiname"));

    let location_name = poiname.as_deref().or(label.as_deref()).unwrap_or("未知位置");
    format!("[位置] {}", location_name)
}

/// 解码通话消息（type 50）。
/// content 可能包含通话时长。
fn decode_call_content(content: &str) -> String {
    // 尝试提取时长（以秒为单位）
    if let Some(dur_secs) = extract_xml_tag_int(content, "duration").or_else(|| {
        // 尝试从原内容正则式提取数字
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

/// 解码文件消息（type 62）。
/// content 可能包含文件名。
fn decode_file_content(content: &str) -> String {
    // 文件名可能在 content 中
    let name = if content.contains('/') {
        content.rsplit('/').next().unwrap_or(content)
    } else if !content.is_empty() {
        content
    } else {
        return "[文件]".to_string();
    };

    // 限制文件名长度
    if name.len() > 80 {
        format!("[文件] {}...", &name[..77])
    } else {
        format!("[文件] {}", name)
    }
}

/// 解码音乐消息（type 419430449）。
fn decode_music_content(content: &str) -> String {
    if !content.is_empty() {
        format!("[音乐] {}", content)
    } else {
        "[音乐]".to_string()
    }
}

/// 提取语音消息中的时长（秒）。
fn extract_voice_duration(content: &str) -> String {
    // 尝试提取 XML duration 字段或数字
    let dur = extract_xml_tag_int(content, "voicelength")
        .or_else(|| extract_xml_tag_int(content, "duration"))
        .or_else(|| {
            content.split(|c: char| !c.is_ascii_digit())
                .find_map(|s| s.parse::<i64>().ok())
        });
    match dur {
        Some(d) if d > 0 => format!(" {}秒", d),
        _ => String::new(),
    }
}

/// 从 XML 字符串中提取指定标签的文本内容。
/// 只处理 `<tag>text</tag>`（无属性）格式。
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
    let value = rest[..value_end].trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

/// 从 XML 字符串中提取指定标签的整数内容。
pub(crate) fn extract_xml_tag_int(xml: &str, tag: &str) -> Option<i64> {
    let text = extract_xml_tag(xml, tag)?;
    text.parse::<i64>().ok()
}

/// 从 XML 自闭合标签中提取属性值（如 `<msg label="xxx" poiname="yyy"/>`）。
pub(crate) fn extract_xml_attr(xml: &str, attr: &str) -> Option<String> {
    let pattern = format!(r#"{}=""#, attr);
    let start = xml.find(&pattern)?;
    let value_start = start + pattern.len();
    if value_start >= xml.len() {
        return None;
    }
    let rest = &xml[value_start..];
    if !rest.starts_with('"') {
        return None;
    }
    let rest = &rest[1..]; // skip opening "
    let end = rest.find('"')?;
    let value = rest[..end].to_string();
    if value.is_empty() { None } else { Some(value) }
}

/// Try to ZLIB-decompress a byte slice into a UTF-8 string.
/// Used when WCDB_CT_message_content = 4 (compressed content).
pub fn try_decompress(raw: &[u8]) -> Option<String> {
    let mut decoder = ZlibDecoder::new(raw);
    let mut s = String::new();
    decoder.read_to_string(&mut s).ok()?;
    if s.is_empty() { None } else { Some(s) }
}

/// Parse sender tgid from message content ("tgid_xxx:\nmessage" or "tgid_xxx: message").
/// Returns (sender_id, clean_content).
pub fn parse_sender_from_content(content: &str) -> (Option<&str>, &str) {
    for (i, c) in content.char_indices() {
        if c != ':' {
            continue;
        }
        if i == 0 {
            break;
        }
        let prefix = &content[..i];
        // Check if prefix looks like a Telegram ID
        let is_id = prefix.starts_with("tgid_")
            || prefix.starts_with("gh_")
            || prefix.contains('@')
            || prefix.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if is_id && prefix.len() >= 3 {
            let after = &content[i + 1..];
            let after = after.trim_start_matches([' ', '\n']);
            return (Some(prefix), after);
        }
        break; // first colon doesn't match, stop
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
        assert_eq!(MessageType::Voice.to_string(), "语音");
        assert_eq!(MessageType::Sticker.to_string(), "表情");
        assert_eq!(MessageType::Video.to_string(), "视频");
        assert_eq!(MessageType::Link.to_string(), "链接/文件/小程序");
        assert_eq!(MessageType::System.to_string(), "系统提示");
        assert_eq!(MessageType::RedEnvelope.to_string(), "红包");
        assert_eq!(MessageType::Transfer.to_string(), "转账");
        assert_eq!(MessageType::Unknown(999).to_string(), "未知(999)");
    }

    #[test]
    fn test_message_type_from_i32() {
        assert_eq!(MessageType::from(1), MessageType::Text);
        assert_eq!(MessageType::from(3), MessageType::Image);
        assert_eq!(MessageType::from(34), MessageType::Voice);
        assert_eq!(MessageType::from(43), MessageType::Video);
        assert_eq!(MessageType::from(47), MessageType::Sticker);
        assert_eq!(MessageType::from(48), MessageType::Location);
        assert_eq!(MessageType::from(49), MessageType::Link);
        assert_eq!(MessageType::from(50), MessageType::Call);
        assert_eq!(MessageType::from(62), MessageType::File);
        assert_eq!(MessageType::from(10000), MessageType::System);
        assert_eq!(MessageType::from(10002), MessageType::Revoke);
        assert_eq!(MessageType::from(436207665), MessageType::RedEnvelope);
        assert_eq!(MessageType::from(536870918), MessageType::Transfer);
        assert_eq!(MessageType::from(419430449), MessageType::Music);
        assert_eq!(MessageType::from(42), MessageType::Unknown(42));
    }

    #[test]
    fn test_decode_text_message() {
        let decoded = decode_message(1, "Hello World", "Alice", None, |id| id.to_string());
        assert_eq!(decoded.msg_type, MessageType::Text);
        assert_eq!(decoded.content, "Hello World");
        assert_eq!(decoded.display_name, "Alice");
    }

    #[test]
    fn test_decode_image_message() {
        let decoded = decode_message(3, "some_path", "Alice", None, |id| id.to_string());
        assert_eq!(decoded.msg_type, MessageType::Image);
        assert!(decoded.content.contains("图片"));
        assert_eq!(decoded.display_name, "Alice");
    }

    #[test]
    fn test_decode_voice_message() {
        let decoded = decode_message(34, "", "Alice", None, |id| id.to_string());
        assert_eq!(decoded.msg_type, MessageType::Voice);
        assert!(decoded.content.contains("语音"));
    }

    #[test]
    fn test_decode_system_message() {
        let decoded = decode_message(10000, "你添加了 xxx 为好友", "Alice", None, |id| id.to_string());
        assert_eq!(decoded.msg_type, MessageType::System);
        assert_eq!(decoded.content, "你添加了 xxx 为好友");
        assert_eq!(decoded.display_name, "系统");
    }

    #[test]
    fn test_decode_red_envelope() {
        let decoded = decode_message(436207665, "", "Alice", None, |id| id.to_string());
        assert_eq!(decoded.msg_type, MessageType::RedEnvelope);
        assert!(decoded.content.contains("红包"));
    }

    #[test]
    fn test_decode_transfer() {
        let decoded = decode_message(536870918, "", "Alice", None, |id| id.to_string());
        assert_eq!(decoded.msg_type, MessageType::Transfer);
        assert!(decoded.content.contains("转账"));
    }

    #[test]
    fn test_decode_compressed_content() {
        // wcdb_ct=4 is handled at DB layer (decompressed before decode_message).
        // If decompression fails or wasn't needed, content passes through normally.
        let decoded = decode_message(1, "some content", "Alice", Some(4), |id| id.to_string());
        assert_eq!(decoded.content, "some content");
    }

    #[test]
    fn test_decode_group_chat_sender_resolution() {
        // Group chat message with tgid prefix
        let decoded = decode_message(
            1,
            "tgid_abc123:\nHello everyone",
            "Group Chat",
            None,
            |id| match id {
                "tgid_abc123" => "Bob".to_string(),
                _ => id.to_string(),
            },
        );
        assert_eq!(decoded.content, "Hello everyone");
        assert_eq!(decoded.display_name, "Bob");
    }

    #[test]
    fn test_parse_sender_from_content() {
        let (id, clean) = parse_sender_from_content("tgid_abc123:\nHello");
        assert_eq!(id, Some("tgid_abc123"));
        assert_eq!(clean, "Hello");

        let (id, clean) = parse_sender_from_content("normal text message");
        assert_eq!(id, None);
        assert_eq!(clean, "normal text message");

        // gh_ prefix (official account)
        let (id, clean) = parse_sender_from_content("gh_xyz789:\nNews");
        assert_eq!(id, Some("gh_xyz789"));
        assert_eq!(clean, "News");

        // chatroom
        let (id, clean) = parse_sender_from_content("123@chatroom:\nGroup msg");
        assert_eq!(id, Some("123@chatroom"));
        assert_eq!(clean, "Group msg");
    }

    #[test]
    fn test_extract_xml_tag_simple() {
        let xml = "<msg><title>Test Title</title><url>https://example.com</url></msg>";
        assert_eq!(extract_xml_tag(xml, "title"), Some("Test Title".to_string()));
        assert_eq!(extract_xml_tag(xml, "url"), Some("https://example.com".to_string()));
        assert_eq!(extract_xml_tag(xml, "nonexist"), None);
    }

    #[test]
    fn test_decode_link_content() {
        let xml = r#"<?xml version="1.0"?><msg><appmsg><title>新闻标题</title><des>摘要</des><url>https://example.com/news</url><type>5</type></appmsg></msg>"#;
        let decoded = decode_message(49, xml, "Alice", None, |id| id.to_string());
        assert_eq!(decoded.msg_type, MessageType::Link);
        assert!(decoded.content.contains("链接"));
        assert!(decoded.content.contains("新闻标题"));
    }

    #[test]
    fn test_decode_mini_program() {
        let xml = r#"<?xml?><msg><appmsg><title>小程序名称</title><type>33</type><appname>某App</appname></appmsg></msg>"#;
        let decoded = decode_message(49, xml, "Alice", None, |id| id.to_string());
        assert!(decoded.content.contains("小程序"));
        assert!(decoded.content.contains("小程序名称"));
    }
}
