use crate::dictionary;
use crate::media;
use crate::media_pb;
use crate::time;
use flate2::read::ZlibDecoder;
use std::fmt;
use std::io::Read;

/// Telegram 消息类型枚举。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MessageType {
    Text,         // 1
    Image,        // 3
    Voice,        // 34
    Sticker,      // 47
    Video,        // 43
    Link,         // 49 — 链接/文件/小程序/音乐
    System,       // 10000
    RedEnvelope,  // 436207665
    Transfer,     // 536870918
    Location,     // 48
    File,         // 62
    Call,         // 50
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
        return DecodedMessage {
            msg_type: msg_type_enum,
            content: decode_system_content(raw_content, msg_type_enum),
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
    let content = decode_content_by_type(
        msg_type,
        clean_content,
        context.time_bucket,
        &resolve_display_name,
    );

    DecodedMessage {
        msg_type: msg_type_enum,
        content,
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
    time_bucket: time::MessageTimeBucket,
    resolve_display_name: &impl Fn(&str) -> String,
) -> String {
    match msg_type {
        1 => decode_text_content(content, time_bucket, resolve_display_name),
        49 => decode_link_content(content, time_bucket, resolve_display_name),
        48 => decode_location_content(content),
        50 => decode_call_content(content),
        62 => decode_file_content(content),
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
        let start = fragment.find("<msg")?;
        &fragment[start..]
    } else {
        fragment
    };

    if !xml.starts_with("<msg") {
        return None;
    }

    if xml.contains("<appmsg") {
        return Some(decode_link_content(xml, time_bucket, resolve_display_name));
    }
    if xml.contains("<img") {
        return Some(media::parse_image_info(xml).display());
    }
    if xml.contains("<videomsg") || xml.contains("<video") {
        return Some(media::parse_video_info(xml).display());
    }

    None
}

fn strip_display_prefix(content: &str) -> &str {
    let trimmed = content.trim_start();
    for prefix in ["[链接]", "[卡片]", "[小程序]", "[文件]"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.trim_start();
        }
    }
    trimmed
}

fn find_internal_xml_start(content: &str) -> Option<usize> {
    ["<?xml", "<msg"]
        .iter()
        .filter_map(|needle| content.find(needle))
        .min()
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
        if let Some(text) = crate::media::extract_xml_tag(trimmed, "content") {
            return text;
        }
    }

    content.to_string()
}

fn decode_link_content(
    content: &str,
    time_bucket: time::MessageTimeBucket,
    resolve_display_name: &impl Fn(&str) -> String,
) -> String {
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
            let title = crate::media::extract_xml_tag(content, "title")
                .unwrap_or_else(|| "未知歌曲".to_string());
            format!("[音乐] {}", title)
        }
        6 => {
            let name = crate::media::extract_xml_tag(content, "title")
                .unwrap_or_else(|| "未知文件".to_string());
            format!("[文件] {}", name)
        }
        19 => decode_chat_history_content(content, time_bucket),
        57 => decode_quote_content(content, time_bucket, resolve_display_name),
        51 => {
            let title = crate::media::extract_xml_tag(content, "title")
                .unwrap_or_else(|| "聊天记录".to_string());
            format!("[引用: {}]", title)
        }
        _ => {
            let title = crate::media::extract_xml_tag(content, "title").unwrap_or_default();
            let desc = crate::media::extract_xml_tag(content, "des").unwrap_or_default();
            let title = if !title.is_empty() {
                title
            } else {
                "未知卡片".to_string()
            };
            if !desc.is_empty() {
                format!("[卡片] {} - {}", title, desc)
            } else {
                format!("[卡片] {}", title)
            }
        }
    }
}

#[derive(Debug, PartialEq)]
struct ChatHistoryItem {
    source_name: String,
    source_time: Option<String>,
    source_timestamp: Option<i64>,
    body: String,
}

fn decode_chat_history_content(content: &str, time_bucket: time::MessageTimeBucket) -> String {
    let title = crate::media::extract_xml_tag(content, "title")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "聊天记录".to_string());
    let record_item = crate::media::extract_xml_tag(content, "recorditem")
        .map(|s| strip_cdata(&s).to_string())
        .unwrap_or_default();

    let items = parse_chat_history_items(&record_item);
    if items.is_empty() {
        let desc = crate::media::extract_xml_tag(content, "des").unwrap_or_default();
        if desc.is_empty() {
            return format!("[聊天记录] {}", title);
        }
        return format!("[聊天记录] {} - {}", title, desc);
    }

    let mut out = format!("[聊天记录] {} ({}条)", title, items.len());
    render_chat_history_items(&mut out, &items, time_bucket);
    out
}

fn strip_cdata(value: &str) -> &str {
    let trimmed = value.trim();
    trimmed
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(trimmed)
}

fn parse_chat_history_items(record_info: &str) -> Vec<ChatHistoryItem> {
    let mut items = Vec::new();
    let mut rest = record_info;

    while let Some(start) = rest.find("<dataitem") {
        rest = &rest[start..];
        let Some(open_end) = rest.find('>') else {
            break;
        };
        let Some(close_start) = rest[open_end + 1..].find("</dataitem>") else {
            break;
        };
        let item_end = open_end + 1 + close_start + "</dataitem>".len();
        let item_xml = &rest[..item_end];

        if let Some(item) = parse_chat_history_item(item_xml) {
            items.push(item);
        }

        rest = &rest[item_end..];
    }

    items
}

fn parse_chat_history_item(item_xml: &str) -> Option<ChatHistoryItem> {
    let source_name = crate::media::extract_xml_tag(item_xml, "sourcename")
        .or_else(|| crate::media::extract_xml_tag(item_xml, "sourcedisplayname"))
        .unwrap_or_else(|| "未知".to_string());
    let source_time = crate::media::extract_xml_tag(item_xml, "sourcetime");
    let source_timestamp = source_time
        .as_deref()
        .and_then(time::parse_local_timestamp_minutes);
    let datatype =
        crate::media::extract_xml_attr(item_xml, "datatype").and_then(|s| s.parse::<i64>().ok());
    let body = decode_chat_history_item_body(item_xml, datatype);
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

fn decode_chat_history_item_body(item_xml: &str, datatype: Option<i64>) -> String {
    let desc = crate::media::extract_xml_tag(item_xml, "datadesc").unwrap_or_default();
    if !desc.is_empty() {
        return desc;
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
        _ if !title.is_empty() => title,
        Some(t) => format!("[记录:{}]", t),
        None => "[记录]".to_string(),
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
    let reply = crate::media::extract_xml_tag(content, "title").unwrap_or_default();
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
                    decode_link_content(normalized, time_bucket, resolve_display_name)
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

fn try_zstd(data: &[u8]) -> Result<String, ()> {
    let mut d = zstd::Decoder::new(data).map_err(|_| ())?;
    let mut s = String::new();
    d.read_to_string(&mut s).map_err(|_| ())?;
    if s.is_empty() {
        Err(())
    } else {
        Ok(s)
    }
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
    let account_prefix = dictionary::account_id_prefix();
    for (i, c) in content.char_indices() {
        if c != ':' {
            continue;
        }
        if i == 0 {
            break;
        }
        let prefix = &content[..i];
        let is_id = prefix.starts_with(&account_prefix)
            || prefix.starts_with("gh_")
            || prefix.contains('@')
            || prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
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
    fn test_decode_revoke_system_message() {
        let xml = r#"<?xml version="1.0"?><sysmsg type="revokemsg"><revokemsg><content>&quot;张三&quot; 撤回了一条消息</content><revoketime>0</revoketime></revokemsg></sysmsg>"#;
        let d = decode_message(10000, xml, "Alice", None, &[], |id| id.to_string());
        assert_eq!(d.content, r#"[撤回] "张三" 撤回了一条消息"#);
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
}
