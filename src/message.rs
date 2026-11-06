use crate::dictionary;
use crate::media;
use crate::media_pb;
use crate::time;
use flate2::read::ZlibDecoder;
use std::cell::RefCell;
use std::fmt;
use std::io::Read;

/// Telegram 消息类型枚举。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MessageType {
    Text,         // 1
    Image,        // 3
    Voice,        // 34
    Mail,         // 35
    Sticker,      // 47
    Video,        // 43
    Link,         // 49 — 链接/文件/小程序/音乐
    System,       // 10000
    Notification, // 11000
    RedEnvelope,  // 436207665
    Transfer,     // 536870918
    Location,     // 48
    ContactCard,  // 42
    File,         // 62
    Call,         // 50
    ExternalCard, // 66
    Music,        // 419430449
    Revoke,       // 10002 撤回消息
    Unknown(i32), // 其他
}

impl fmt::Display for MessageType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            MessageType::Text => "文本",
            MessageType::Image => "图片",
            MessageType::Voice => "语音",
            MessageType::Mail => "邮件",
            MessageType::Sticker => "表情",
            MessageType::Video => "视频",
            MessageType::Link => "链接/文件/小程序",
            MessageType::System => "系统提示",
            MessageType::Notification => "通知",
            MessageType::RedEnvelope => "红包",
            MessageType::Transfer => "转账",
            MessageType::Location => "位置",
            MessageType::ContactCard => "名片",
            MessageType::File => "文件",
            MessageType::Call => "语音/视频通话",
            MessageType::ExternalCard => "外部联系人",
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
            35 => MessageType::Mail,
            42 => MessageType::ContactCard,
            43 => MessageType::Video,
            47 => MessageType::Sticker,
            48 => MessageType::Location,
            49 => MessageType::Link,
            50 => MessageType::Call,
            62 => MessageType::File,
            10000 => MessageType::System,
            11000 => MessageType::Notification,
            10002 => MessageType::Revoke,
            419430449 => MessageType::Music,
            66 => MessageType::ExternalCard,
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
    /// 发送者显示名（群聊中会从账号 ID 解析为昵称）。
    pub display_name: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DecodeContext {
    pub time_bucket: time::MessageTimeBucket,
    pub voice_id: Option<i64>,
}

/// 解码消息。
///
/// # Arguments
/// - `msg_type` — 原始消息类型值
/// - `raw_content` — 原始消息内容（可能为 TEXT 或 BLOB 转 String）
/// - `session_display_name` — 会话的显示名（私聊即对方昵称，群聊即群名）
/// - `compression_marker` — message compression marker from the source row
/// - `packed_info` — packed media metadata bytes
/// - `resolve_display_name` — 将账号 ID 解析为显示名的函数
pub fn decode_message(
    msg_type: i32,
    raw_content: &str,
    session_display_name: &str,
    _compression_marker: Option<i64>,
    packed_info: &[u8],
    resolve_display_name: impl Fn(&str) -> String,
) -> DecodedMessage {
    decode_message_with_time_bucket(
        msg_type,
        raw_content,
        session_display_name,
        _compression_marker,
        packed_info,
        time::MessageTimeBucket::Minute(1),
        resolve_display_name,
    )
}

pub(crate) fn decode_message_with_time_bucket(
    msg_type: i32,
    raw_content: &str,
    session_display_name: &str,
    _compression_marker: Option<i64>,
    packed_info: &[u8],
    time_bucket: time::MessageTimeBucket,
    resolve_display_name: impl Fn(&str) -> String,
) -> DecodedMessage {
    decode_message_with_context(
        msg_type,
        raw_content,
        session_display_name,
        _compression_marker,
        packed_info,
        DecodeContext {
            time_bucket,
            voice_id: None,
        },
        resolve_display_name,
    )
}

pub(crate) fn decode_message_with_context(
    msg_type: i32,
    raw_content: &str,
    session_display_name: &str,
    _compression_marker: Option<i64>,
    packed_info: &[u8],
    context: DecodeContext,
    resolve_display_name: impl Fn(&str) -> String,
) -> DecodedMessage {
    let msg_type_enum: MessageType = msg_type.into();

    if msg_type == 10000 || msg_type == 10002 {
        let (_, clean_content) = parse_sender_from_content(raw_content);
        return DecodedMessage {
            msg_type: msg_type_enum,
            content: sanitize_decoded_content(decode_system_content(clean_content, msg_type_enum)),
            display_name: "系统".to_string(),
        };
    }

    if is_media_type(msg_type) {
        let (sender_id, clean_content) = parse_sender_from_content(raw_content);
        let display_name = match sender_id {
            Some(id) => resolve_display_name(id),
            None => session_display_name.to_string(),
        };
        let content = decode_media_content(msg_type, clean_content, packed_info, context.voice_id);
        return DecodedMessage {
            msg_type: msg_type_enum,
            content: sanitize_decoded_content(content),
            display_name,
        };
    }

    if raw_content.is_empty() && msg_type != 1 {
        return DecodedMessage {
            msg_type: msg_type_enum,
            content: sanitize_decoded_content(format!("[{}]", msg_type_enum)),
            display_name: session_display_name.to_string(),
        };
    }

    let (sender_id, clean_content) = parse_sender_from_content(raw_content);
    let display_name = match sender_id {
        Some(id) => resolve_display_name(id),
        None => session_display_name.to_string(),
    };
    let content = decode_content_by_type(
        msg_type,
        clean_content,
        packed_info,
        context.time_bucket,
        &resolve_display_name,
    );

    DecodedMessage {
        msg_type: msg_type_enum,
        content: sanitize_decoded_content(content),
        display_name,
    }
}

fn is_media_type(t: i32) -> bool {
    matches!(t, 3 | 34 | 43 | 47)
}

/// 解码媒体类消息：优先使用 packed media metadata 的 protobuf 元信息，回退到 XML 解析。
fn decode_media_content(
    msg_type: i32,
    raw_content: &str,
    packed_info: &[u8],
    voice_id: Option<i64>,
) -> String {
    match msg_type {
        34 => decode_voice_content(raw_content, packed_info, voice_id),
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

fn decode_voice_content(raw_content: &str, packed_info: &[u8], voice_id: Option<i64>) -> String {
    let duration_secs = extract_voice_duration_secs(raw_content);
    let audio_text = extract_audio_text(packed_info);

    if let Some(id) = voice_id.filter(|id| *id > 0) {
        let tag = voice_tag(id, duration_secs);
        if let Some(text) = audio_text {
            return format!("{} {}", tag, text);
        }
        return tag;
    }

    if let Some(text) = audio_text {
        return format!("[语音] {}", text);
    }
    format!("[语音{}]", format_voice_duration_zh(duration_secs))
}

fn voice_tag(id: i64, duration_secs: Option<i64>) -> String {
    match duration_secs.filter(|duration| *duration > 0) {
        Some(duration) => format!("[voice:{}:{}s]", id, duration),
        None => format!("[voice:{}]", id),
    }
}

fn extract_image_display(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }
    if let Some(v2) = media_pb::parse_img2(data) {
        if let Some(img) = v2.image {
            if media_pb::image_identifier(&img).is_some() {
                return Some(media_pb::display_image(&img));
            }
        }
    }
    if let Some(v1) = media_pb::parse_img(data) {
        if !v1.filename.is_empty() {
            return Some(media::image_tag(Some(&v1.filename)));
        }
    }
    None
}

fn extract_video_display(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }
    if let Some(v2) = media_pb::parse_img2(data) {
        if let Some(vid) = v2.video {
            return Some(media_pb::display_video(&vid));
        }
    }
    None
}

fn extract_audio_text(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }
    if let Some(v2) = media_pb::parse_img2(data) {
        if let Some(audio) = v2.audio {
            if !audio.audio_text.is_empty() {
                return Some(audio.audio_text);
            }
        }
    }
    None
}

fn decode_content_by_type(
    msg_type: i32,
    content: &str,
    packed_info: &[u8],
    time_bucket: time::MessageTimeBucket,
    resolve_display_name: &impl Fn(&str) -> String,
) -> String {
    match msg_type {
        1 => decode_text_content(content, time_bucket, resolve_display_name),
        49 => decode_link_content(content, packed_info, time_bucket, resolve_display_name),
        35 => decode_mail_content(content),
        42 => decode_contact_card_content(content, "名片"),
        48 => decode_location_content(content),
        50 => decode_call_content(content),
        62 => decode_file_content(content, packed_info),
        66 => decode_contact_card_content(content, "外部联系人"),
        419430449 => decode_music_content(content),
        436207665 | 536870918 => format!("[{}]", MessageType::from(msg_type)),
        _ => decode_text_content(content, time_bucket, resolve_display_name),
    }
}

fn decode_text_content(
    content: &str,
    time_bucket: time::MessageTimeBucket,
    resolve_display_name: &impl Fn(&str) -> String,
) -> String {
    decode_text_with_internal_xml(content, time_bucket, resolve_display_name)
        .unwrap_or_else(|| content.to_string())
}

fn decode_text_with_internal_xml(
    content: &str,
    time_bucket: time::MessageTimeBucket,
    resolve_display_name: &impl Fn(&str) -> String,
) -> Option<String> {
    let normalized = strip_display_prefix(content);
    if let Some(decoded) =
        decode_internal_xml_fragment(normalized, time_bucket, resolve_display_name)
    {
        return Some(decoded);
    }

    let start = find_internal_xml_start(content)?;
    let prefix = content[..start].trim_end();
    let fragment = &content[start..];
    let decoded = decode_internal_xml_fragment(fragment, time_bucket, resolve_display_name)?;
    if prefix.is_empty() {
        Some(decoded)
    } else {
        Some(format!("{}\n{}", prefix, decoded))
    }
}

fn decode_internal_xml_fragment(
    fragment: &str,
    time_bucket: time::MessageTimeBucket,
    resolve_display_name: &impl Fn(&str) -> String,
) -> Option<String> {
    let fragment = strip_display_prefix(fragment).trim_start();
    let xml = if fragment.starts_with("<?xml") {
        let start = fragment.find("<msg").or_else(|| fragment.find("<sysmsg"))?;
        &fragment[start..]
    } else {
        fragment
    };

    if xml.starts_with("&lt;") {
        return Some("[消息]".to_string());
    }

    if !xml.starts_with("<msg") && !xml.starts_with("<sysmsg") {
        return None;
    }

    if xml.contains("<appmsg") {
        return Some(decode_link_content(
            xml,
            &[],
            time_bucket,
            resolve_display_name,
        ));
    }
    if xml.contains("<img") {
        return Some(media::parse_image_info(xml).display());
    }
    if xml.contains("<videomsg") || xml.contains("<video") {
        return Some(media::parse_video_info(xml).display());
    }
    if xml.contains("<emoji") {
        return Some(media::parse_sticker_info(xml).display());
    }
    if xml.starts_with("<sysmsg") {
        return Some(decode_system_content(xml, MessageType::System));
    }

    Some(decode_generic_xml_fragment(xml))
}

fn strip_display_prefix(content: &str) -> &str {
    let trimmed = content.trim_start();
    for prefix in ["[链接]", "[卡片]", "[小程序]", "[文件]", "[视频]"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.trim_start();
        }
    }
    trimmed
}

/// Markers that open an embedded XML fragment. Each starts with `<` or `&`.
const XML_START_MARKERS: &[&str] = &[
    "<?xml",
    "<msg",
    "<sysmsg",
    "&lt;?xml",
    "&lt;msg",
    "&lt;sysmsg",
];

/// Markers that show content still carries raw XML. Each starts with `<` or `&`.
const XML_CONTENT_MARKERS: &[&str] = &[
    "<?xml",
    "<msg",
    "<appmsg",
    "<sysmsg",
    "<recordinfo",
    "<datalist",
    "<dataitem",
    "<img",
    "<videomsg",
    "<voicemsg",
    "<emoji",
    "<location",
    "<pushmail",
    "&lt;?xml",
    "&lt;msg",
    "&lt;appmsg",
    "&lt;sysmsg",
    "&lt;recordinfo",
    "&lt;dataitem",
];

/// Every marker starts with `<` or `&`, so a single pass over those bytes finds
/// the first offset where any marker matches, the same answer as taking the
/// minimum of a separate `find` per marker.
fn first_marker_at(content: &str, needles: &[&str]) -> Option<usize> {
    let bytes = content.as_bytes();
    memchr::memchr2_iter(b'<', b'&', bytes).find(|&i| {
        needles
            .iter()
            .any(|needle| bytes[i..].starts_with(needle.as_bytes()))
    })
}

fn find_internal_xml_start(content: &str) -> Option<usize> {
    first_marker_at(content, XML_START_MARKERS)
}

fn contains_internal_xml_marker(content: &str) -> bool {
    first_marker_at(content, XML_CONTENT_MARKERS).is_some()
}

fn sanitize_decoded_content(content: String) -> String {
    if !contains_internal_xml_marker(&content) {
        return content;
    }

    let marker_start = find_internal_xml_start(&content)
        .or_else(|| content.find("<img"))
        .or_else(|| content.find("<emoji"))
        .or_else(|| content.find("<appmsg"))
        .or_else(|| content.find('<'))
        .unwrap_or(content.len());
    let prefix = content[..marker_start].trim_end();
    let marker = &content[marker_start..];
    let replacement = if marker.contains("<emoji") {
        media::parse_sticker_info(marker).display()
    } else if marker.contains("<img") || marker.contains("&lt;img") {
        "[img]".to_string()
    } else if marker.contains("<videomsg") || marker.contains("<video") {
        "[视频]".to_string()
    } else {
        "[消息]".to_string()
    };

    if prefix.is_empty() {
        replacement
    } else if prefix == ">" {
        format!("> {}", replacement)
    } else {
        format!("{}\n{}", prefix, replacement)
    }
}

fn safe_xml_tag(xml: &str, tag: &str) -> Option<String> {
    crate::media::extract_xml_tag(xml, tag).and_then(clean_summary_field)
}

fn clean_summary_field(value: String) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || contains_internal_xml_marker(value) {
        None
    } else {
        Some(truncate_display(value, 200))
    }
}

fn decode_generic_xml_fragment(xml: &str) -> String {
    let title = safe_xml_tag(xml, "title");
    let desc = safe_xml_tag(xml, "des")
        .or_else(|| safe_xml_tag(xml, "desc"))
        .or_else(|| safe_xml_tag(xml, "content"));

    match (title, desc) {
        (Some(title), Some(desc)) => format!("[消息] {} - {}", title, desc),
        (Some(title), None) => format!("[消息] {}", title),
        (None, Some(desc)) => format!("[消息] {}", desc),
        (None, None) => "[消息]".to_string(),
    }
}

fn decode_system_content(content: &str, msg_type: MessageType) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return format!("[{}]", msg_type);
    }

    let sysmsg_type = crate::media::extract_xml_attr(trimmed, "type").unwrap_or_default();
    if sysmsg_type == "revokemsg" || matches!(msg_type, MessageType::Revoke) {
        if let Some(revoke_content) = crate::media::extract_xml_tag(trimmed, "content") {
            return format!("[撤回] {}", revoke_content);
        }
        return "[撤回]".to_string();
    }

    if trimmed.starts_with('<') {
        if let Some(text) = safe_xml_tag(trimmed, "content") {
            return text;
        }
        let generic = decode_generic_xml_fragment(trimmed);
        if generic != "[消息]" {
            return generic;
        }
        return format!("[{}]", msg_type);
    }

    content.to_string()
}

fn decode_link_content(
    content: &str,
    packed_info: &[u8],
    time_bucket: time::MessageTimeBucket,
    resolve_display_name: &impl Fn(&str) -> String,
) -> String {
    if !content.trim_start().starts_with('<') {
        if content.len() > 200 {
            return format!("[链接] {}", truncate_display(content, 200));
        }
        return format!("[链接] {}", content);
    }

    if media::is_pat_app_message(content) {
        return decode_pat_content(content);
    }

    let sub_type = crate::media::extract_xml_tag_int(content, "type").unwrap_or(0);
    match sub_type {
        5 => decode_app_link_content(content),
        33 => media::parse_mini_program_info(content)
            .as_ref()
            .map(media::MiniProgramInfo::display)
            .unwrap_or_else(|| "[小程序]".to_string()),
        36 => media::parse_mini_program_info(content)
            .as_ref()
            .map(media::MiniProgramInfo::display)
            .unwrap_or_else(|| decode_app_card_fallback(sub_type, "小程序", content)),
        4 => decode_app_card_fallback(sub_type, "视频", content),
        3 => {
            let title = crate::media::extract_xml_tag(content, "title")
                .unwrap_or_else(|| "未知歌曲".to_string());
            format!("[音乐] {}", title)
        }
        6 => decode_file_content(content, packed_info),
        62 => decode_file_content(content, packed_info),
        19 => decode_chat_history_content(content, time_bucket),
        57 => decode_quote_content(content, time_bucket, resolve_display_name),
        87 => decode_group_notice_content(content)
            .unwrap_or_else(|| decode_app_card_fallback(sub_type, "卡片", content)),
        2000 => decode_app_card_fallback(sub_type, "转账", content),
        2001 => decode_app_card_fallback(sub_type, "红包", content),
        51 => decode_short_video_or_legacy_reference_content(content),
        _ => decode_app_card_fallback(sub_type, "卡片", content),
    }
}

fn decode_pat_content(content: &str) -> String {
    let title = safe_xml_tag(content, "title")
        .or_else(|| safe_xml_tag(content, "template"))
        .unwrap_or_default();
    if title.is_empty() {
        "[拍一拍]".to_string()
    } else {
        format!("[拍一拍] {}", title)
    }
}

fn decode_app_link_content(content: &str) -> String {
    media::parse_link_info(content)
        .as_ref()
        .map(display_link_info)
        .unwrap_or_else(|| "[链接]".to_string())
}

fn decode_group_notice_content(content: &str) -> Option<String> {
    let announcement = crate::media::extract_xml_tag(content, "textannouncement")?;
    let announcement = media::strip_cdata(&announcement).trim();
    (!announcement.is_empty()).then(|| format!("[群公告] {}", announcement))
}

fn display_link_info(info: &media::LinkInfo) -> String {
    let display = info.display();
    let Some(url) = clean_url_field(&info.url) else {
        return display;
    };
    if display.contains(&url) {
        display
    } else {
        append_url_line(display, Some(url))
    }
}

fn decode_short_video_or_legacy_reference_content(content: &str) -> String {
    if let Some(info) = media::parse_short_video_feed_info(content) {
        return display_short_video_feed_info(&info);
    }

    let title =
        crate::media::extract_xml_tag(content, "title").unwrap_or_else(|| "聊天记录".to_string());
    format!("[引用: {}]", title)
}

fn display_short_video_feed_info(info: &media::ShortVideoFeedInfo) -> String {
    let label = if info.is_video() {
        "视频"
    } else {
        "视频号"
    };
    let title = truncate_display(info.title.trim(), 200);
    let desc = truncate_display(info.description.trim(), 200);
    let nickname = info.nickname.trim();

    let mut summary = if !title.is_empty() && !desc.is_empty() && title != desc {
        format!("{} - {}", title, desc)
    } else if !title.is_empty() {
        title
    } else if !desc.is_empty() {
        desc
    } else if !nickname.is_empty() {
        format!("@{}", nickname)
    } else {
        "视频号内容".to_string()
    };

    if !nickname.is_empty() && !summary.contains(nickname) {
        summary.push_str(" - @");
        summary.push_str(nickname);
    }

    format!("[{}] {}", label, summary)
}

fn decode_app_card_fallback(sub_type: i64, label: &str, content: &str) -> String {
    let title = safe_xml_tag(content, "title").unwrap_or_default();
    let desc = safe_xml_tag(content, "des").unwrap_or_default();
    let label = if label == "卡片" {
        if sub_type > 0 {
            format!("{}:{}", label, sub_type)
        } else {
            label.to_string()
        }
    } else {
        label.to_string()
    };
    let title = if !title.is_empty() {
        title
    } else if label.starts_with("卡片:") {
        "未知卡片".to_string()
    } else {
        format!("未知{}", label)
    };
    let display = if !desc.is_empty() {
        format!("[{}] {} - {}", label, title, desc)
    } else {
        format!("[{}] {}", label, title)
    };
    append_url_line(display, safe_url_tag(content))
}

fn append_url_line(mut display: String, url: Option<String>) -> String {
    if let Some(url) = url {
        display.push('\n');
        display.push_str("  ");
        display.push_str(&url);
    }
    display
}

fn safe_url_tag(xml: &str) -> Option<String> {
    crate::media::extract_xml_tag(xml, "url").and_then(|url| clean_url_field(&url))
}

fn clean_url_field(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.contains('<')
        || value.contains('>')
        || contains_internal_xml_marker(value)
    {
        None
    } else {
        Some(value.to_string())
    }
}

#[derive(Debug, PartialEq)]
struct ChatHistoryItem {
    source_name: String,
    source_time: Option<String>,
    source_timestamp: Option<i64>,
    body: String,
}

/// Deepest forwarded record expanded inside another; deeper ones render as a
/// `[记录]` tag. Each level is a strictly smaller slice, so recursion always
/// ends; the bound only keeps hostile input from exhausting the stack.
const MAX_CHAT_HISTORY_DEPTH: usize = 32;

fn decode_chat_history_content(content: &str, time_bucket: time::MessageTimeBucket) -> String {
    let title = crate::media::extract_xml_tag(content, "title")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "聊天记录".to_string());
    let items = crate::media::extract_xml_element(content, "recorditem")
        .map(|record| parse_chat_history_items(media::strip_cdata(&record), time_bucket, 0))
        .unwrap_or_default();
    let summary = || crate::media::extract_xml_tag(content, "des");
    render_chat_history(&title, items, summary, time_bucket)
}

/// Expands a `<recordinfo>` document nested in a forwarded item. Its own
/// title and summary sit outside `<datalist>`, so inner records cannot
/// supply them.
fn decode_nested_chat_history(
    record: &str,
    time_bucket: time::MessageTimeBucket,
    depth: usize,
) -> String {
    let list = crate::media::find_xml_element(record, "datalist");
    let (head, tail) = match &list {
        Some(list) => (&record[..list.start], &record[list.end..]),
        None => (record, ""),
    };
    let own = |tag| {
        crate::media::extract_xml_tag(head, tag)
            .or_else(|| crate::media::extract_xml_tag(tail, tag))
    };
    let title = own("title").unwrap_or_else(|| "聊天记录".to_string());
    let items = parse_chat_history_items(record, time_bucket, depth);
    render_chat_history(&title, items, || own("desc"), time_bucket)
}

fn render_chat_history(
    title: &str,
    items: Vec<ChatHistoryItem>,
    summary: impl FnOnce() -> Option<String>,
    time_bucket: time::MessageTimeBucket,
) -> String {
    if items.is_empty() {
        return match summary() {
            Some(desc) => format!("[聊天记录] {} - {}", title, desc),
            None => format!("[聊天记录] {}", title),
        };
    }
    let mut out = format!("[聊天记录] {} ({}条)", title, items.len());
    render_chat_history_items(&mut out, &items, time_bucket);
    out
}

/// Items of the record's top-level `<datalist>`; items of nested records stay
/// inside their parent item. Records without a `<datalist>` are scanned whole.
fn parse_chat_history_items(
    record: &str,
    time_bucket: time::MessageTimeBucket,
    depth: usize,
) -> Vec<ChatHistoryItem> {
    let mut rest = match crate::media::find_xml_element(record, "datalist") {
        Some(list) => &record[list.body],
        None => record,
    };
    let mut items = Vec::new();
    while let Some(element) = crate::media::find_xml_element(rest, "dataitem") {
        let item_xml = &rest[element.start..element.end];
        if let Some(item) = parse_chat_history_item(item_xml, time_bucket, depth) {
            items.push(item);
        }
        rest = &rest[element.end..];
    }
    items
}

fn parse_chat_history_item(
    item_xml: &str,
    time_bucket: time::MessageTimeBucket,
    depth: usize,
) -> Option<ChatHistoryItem> {
    // A forwarded record nested in this item carries its own senders, times
    // and titles; read this item's fields from around it.
    let nested = crate::media::find_xml_element(item_xml, "recordxml")
        .or_else(|| crate::media::find_xml_element(item_xml, "recorditem"));
    let (head, tail) = match &nested {
        Some(nested) => (&item_xml[..nested.start], &item_xml[nested.end..]),
        None => (item_xml, ""),
    };
    let own = |tag| {
        crate::media::extract_xml_tag(head, tag)
            .or_else(|| crate::media::extract_xml_tag(tail, tag))
    };

    let source_name = own("sourcename")
        .or_else(|| own("sourcedisplayname"))
        .unwrap_or_else(|| "未知".to_string());
    let source_time = own("sourcetime");
    let source_timestamp = source_time
        .as_deref()
        .and_then(time::parse_local_timestamp_minutes);
    let body = match nested {
        Some(nested) if depth < MAX_CHAT_HISTORY_DEPTH => {
            let record = media::strip_cdata(&item_xml[nested.body]);
            decode_nested_chat_history(record, time_bucket, depth + 1)
        }
        Some(_) => fallback_tag_with_title(
            "记录",
            &own("datatitle")
                .or_else(|| own("title"))
                .unwrap_or_default(),
        ),
        None => {
            let datatype = crate::media::extract_xml_attr(item_xml, "datatype")
                .and_then(|s| s.parse::<i64>().ok());
            decode_chat_history_item_body(item_xml, datatype, time_bucket, depth)
        }
    };
    if body.is_empty() {
        return None;
    }

    Some(ChatHistoryItem {
        source_name,
        source_time,
        source_timestamp,
        body,
    })
}

fn decode_chat_history_item_body(
    item_xml: &str,
    datatype: Option<i64>,
    time_bucket: time::MessageTimeBucket,
    depth: usize,
) -> String {
    let embedded = |value: &str| decode_embedded_history_text(value, time_bucket, depth);
    let desc = crate::media::extract_xml_tag(item_xml, "datadesc").unwrap_or_default();
    if !desc.is_empty() {
        return embedded(&desc);
    }

    let title = crate::media::extract_xml_tag(item_xml, "datatitle")
        .or_else(|| crate::media::extract_xml_tag(item_xml, "title"))
        .unwrap_or_default();

    match datatype {
        Some(2) => chat_history_image_tag(item_xml),
        Some(3) => fallback_tag_with_title("语音", &title),
        Some(4) | Some(15) => fallback_tag_with_title("视频", &title),
        Some(5) => fallback_tag_with_title("链接", &title),
        Some(6) => fallback_tag_with_title("位置", &title),
        Some(8) => fallback_tag_with_title("文件", &title),
        _ if !title.is_empty() => embedded(&title),
        Some(t) => format!("[记录:{}]", t),
        None => "[记录]".to_string(),
    }
}

fn decode_embedded_history_text(
    value: &str,
    time_bucket: time::MessageTimeBucket,
    depth: usize,
) -> String {
    // Some items carry a nested record as entity-escaped text.
    if depth < MAX_CHAT_HISTORY_DEPTH && value.contains("<recordinfo") {
        return decode_nested_chat_history(value, time_bucket, depth + 1);
    }
    if contains_internal_xml_marker(value) {
        decode_text_with_internal_xml(value, time::MessageTimeBucket::Minute(1), &|id| {
            id.to_string()
        })
        .unwrap_or_else(|| "[消息]".to_string())
    } else {
        value.to_string()
    }
}

fn chat_history_image_tag(item_xml: &str) -> String {
    if let Some(id) = crate::media::extract_xml_tag(item_xml, "fullmd5")
        .or_else(|| crate::media::extract_xml_tag(item_xml, "thumbfullmd5"))
        .or_else(|| crate::media::extract_xml_attr(item_xml, "dataid"))
    {
        crate::media::image_tag(Some(&id))
    } else {
        crate::media::image_tag(None)
    }
}

fn fallback_tag_with_title(label: &str, title: &str) -> String {
    if title.is_empty() {
        format!("[{}]", label)
    } else {
        format!("[{}] {}", label, title)
    }
}

fn render_chat_history_items(
    out: &mut String,
    items: &[ChatHistoryItem],
    time_bucket: time::MessageTimeBucket,
) {
    if time_bucket == time::MessageTimeBucket::PerMessage {
        for item in items {
            append_chat_history_item(out, item, true, item.source_time.as_deref());
        }
        return;
    }

    let mut last_time_label: Option<String> = None;
    let mut last_sender: Option<String> = None;

    for item in items {
        let time_label = chat_history_time_label(item, time_bucket);
        if time_label != last_time_label {
            if let Some(label) = &time_label {
                append_chat_history_line(out, &format!("[{}]", label));
            }
            last_time_label = time_label;
            last_sender = None;
        }

        let show_sender = last_sender.as_deref() != Some(item.source_name.as_str());
        append_chat_history_item(out, item, show_sender, None);
        last_sender = Some(item.source_name.clone());
    }
}

fn chat_history_time_label(
    item: &ChatHistoryItem,
    time_bucket: time::MessageTimeBucket,
) -> Option<String> {
    match time_bucket {
        time::MessageTimeBucket::PerMessage | time::MessageTimeBucket::None => None,
        _ => item
            .source_timestamp
            .map(|ts| time::format_message_time_bucket(ts, time_bucket))
            .or_else(|| item.source_time.clone()),
    }
}

fn append_chat_history_item(
    out: &mut String,
    item: &ChatHistoryItem,
    show_sender: bool,
    inline_time: Option<&str>,
) {
    let mut lines = item.body.lines();
    let Some(first) = lines.next() else {
        return;
    };

    let first_line = match (inline_time, show_sender) {
        (Some(t), true) => format!("[{}] {}: {}", t, item.source_name, first),
        (Some(t), false) => format!("[{}] {}", t, first),
        (None, true) => format!("{}: {}", item.source_name, first),
        (None, false) => format!(" {}", first),
    };
    append_chat_history_line(out, &first_line);

    for line in lines {
        let continuation = if show_sender {
            format!("  {}", line)
        } else {
            format!(" {}", line)
        };
        append_chat_history_line(out, &continuation);
    }
}

fn append_chat_history_line(out: &mut String, line: &str) {
    out.push('\n');
    out.push_str("        ");
    out.push_str(line);
}

fn decode_quote_content(
    content: &str,
    time_bucket: time::MessageTimeBucket,
    resolve_display_name: &impl Fn(&str) -> String,
) -> String {
    let reply = safe_xml_tag(content, "title").unwrap_or_default();
    let quoted = crate::media::extract_xml_tag(content, "refermsg")
        .map(|refermsg| decode_refermsg_content(&refermsg, time_bucket, resolve_display_name))
        .unwrap_or_default();

    match (quoted.is_empty(), reply.is_empty()) {
        (false, false) => format!("> {}\n {}", quoted, reply),
        (false, true) => format!("> {}", quoted),
        (true, false) => reply,
        (true, true) => "[引用]".to_string(),
    }
}

fn decode_refermsg_content(
    refermsg: &str,
    time_bucket: time::MessageTimeBucket,
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
            1 => decode_text_content(clean_content, time_bucket, resolve_display_name),
            t if is_media_type(t) => decode_media_content(t, clean_content, &[], None),
            49 => {
                let normalized = strip_display_prefix(clean_content);
                if normalized.trim_start().starts_with('<') {
                    decode_link_content(normalized, &[], time_bucket, resolve_display_name)
                } else {
                    decode_text_content(clean_content, time_bucket, resolve_display_name)
                }
            }
            _ if clean_content.trim_start().starts_with('<') => {
                format!("[{}]", MessageType::from(ref_type))
            }
            _ => decode_text_content(clean_content, time_bucket, resolve_display_name),
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
    let embedded_name = crate::media::extract_xml_tag(refermsg, "displayname")
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());

    let sender_id = content_sender
        .map(|id| id.to_string())
        .or_else(|| crate::media::extract_xml_tag(refermsg, "chatusr"))
        .or_else(|| {
            crate::media::extract_xml_tag(refermsg, "fromusr")
                .filter(|id| !id.contains("@chatroom"))
        });

    if let Some(sender_id) = sender_id {
        let sender = resolve_display_name(&sender_id);
        if !sender.trim().is_empty() && sender != sender_id {
            return Some(sender);
        }
        return embedded_name.or_else(|| (!sender.trim().is_empty()).then_some(sender));
    }

    embedded_name
}

fn decode_location_content(content: &str) -> String {
    if !content.trim_start().starts_with('<') {
        return format!("[位置] {}", content);
    }
    let label = crate::media::extract_xml_attr(content, "label")
        .or_else(|| crate::media::extract_xml_tag(content, "label"));
    let poiname = crate::media::extract_xml_attr(content, "poiname")
        .or_else(|| crate::media::extract_xml_tag(content, "poiname"));
    let location_name = poiname
        .as_deref()
        .or(label.as_deref())
        .unwrap_or("未知位置");
    format!("[位置] {}", location_name)
}

fn decode_call_content(content: &str) -> String {
    if let Some(dur_secs) = crate::media::extract_xml_tag_int(content, "duration").or_else(|| {
        content
            .split_whitespace()
            .find_map(|w| w.parse::<i64>().ok())
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

fn decode_mail_content(content: &str) -> String {
    if !content.trim_start().starts_with('<') {
        if content.is_empty() {
            return "[邮件]".to_string();
        }
        return format!("[邮件] {}", truncate_display(content, 120));
    }

    let subject = crate::media::extract_xml_tag(content, "subject")
        .or_else(|| crate::media::extract_xml_tag(content, "title"))
        .unwrap_or_default();
    let sender = crate::media::extract_xml_tag(content, "sender")
        .or_else(|| crate::media::extract_xml_tag(content, "name"))
        .unwrap_or_default();
    let digest = crate::media::extract_xml_tag(content, "digest").unwrap_or_default();
    let summary = if !subject.is_empty() {
        subject
    } else if !digest.is_empty() {
        digest
    } else {
        String::new()
    };

    match (summary.is_empty(), sender.is_empty()) {
        (false, false) => format!("[邮件] {} - {}", summary, sender),
        (false, true) => format!("[邮件] {}", summary),
        (true, false) => format!("[邮件] {}", sender),
        (true, true) => "[邮件]".to_string(),
    }
}

fn decode_contact_card_content(content: &str, label: &str) -> String {
    if !content.trim_start().starts_with('<') {
        if content.is_empty() {
            return format!("[{}]", label);
        }
        return format!("[{}] {}", label, truncate_display(content, 120));
    }

    let name = crate::media::extract_xml_attr(content, "nickname")
        .or_else(|| crate::media::extract_xml_attr(content, "alias"))
        .or_else(|| crate::media::extract_xml_attr(content, "username"))
        .unwrap_or_default();
    if name.is_empty() {
        format!("[{}]", label)
    } else {
        format!("[{}] {}", label, name)
    }
}

fn decode_file_content(content: &str, packed_info: &[u8]) -> String {
    let packed_name = extract_file_name_from_packed_info(packed_info);
    if content.trim_start().starts_with('<') {
        if let Some(info) = media::parse_file_info(content) {
            return info.display(packed_name.as_deref());
        }
        if let Some(name) = packed_name {
            return format!("[文件] {}", name);
        }
        return "[文件]".to_string();
    }

    let name = if let Some(name) = packed_name.as_deref() {
        name
    } else if content.contains('/') {
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

fn extract_file_name_from_packed_info(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }
    let meta = media_pb::parse_img2(data)?.file?;
    media_pb::file_identifier(&meta).map(ToString::to_string)
}

fn decode_music_content(content: &str) -> String {
    if !content.is_empty() {
        format!("[音乐] {}", content)
    } else {
        "[音乐]".to_string()
    }
}

fn extract_voice_duration_secs(content: &str) -> Option<i64> {
    crate::media::extract_xml_tag_int(content, "voicelength")
        .or_else(|| crate::media::extract_xml_tag_int(content, "duration"))
        .or_else(|| {
            content
                .split(|c: char| !c.is_ascii_digit())
                .find_map(|s| s.parse::<i64>().ok())
        })
}

fn format_voice_duration_zh(duration_secs: Option<i64>) -> String {
    match duration_secs {
        Some(d) if d > 0 => format!(" {}秒", d),
        _ => String::new(),
    }
}

fn truncate_display(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

/// Decompress message content.
///
/// Tries ZSTD first (Telegram 4.x), then ZLIB (older), then falls back to raw UTF-8.
pub fn try_decompress(raw: &[u8]) -> Option<String> {
    if let Ok(s) = try_zstd(raw) {
        if !s.is_empty() {
            return Some(s);
        }
    }
    if let Ok(s) = try_zlib(raw) {
        if !s.is_empty() {
            return Some(s);
        }
    }
    String::from_utf8(raw.to_vec())
        .ok()
        .filter(|s| !s.is_empty())
}

thread_local! {
    /// One decompression context per thread, reused across messages. Each use
    /// starts by resetting the session (`Decoder::with_context`), so no state
    /// or output carries from one message to the next; only the allocation is
    /// reused.
    static ZSTD_DCTX: RefCell<Option<zstd::zstd_safe::DCtx<'static>>> =
        const { RefCell::new(None) };
}

const ZSTD_RESERVE_CAP: u64 = 1 << 20;

fn try_zstd(data: &[u8]) -> Result<String, ()> {
    ZSTD_DCTX.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(zstd::zstd_safe::DCtx::try_create().ok_or(())?);
        }
        let context = slot.as_mut().expect("context was just created");
        // The declared size only sizes the first allocation; it comes from the
        // input, so it is capped, and decoding still grows the buffer if needed.
        let declared = zstd::zstd_safe::get_frame_content_size(data)
            .ok()
            .flatten()
            .unwrap_or(0);
        let mut out = Vec::with_capacity(declared.min(ZSTD_RESERVE_CAP) as usize);
        let mut d = zstd::stream::read::Decoder::with_context(data, context);
        d.read_to_end(&mut out).map_err(|_| ())?;
        String::from_utf8(out).map_err(|_| ())
    })
}

fn try_zlib(data: &[u8]) -> Result<String, ()> {
    let mut d = ZlibDecoder::new(data);
    let mut s = String::new();
    d.read_to_string(&mut s).map_err(|_| ())?;
    if s.is_empty() {
        Err(())
    } else {
        Ok(s)
    }
}

/// Parse sender account id from message content.
pub fn parse_sender_from_content(content: &str) -> (Option<&str>, &str) {
    // Only the first `:` is considered; a byte search finds the same offset
    // as walking chars, since `:` is ASCII.
    let Some(i) = memchr::memchr(b':', content.as_bytes()).filter(|&i| i > 0) else {
        return (None, content);
    };
    let prefix = &content[..i];
    let is_id = prefix.starts_with(dictionary::account_id_prefix())
        || prefix.starts_with("gh_")
        || prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'@'));
    if is_id && prefix.len() >= 3 {
        let after = &content[i + 1..];
        let after = after.trim_start_matches([' ', '\n']);
        return (Some(prefix), after);
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
        assert_eq!(MessageType::from(35), MessageType::Mail);
        assert_eq!(MessageType::from(42), MessageType::ContactCard);
        assert_eq!(MessageType::from(66), MessageType::ExternalCard);
        assert_eq!(MessageType::from(11000), MessageType::Notification);
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
    fn test_decode_revoke_system_message() {
        let xml = r#"<?xml version="1.0"?><sysmsg type="revokemsg"><revokemsg><content>&quot;张三&quot; 撤回了一条消息</content><revoketime>0</revoketime></revokemsg></sysmsg>"#;
        let d = decode_message(10000, xml, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, r#"[撤回] "张三" 撤回了一条消息"#);
        assert_eq!(d.display_name, "系统");
    }

    #[test]
    fn test_decode_system_message_strips_sender_prefix_before_xml() {
        let xml = r#"tgid_abc:
<sysmsg type="revokemsg"><revokemsg><content>撤回了一条消息</content></revokemsg></sysmsg>"#;
        let d = decode_message(10000, xml, "Group", None, &[], |id| id.to_string());
        assert_eq!(d.content, "[撤回] 撤回了一条消息");
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
        packed.push(8);
        packed.push(1);
        let img = [
            8, 0xb8, 0x08, 16, 0x80, 0x0f, 34, 8, 116, 101, 115, 116, 46, 106, 112, 103,
        ];
        packed.push(26);
        packed.push(img.len() as u8);
        packed.extend_from_slice(&img);
        let d = decode_message(3, "", "Alice", None, &packed, |id| id.to_string());
        assert_eq!(d.msg_type, MessageType::Image);
        assert_eq!(d.content, "[img:test.jpg]");
    }

    #[test]
    fn test_decode_image_with_xml_identifier() {
        let xml = r#"<msg><img aeskey="abc123" cdnthumbwidth="180" cdnthumbheight="153" rawlength="38186" /></msg>"#;
        let d = decode_message(3, xml, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, "[img:abc123]");
    }

    #[test]
    fn test_decode_image_falls_back_to_xml_identifier() {
        let mut packed = Vec::new();
        packed.push(8);
        packed.push(1);
        let img = [8, 0xb8, 0x08, 16, 0x80, 0x0f];
        packed.push(26);
        packed.push(img.len() as u8);
        packed.extend_from_slice(&img);

        let xml = r#"<msg><img aeskey="xml-key" /></msg>"#;
        let d = decode_message(3, xml, "Alice", None, &packed, |id| id.to_string());
        assert_eq!(d.content, "[img:xml-key]");
    }

    #[test]
    fn test_decode_image_without_identifier() {
        let xml =
            r#"<msg><img cdnthumbwidth="180" cdnthumbheight="153" rawlength="38186" /></msg>"#;
        let d = decode_message(3, xml, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, "[img]");
    }

    #[test]
    fn test_decode_group_chat_sender() {
        let d = decode_message(1, "tgid_abc:\nHello", "Group", None, &[], |id| {
            if id == "tgid_abc" {
                "Bob".into()
            } else {
                id.into()
            }
        });
        assert_eq!(d.content, "Hello");
        assert_eq!(d.display_name, "Bob");
    }

    #[test]
    fn test_decode_media_group_chat_sender() {
        let d = decode_message(3, "tgid_abc:\n", "Group", None, &[], |id| {
            if id == "tgid_abc" {
                "Bob".into()
            } else {
                id.into()
            }
        });
        assert_eq!(d.display_name, "Bob");
    }

    #[test]
    fn test_decode_voice_includes_export_id_and_duration() {
        let d = decode_message_with_context(
            34,
            "<msg><voicelength>7</voicelength></msg>",
            "Alice",
            None,
            &[],
            DecodeContext {
                time_bucket: time::MessageTimeBucket::Minute(1),
                voice_id: Some(42),
            },
            |id| id.to_string(),
        );

        assert_eq!(d.content, "[voice:42:7s]");
    }

    #[test]
    fn test_decode_voice_keeps_legacy_display_without_id() {
        let d = decode_message(
            34,
            "<msg><voicelength>7</voicelength></msg>",
            "Alice",
            None,
            &[],
            |id| id.to_string(),
        );

        assert_eq!(d.content, "[语音 7秒]");
    }

    #[test]
    fn test_parse_sender() {
        let (id, c) = parse_sender_from_content("tgid_a:\nHi");
        assert_eq!(id, Some("tgid_a"));
        assert_eq!(c, "Hi");
        let (id, c) = parse_sender_from_content("room@chatroom:\nHi");
        assert_eq!(id, Some("room@chatroom"));
        assert_eq!(c, "Hi");
        let (id, c) = parse_sender_from_content("plain text");
        assert_eq!(id, None);
        assert_eq!(c, "plain text");
    }

    #[test]
    fn test_parse_sender_uses_only_the_first_colon() {
        for (input, expected) in [
            ("", (None, "")),
            (":abc", (None, ":abc")),
            ("ab:x", (None, "ab:x")),
            ("a b:c", (None, "a b:c")),
            ("中文名:x", (None, "中文名:x")),
            ("文本 abc:def", (None, "文本 abc:def")),
            ("abc:", (Some("abc"), "")),
            ("abc: \n \tx:y", (Some("abc"), "\tx:y")),
            ("gh_x y:z", (Some("gh_x y"), "z")),
            ("a-b@c:d", (Some("a-b@c"), "d")),
            ("x y:abc:z", (None, "x y:abc:z")),
        ] {
            assert_eq!(parse_sender_from_content(input), expected, "{input:?}");
        }
    }

    #[test]
    fn test_decode_link() {
        let xml = r#"<?xml?><msg><appmsg><title>标题</title><type>5</type></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| id.to_string());
        assert!(d.content.contains("链接"));
        assert!(d.content.contains("标题"));
    }

    #[test]
    fn test_decode_xml_declared_app_link_from_text_path() {
        let xml = r#"<?xml version="1.0"?>
<msg>
        <appmsg appid="test" sdkver="0">
                <title>我说这家纽约zui好吃台湾菜应该没人反对吧</title>
                <des>@曼迪酱's note
95 Shares</des>
                <type>5</type>
                <url>https://www.example.com/item/1?x=1&amp;y=2</url>
        </appmsg>
</msg>"#;
        let d = decode_message(1, xml, "Alice", None, &[], |id| id.to_string());

        assert!(d.content.starts_with("[链接]"));
        assert!(d
            .content
            .contains("我说这家纽约zui好吃台湾菜应该没人反对吧"));
        assert!(d.content.contains("@曼迪酱's note"));
        assert!(d.content.contains("https://www.example.com/item/1?x=1&y=2"));
        assert!(!d.content.contains("<appmsg"));
    }

    #[test]
    fn test_decode_app_link_keeps_long_url() {
        let long_url = format!("https://www.example.com/{}", "a".repeat(180));
        let xml = format!(
            r#"<msg><appmsg><title>Long Link</title><des>Details</des><type>5</type><url>{}</url></appmsg></msg>"#,
            long_url
        );
        let d = decode_message(49, &xml, "Alice", None, &[], |id| id.to_string());

        assert_eq!(
            d.content,
            format!("[链接] Long Link - Details\n  {}", long_url)
        );
    }

    #[test]
    fn test_decode_app_subtype_4_as_video_card() {
        let xml = r#"<?xml version="1.0"?>
<msg>
        <appmsg appid="test" sdkver="0">
                <title>她有时候只是激素影响，并不是故意的</title>
                <des>@有趣小剧场's note
38.2k Shares</des>
                <type>4</type>
                <url>https://www.example.com/video/1</url>
        </appmsg>
</msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| id.to_string());

        assert_eq!(
            d.content,
            "[视频] 她有时候只是激素影响，并不是故意的 - @有趣小剧场's note\n38.2k Shares\n  https://www.example.com/video/1"
        );
    }

    #[test]
    fn test_decode_short_video_subtype_51_as_video_card() {
        let xml = r#"<?xml version="1.0"?>
<msg>
        <appmsg appid="" sdkver="0">
                <title>当前版本不支持展示该内容，请升级至最新版本。</title>
                <type>51</type>
                <url>https://www.example.com/security/readtemplate?t=upgrade</url>
                <finderFeed>
                        <feedType>4</feedType>
                        <title><![CDATA[货币周期]]></title>
                        <nickname><![CDATA[混哥趋势观]]></nickname>
                        <desc><![CDATA[接下来的几年，央妈要靠什么印钱？ #通胀 #AI]]></desc>
                        <mediaList><media><mediaType>4</mediaType></media></mediaList>
                </finderFeed>
        </appmsg>
</msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| id.to_string());

        assert_eq!(
            d.content,
            "[视频] 货币周期 - 接下来的几年，央妈要靠什么印钱？ #通胀 #AI - @混哥趋势观"
        );
    }

    #[test]
    fn test_decode_legacy_subtype_51_as_reference_card() {
        let xml = r#"<msg><appmsg><title>Chat History</title><type>51</type></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| id.to_string());

        assert_eq!(d.content, "[引用: Chat History]");
    }

    #[test]
    fn test_decode_prefixed_app_video_xml() {
        let xml = r#"[视频] <?xml version="1.0"?><msg><appmsg><title>思朗诵，开始吟唱</title><type>4</type></appmsg></msg>"#;
        let d = decode_message(1, xml, "Alice", None, &[], |id| id.to_string());

        assert_eq!(d.content, "[视频] 思朗诵，开始吟唱");
    }

    #[test]
    fn test_decode_file_message_includes_size() {
        let xml = r#"<msg><appmsg><title>report.pdf</title><type>6</type><appattach><totallen>1536</totallen><fileext>pdf</fileext></appattach></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, "[文件] report.pdf (2KB)");
    }

    #[test]
    fn test_decode_app_subtype_62_as_file() {
        let xml = r#"<msg><appmsg><title>report.pdf</title><type>62</type><appattach><totallen>4096</totallen><fileext>pdf</fileext></appattach></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, "[文件] report.pdf (4KB)");
    }

    #[test]
    fn test_decode_pat_app_message_not_as_file() {
        let xml = r#"<msg><appmsg><title>我拍了拍 "Bob" 放了个炮[爆竹]</title><type>62</type><appattach><totallen>0</totallen><fileext></fileext></appattach><patinfo><fromusername>tgid_me</fromusername><pattedusername>tgid_bob</pattedusername><template>我拍了拍 "${tgid_bob}" 放了个炮[爆竹]</template></patinfo></appmsg></msg>"#;
        let d = decode_message(49, xml, "Bob", None, &[], |id| id.to_string());

        assert_eq!(d.content, r#"[拍一拍] 我拍了拍 "Bob" 放了个炮[爆竹]"#);
    }

    #[test]
    fn test_decode_file_message_uses_packed_name_fallback() {
        let filename = b"fallback.docx";
        let mut inner = vec![8, 0, 18, filename.len() as u8];
        inner.extend_from_slice(filename);
        let mut file = vec![10, inner.len() as u8];
        file.extend_from_slice(&inner);
        let mut packed = vec![8, 3, 16, 6, 58, file.len() as u8];
        packed.extend_from_slice(&file);

        let xml = r#"<msg><appmsg><type>6</type><appattach><totallen>0</totallen></appattach></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &packed, |id| id.to_string());
        assert_eq!(d.content, "[文件] fallback.docx");
    }

    #[test]
    fn test_decode_mail_message() {
        let xml = r#"<msg><pushmail><subject>Weekly Update</subject><sender>Alice</sender><digest>short</digest></pushmail></msg>"#;
        let d = decode_message(35, xml, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.msg_type, MessageType::Mail);
        assert_eq!(d.content, "[邮件] Weekly Update - Alice");
    }

    #[test]
    fn test_decode_contact_card_message() {
        let xml = r#"<msg username="tgid_card" nickname="Alice Card" alias="alice" />"#;
        let d = decode_message(42, xml, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.msg_type, MessageType::ContactCard);
        assert_eq!(d.content, "[名片] Alice Card");
    }

    #[test]
    fn test_decode_external_card_message() {
        let xml = r#"<msg username="tgid_external" nickname="External Alice" />"#;
        let d = decode_message(66, xml, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.msg_type, MessageType::ExternalCard);
        assert_eq!(d.content, "[外部联系人] External Alice");
    }

    #[test]
    fn test_decode_notification_message() {
        let d = decode_message(11000, "", "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.msg_type, MessageType::Notification);
        assert_eq!(d.content, "[通知]");
    }

    #[test]
    fn test_unknown_app_subtype_keeps_subtype_in_summary() {
        let xml = r#"<msg><appmsg><title>Special Card</title><des>Details</des><type>87</type></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, "[卡片:87] Special Card - Details");
    }

    #[test]
    fn test_decode_group_notice_card() {
        let xml = r#"<msg><appmsg><type>87</type><group_notice_item><announcement><textannouncement><![CDATA[课程资料、课程视频和练习均会开源]]></textannouncement></announcement></group_notice_item></appmsg></msg>"#;
        let d = decode_message(49, xml, "Group", None, &[], |id| id.to_string());

        assert_eq!(d.content, "[群公告] 课程资料、课程视频和练习均会开源");
    }

    #[test]
    fn test_unknown_internal_xml_is_summarized_without_raw_tags() {
        let xml = r#"<msg><quote username="tgid_a" displayname="Alice" /></msg>"#;
        let d = decode_message(1, xml, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, "[消息]");
    }

    #[test]
    fn test_late_internal_xml_tail_is_sanitized() {
        let raw = "prefix <msg><emoji /></msg>";
        let d = decode_message(1, raw, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, "prefix\n[表情]");
    }

    #[test]
    fn test_sticker_message_includes_export_identifier() {
        let raw = r#"<msg><emoji md5="abc123" /></msg>"#;
        let d = decode_message(47, raw, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, "[sticker:abc123]");

        let raw = r#"prefix <msg><emoji md5="abc123" /></msg>"#;
        let d = decode_message(1, raw, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, "prefix\n[sticker:abc123]");
    }

    #[test]
    fn test_long_non_xml_link_truncates_on_char_boundary() {
        let raw = "好".repeat(220);
        let d = decode_message(49, &raw, "Alice", None, &[], |id| id.to_string());
        assert!(d.content.starts_with("[链接] "));
        assert!(d.content.ends_with("..."));
    }

    #[test]
    fn test_app_fallback_drops_xml_summary_fields() {
        let xml = r#"<msg><appmsg><title>&lt;msg&gt;&lt;quote /&gt;&lt;/msg&gt;</title><type>87</type></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, "[卡片:87] 未知卡片");
    }

    #[test]
    fn test_decode_chat_history_expands_record_items() {
        let xml = r#"<msg><appmsg><title>Chat History for A and B</title><des>A: first
B: second</des><type>19</type><recorditem><![CDATA[<recordinfo><datalist count="4"><dataitem datatype="1" dataid="a"><sourcename>A</sourcename><sourcetime>2026-04-29 07:39</sourcetime><datadesc>first</datadesc></dataitem><dataitem datatype="1" dataid="b"><sourcename>B</sourcename><sourcetime>2026-04-29 21:15</sourcetime><datadesc>second line
continues</datadesc></dataitem><dataitem datatype="1" dataid="c"><sourcename>B</sourcename><sourcetime>2026-04-29 21:15</sourcetime><datadesc>follow-up</datadesc></dataitem><dataitem datatype="2" dataid="img123"><sourcename>A</sourcename><sourcetime>2026-04-29 21:16</sourcetime></dataitem></datalist></recordinfo>]]></recorditem></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| id.to_string());

        assert_eq!(
            d.content,
            "[聊天记录] Chat History for A and B (4条)\n        [2026-04-29 07:39]\n        A: first\n        [2026-04-29 21:15]\n        B: second line\n          continues\n         follow-up\n        [2026-04-29 21:16]\n        A: [img:img123]"
        );
    }

    #[test]
    fn test_decode_chat_history_uses_requested_time_bucket() {
        let xml = r#"<msg><appmsg><title>Chat History</title><type>19</type><recorditem><![CDATA[<recordinfo><datalist count="2"><dataitem datatype="1"><sourcename>A</sourcename><sourcetime>2026-04-29 21:15</sourcetime><datadesc>first</datadesc></dataitem><dataitem datatype="1"><sourcename>B</sourcename><sourcetime>2026-04-29 21:59</sourcetime><datadesc>second</datadesc></dataitem></datalist></recordinfo>]]></recorditem></appmsg></msg>"#;
        let d = decode_message_with_time_bucket(
            49,
            xml,
            "Alice",
            None,
            &[],
            time::MessageTimeBucket::Hour(1),
            |id| id.to_string(),
        );

        assert_eq!(
            d.content,
            "[聊天记录] Chat History (2条)\n        [2026-04-29 21:00]\n        A: first\n        B: second"
        );
    }

    #[test]
    fn test_decode_chat_history_falls_back_to_summary() {
        let xml = r#"<msg><appmsg><title>Chat History</title><des>A: first</des><type>19</type></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, "[聊天记录] Chat History - A: first");
    }

    #[test]
    fn test_decode_nested_chat_history() {
        // The inner record sits inside an item of the outer CDATA record; its
        // items must not be counted as items of the outer record.
        let xml = r#"<msg><appmsg><title>Outer History</title><type>19</type><recorditem><![CDATA[<recordinfo><datalist count="2"><dataitem datatype="1" dataid="a"><sourcename>A</sourcename><sourcetime>2026-05-01 10:00</sourcetime><datadesc>hello</datadesc></dataitem><dataitem datatype="19" dataid="nested"><sourcename>B</sourcename><sourcetime>2026-05-01 10:05</sourcetime><recorditem><recordinfo><title>Inner Record</title><datalist count="2"><dataitem datatype="1" dataid="x"><sourcename>C</sourcename><sourcetime>2026-05-01 09:00</sourcetime><datadesc>inner msg 1</datadesc></dataitem><dataitem datatype="1" dataid="y"><sourcename>D</sourcename><sourcetime>2026-05-01 09:01</sourcetime><datadesc>inner msg 2</datadesc></dataitem></datalist></recordinfo></recorditem></dataitem></datalist></recordinfo>]]></recorditem></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| id.to_string());

        assert_eq!(
            d.content,
            "[聊天记录] Outer History (2条)\n        [2026-05-01 10:00]\n        A: hello\n        [2026-05-01 10:05]\n        B: [聊天记录] Inner Record (2条)\n                  [2026-05-01 09:00]\n                  C: inner msg 1\n                  [2026-05-01 09:01]\n                  D: inner msg 2"
        );
    }

    #[test]
    fn test_decode_nested_record_xml_reads_fields_around_it() {
        // The nested record comes before this item's own sender and time, and
        // its own items carry the same field names.
        let xml = r#"<msg><appmsg><title>Outer</title><type>19</type><recorditem><![CDATA[<recordinfo><datalist count="1"><dataitem datatype="17" dataid="n"><datatitle>Inner</datatitle><recordxml><recordinfo><title>Inner</title><desc>C: hi</desc><datalist count="1"><dataitem datatype="1"><sourcename>C</sourcename><sourcetime>2026-05-01 09:00</sourcetime><datadesc>hi</datadesc></dataitem></datalist></recordinfo></recordxml><sourcename>B</sourcename><sourcetime>2026-05-01 10:05</sourcetime></dataitem></datalist></recordinfo>]]></recorditem></appmsg></msg>"#;
        let d = decode_message_with_time_bucket(
            49,
            xml,
            "Alice",
            None,
            &[],
            time::MessageTimeBucket::PerMessage,
            |id| id.to_string(),
        );

        assert_eq!(
            d.content,
            "[聊天记录] Outer (1条)\n        [2026-05-01 10:05] B: [聊天记录] Inner (1条)\n                  [2026-05-01 09:00] C: hi"
        );
    }

    #[test]
    fn test_decode_nested_chat_history_stops_at_depth_limit() {
        let mut record = r#"<dataitem datatype="1"><sourcename>E</sourcename><datadesc>deepest</datadesc></dataitem>"#.to_string();
        for level in (1..=MAX_CHAT_HISTORY_DEPTH + 1).rev() {
            record = format!(
                r#"<dataitem datatype="17"><datatitle>L{level}</datatitle><recordxml><recordinfo><title>L{level}</title><datalist>{record}</datalist></recordinfo></recordxml><sourcename>S</sourcename></dataitem>"#
            );
        }
        let xml = format!(
            r#"<msg><appmsg><title>L0</title><type>19</type><recorditem><![CDATA[<recordinfo><datalist>{record}</datalist></recordinfo>]]></recorditem></appmsg></msg>"#
        );
        let d = decode_message(49, &xml, "Alice", None, &[], |id| id.to_string());

        let deepest = format!("[聊天记录] L{MAX_CHAT_HISTORY_DEPTH} (1条)");
        assert!(d.content.contains(&deepest), "content was: {}", d.content);
        let cut = format!("S: [记录] L{}", MAX_CHAT_HISTORY_DEPTH + 1);
        assert!(d.content.contains(&cut), "content was: {}", d.content);
        assert!(!d.content.contains("deepest"));
    }

    #[test]
    fn test_decode_quote_message() {
        let xml = r#"<msg><appmsg><title>回复内容</title><type>57</type><refermsg><type>1</type><displayname>Bob</displayname><content>引用内容</content></refermsg></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, "> Bob: 引用内容\n 回复内容");
    }

    #[test]
    fn test_decode_quote_prefers_resolved_sender_over_embedded_name() {
        let xml = r#"<msg><appmsg><title>回复内容</title><type>57</type><refermsg><type>1</type><displayname>Group Bob</displayname><chatusr>tgid_bob</chatusr><content>引用内容</content></refermsg></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| {
            if id == "tgid_bob" {
                "Remark Bob".into()
            } else {
                id.into()
            }
        });
        assert_eq!(d.content, "> Remark Bob: 引用内容\n 回复内容");
    }

    #[test]
    fn test_decode_quote_uses_embedded_name_when_sender_is_unknown() {
        let xml = r#"<msg><appmsg><title>回复内容</title><type>57</type><refermsg><type>1</type><displayname>Group Bob</displayname><chatusr>tgid_bob</chatusr><content>引用内容</content></refermsg></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, "> Group Bob: 引用内容\n 回复内容");
    }

    #[test]
    fn test_decode_quoted_group_image() {
        let xml = r#"<msg><appmsg><title>回复图片</title><type>57</type><refermsg><type>3</type><chatusr>tgid_abc</chatusr><content>tgid_abc:
&lt;msg&gt;&lt;img cdnthumbwidth="180" cdnthumbheight="153" length="38186" /&gt;&lt;/msg&gt;</content></refermsg></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| {
            if id == "tgid_abc" {
                "Bob".into()
            } else {
                id.into()
            }
        });
        assert_eq!(d.content, "> Bob: [img]\n 回复图片");
    }

    #[test]
    fn test_decode_quoted_link_with_display_prefix() {
        let xml = r#"<msg><appmsg><title>回复链接</title><type>57</type><refermsg><type>49</type><displayname>Bob</displayname><content>[链接] &lt;?xml version="1.0"?&gt;&lt;msg&gt;&lt;appmsg&gt;&lt;title&gt;Example&lt;/title&gt;&lt;type&gt;5&lt;/type&gt;&lt;/appmsg&gt;&lt;/msg&gt;</content></refermsg></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| id.to_string());

        assert_eq!(d.content, "> Bob: [链接] Example\n 回复链接");
    }

    #[test]
    fn test_decode_quoted_text_replaces_embedded_app_xml() {
        let xml = r#"<msg><appmsg><title>外层回复文本</title><type>57</type><refermsg><type>1</type><displayname>Bob</displayname><content>tgid_bob:
被引用的占位文本
&lt;msg&gt;&lt;appmsg&gt;&lt;title&gt;嵌套引用标题&lt;/title&gt;&lt;type&gt;57&lt;/type&gt;&lt;refermsg&gt;&lt;type&gt;3&lt;/type&gt;&lt;chatusr&gt;tgid_alice&lt;/chatusr&gt;&lt;content&gt;tgid_alice:
&amp;lt;msg&amp;gt;&amp;lt;img aeskey="img-key" /&amp;gt;&amp;lt;/msg&amp;gt;&lt;/content&gt;&lt;/refermsg&gt;&lt;/appmsg&gt;&lt;/msg&gt;</content></refermsg></appmsg></msg>"#;
        let d = decode_message(49, xml, "Alice", None, &[], |id| match id {
            "tgid_alice" => "Alice".to_string(),
            "tgid_bob" => "Bob".to_string(),
            _ => id.to_string(),
        });

        assert!(d.content.contains("被引用的占位文本"));
        assert!(d.content.contains("嵌套引用标题"));
        assert!(d.content.contains("外层回复文本"));
        assert!(!d.content.contains("<msg"));
        assert!(!d.content.contains("<appmsg"));
    }

    fn zstd_frame(text: &[u8]) -> Vec<u8> {
        zstd::bulk::compress(text, 3).unwrap()
    }

    fn skippable_frame(payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0x50, 0x2A, 0x4D, 0x18];
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn zlib_stream(text: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(text).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn try_decompress_accepts_zstd_frames() {
        assert_eq!(
            try_decompress(&zstd_frame("你好".as_bytes())).as_deref(),
            Some("你好")
        );
        let big = "长消息".repeat(100_000);
        assert_eq!(try_decompress(&zstd_frame(big.as_bytes())), Some(big));

        let mut two = zstd_frame(b"ab");
        two.extend(zstd_frame(b"cd"));
        assert_eq!(try_decompress(&two).as_deref(), Some("abcd"));

        let mut skipped = skippable_frame(b"xyz");
        skipped.extend(zstd_frame(b"ab"));
        skipped.extend(skippable_frame(b""));
        assert_eq!(try_decompress(&skipped).as_deref(), Some("ab"));
    }

    #[test]
    fn try_decompress_falls_back_for_rejected_zstd() {
        // A frame whose output is empty, invalid UTF-8, truncated, or followed by
        // trailing bytes is not zstd text; the raw bytes then decide.
        assert_eq!(try_decompress(&zstd_frame(b"")), None);
        // A lone skippable frame decodes to nothing; its bytes happen to be UTF-8.
        let lone = skippable_frame(b"hi");
        assert_eq!(try_decompress(&lone), String::from_utf8(lone.clone()).ok());
        assert_eq!(try_decompress(&zstd_frame(&[0xff, 0xfe])), None);

        let frame = zstd_frame("你好你好你好你好".as_bytes());
        assert_eq!(try_decompress(&frame[..frame.len() - 1]), None);

        let mut trailing = zstd_frame(b"ab");
        trailing.extend_from_slice(b"tail");
        assert_eq!(try_decompress(&trailing), None);

        let mut empty_then_text = zstd_frame(b"");
        empty_then_text.extend(zstd_frame(b"ok"));
        assert_eq!(try_decompress(&empty_then_text).as_deref(), Some("ok"));
    }

    #[test]
    fn try_decompress_reuses_state_after_failures() {
        let frame = zstd_frame(b"first");
        assert_eq!(try_decompress(&frame[..frame.len() - 2]), None);
        assert_eq!(try_decompress(&frame).as_deref(), Some("first"));
        let mut trailing = zstd_frame(b"x");
        trailing.push(0x28);
        assert_eq!(try_decompress(&trailing), None);
        assert_eq!(
            try_decompress(&zstd_frame(b"second")).as_deref(),
            Some("second")
        );
    }

    #[test]
    fn try_decompress_zlib_and_plain_text() {
        assert_eq!(
            try_decompress(&zlib_stream("旧版".as_bytes())).as_deref(),
            Some("旧版")
        );
        assert_eq!(try_decompress(&zlib_stream(b"")), None);
        assert_eq!(try_decompress(b"plain text").as_deref(), Some("plain text"));
        assert_eq!(
            try_decompress("纯文本".as_bytes()).as_deref(),
            Some("纯文本")
        );
        assert_eq!(try_decompress(b""), None);
        assert_eq!(try_decompress(&[0xff, 0x00]), None);
        // Starts with the zstd magic but is not a frame.
        assert_eq!(try_decompress(&[0x28, 0xB5, 0x2F, 0xFD]), None);
        assert_eq!(try_decompress(&[0x28, 0xB5, 0x2F, 0xFD, b'a']), None);
    }

    #[test]
    fn internal_xml_markers_match_per_needle_search() {
        let inputs = [
            "",
            "plain 文本",
            "a < b & c",
            "前缀 <msg><img/></msg>",
            "x &lt;msg&gt; y <?xml version",
            "&amp;lt;sysmsg <sysmsg",
            "<emoji> then <location",
            "文本&lt;dataitem",
            "<<msg",
            "&&lt;?xml",
            "<ms",
            "&lt;",
        ];
        for needle in XML_START_MARKERS.iter().chain(XML_CONTENT_MARKERS) {
            assert!(
                needle.starts_with('<') || needle.starts_with('&'),
                "{needle}"
            );
        }
        for input in inputs {
            let expected_start = XML_START_MARKERS.iter().filter_map(|n| input.find(n)).min();
            assert_eq!(find_internal_xml_start(input), expected_start, "{input:?}");
            let expected_marker = XML_CONTENT_MARKERS.iter().any(|n| input.contains(n));
            assert_eq!(
                contains_internal_xml_marker(input),
                expected_marker,
                "{input:?}"
            );
        }
    }
}
