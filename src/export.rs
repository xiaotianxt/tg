use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::contact;
use crate::db;
use crate::dictionary;
use crate::media;
use crate::media_index::MediaIndex;
use crate::message;
use crate::message_index;
use crate::parallel;
use crate::time;

#[derive(Clone, serde::Serialize)]
struct ExportMessage {
    time: String,
    timestamp: i64,
    sender: String,
    #[serde(rename = "type")]
    msg_type: i64,
    type_name: String,
    content: String,
    #[serde(skip)]
    raw_content: String,
    #[serde(skip)]
    packed_info: Vec<u8>,
}

type ExportMessageRow = (i64, i64, String, Option<i64>, Vec<u8>);

enum MediaExportJob {
    Cached {
        index: usize,
        src: PathBuf,
        category: &'static str,
        msg_type: i64,
    },
    Sticker {
        index: usize,
        message: ExportMessage,
    },
}

struct MediaExportResult {
    index: usize,
    is_sticker: bool,
    result: Result<PathBuf, String>,
}

pub struct ImageExportConfig<'a> {
    pub output_dir: &'a Path,
    pub list: bool,
    pub all: bool,
    pub index: Option<usize>,
    pub id: Option<&'a str>,
    pub limit: usize,
    pub since: Option<i64>,
    pub jobs: usize,
}

pub struct FileExportConfig<'a> {
    pub output_dir: &'a Path,
    pub list: bool,
    pub all: bool,
    pub index: Option<usize>,
    pub id: Option<&'a str>,
    pub limit: usize,
    pub since: Option<i64>,
    pub jobs: usize,
}

pub struct VoiceExportConfig<'a> {
    pub output_dir: &'a Path,
    pub format: VoiceOutputFormat,
    pub decoder: Option<&'a Path>,
    pub list: bool,
    pub all: bool,
    pub index: Option<usize>,
    pub id: Option<i64>,
    pub limit: usize,
    pub since: Option<i64>,
    pub jobs: usize,
    pub sample_rate: u32,
}

pub struct MessageExportConfig<'a> {
    pub decrypted_dir: &'a Path,
    pub session_query: &'a str,
    pub format: &'a str,
    pub output_dir: &'a Path,
    pub media_dir: Option<&'a Path>,
    pub since: Option<i64>,
    pub limit: Option<usize>,
    pub name_mode: contact::DisplayNameMode,
    pub jobs: usize,
}

struct MessageDisplayContext<'a> {
    chat_name: &'a str,
    contacts: &'a HashMap<String, contact::Contact>,
    name_mode: contact::DisplayNameMode,
    room_member_names: &'a HashMap<String, String>,
}

impl MessageDisplayContext<'_> {
    fn resolve_sender(&self, id: &str) -> String {
        contact::resolve_sender_name_with_mode(
            id,
            self.contacts,
            self.name_mode,
            self.room_member_names,
        )
    }
}

#[derive(Clone)]
struct ImageMessage {
    time: String,
    timestamp: i64,
    raw_content: String,
    packed_info: Vec<u8>,
}

struct ImageCandidate {
    index: usize,
    message: ImageMessage,
    identifier: Option<String>,
    source: Option<PathBuf>,
}

#[derive(Clone)]
struct FileMessage {
    time: String,
    timestamp: i64,
    raw_content: String,
    packed_info: Vec<u8>,
    msg_type: i64,
}

struct FileCandidate {
    index: usize,
    message: FileMessage,
    identifier: Option<String>,
    source: Option<PathBuf>,
}

#[derive(Clone)]
struct VoiceMessage {
    time: String,
    timestamp: i64,
    local_id: i64,
    svr_id: i64,
    voice_data: Vec<u8>,
}

struct VoiceCandidate {
    index: usize,
    message: VoiceMessage,
    format: Option<VoiceFormat>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceOutputFormat {
    Native,
    Wav,
    Pcm,
}

impl VoiceOutputFormat {
    pub fn parse(value: &str) -> Result<Self, String> {
        let normalized = value.trim().to_ascii_lowercase();
        if normalized == native_codec_token() {
            return Ok(Self::Native);
        }
        match normalized.as_str() {
            "native" | "voice" => Ok(Self::Native),
            "wav" => Ok(Self::Wav),
            "pcm" => Ok(Self::Pcm),
            other => Err(format!(
                "Unsupported voice format '{}'; expected native, wav, or pcm",
                other
            )),
        }
    }

    fn extension(self, payload_format: VoiceFormat) -> &'static str {
        match self {
            VoiceOutputFormat::Native => payload_format.extension(),
            VoiceOutputFormat::Wav => "wav",
            VoiceOutputFormat::Pcm => "pcm",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoiceFormat {
    NativeEncoded,
    Amr,
    Raw,
}

impl VoiceFormat {
    fn extension(self) -> &'static str {
        match self {
            VoiceFormat::NativeEncoded => "voice",
            VoiceFormat::Amr => "amr",
            VoiceFormat::Raw => "aud",
        }
    }

    fn label(self) -> &'static str {
        match self {
            VoiceFormat::NativeEncoded => "native",
            VoiceFormat::Amr => "amr",
            VoiceFormat::Raw => "raw",
        }
    }
}

struct VoicePayload<'a> {
    bytes: &'a [u8],
    format: VoiceFormat,
}

/// Export messages for a session.
pub fn export_messages(
    config: MessageExportConfig<'_>,
) -> Result<Vec<(&'static str, PathBuf)>, String> {
    let MessageExportConfig {
        decrypted_dir,
        session_query,
        format,
        output_dir,
        media_dir,
        since,
        limit,
        name_mode,
        jobs,
    } = config;

    let (contact_db, message_dbs) = db::find_decrypted_dbs(decrypted_dir);
    let contact_db_path = contact_db.as_deref();

    // Resolve session username
    let username =
        db::resolve_username_for_messages(session_query, contact_db_path, &message_dbs, jobs)?;

    // Load contacts for display name
    let contacts = contact_db_path
        .and_then(|p| contact::load_contacts(p).ok())
        .unwrap_or_default();
    let display_name = contacts
        .get(&username)
        .map(|c| c.display_name(name_mode))
        .unwrap_or(&username);
    let room_member_names = if name_mode == contact::DisplayNameMode::Anonymous {
        contact_db_path
            .and_then(|p| contact::load_chat_room_member_names(p, &username).ok())
            .unwrap_or_default()
    } else {
        HashMap::new()
    };
    let display_context = MessageDisplayContext {
        chat_name: display_name,
        contacts: &contacts,
        name_mode,
        room_member_names: &room_member_names,
    };

    // Table name
    let table_name = db::msg_table_name(&username);
    let since_clause = since
        .map(|ts| format!(" AND create_time >= {}", ts))
        .unwrap_or_default();
    let order_dir = if limit.is_some() { "DESC" } else { "ASC" };
    let limit_clause = limit.map(|n| format!(" LIMIT {}", n)).unwrap_or_default();

    let mut used_index = false;
    let mut all_messages: Vec<ExportMessage> = if let Some(since_ts) = since {
        match message_index::open_existing_recent(decrypted_dir) {
            Ok(Some(index)) if index.covers(since_ts) => {
                match load_indexed_export_messages(
                    &index,
                    &username,
                    &display_context,
                    since_ts,
                    limit,
                ) {
                    Ok(messages) => {
                        used_index = true;
                        messages
                    }
                    Err(e) => {
                        log::warn!("Message index export failed; falling back: {}", e);
                        Vec::new()
                    }
                }
            }
            Ok(_) => Vec::new(),
            Err(e) => {
                log::warn!("Message index read failed; falling back: {}", e);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let db_jobs = parallel::job_count(jobs, 8);
    let per_db_messages = if used_index {
        Vec::new()
    } else {
        parallel::map_ordered(message_dbs.clone(), db_jobs, |db_path| {
            let mut messages = Vec::new();
            let conn = match Connection::open(&db_path) {
                Ok(c) => c,
                Err(_) => return messages,
            };

            let body_col = dictionary::msg_body_column();
            let marker_col = dictionary::msg_compression_marker_column();
            let packed_col = dictionary::msg_packed_meta_column();
            let table = db::quote_identifier(&table_name);
            let sql = format!(
            "SELECT local_type, create_time, {body_col}, {marker_col}, {packed_col} \
             FROM {table} WHERE create_time > 0{since_clause} ORDER BY create_time {order_dir}{limit_clause}"
        );

            let rows: Vec<ExportMessageRow> = match conn.prepare(&sql) {
                Ok(mut stmt) => match stmt.query_map([], |row| {
                    let compression_marker: Option<i64> = row.get::<_, Option<i64>>(3)?;
                    let content: String = if compression_marker == Some(4) {
                        if let Ok(b) = row.get::<_, Vec<u8>>(2) {
                            message::try_decompress(&b).unwrap_or_default()
                        } else {
                            String::new()
                        }
                    } else {
                        match row.get::<_, Option<String>>(2) {
                            Ok(Some(s)) => s,
                            _ => match row.get::<_, Option<Vec<u8>>>(2) {
                                Ok(Some(b)) => String::from_utf8(b).unwrap_or_default(),
                                _ => String::new(),
                            },
                        }
                    };
                    let packed_info: Vec<u8> =
                        row.get::<_, Option<Vec<u8>>>(4)?.unwrap_or_default();
                    Ok((
                        row.get::<_, Option<i64>>(0)?.unwrap_or(-1),
                        row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        content,
                        compression_marker,
                        packed_info,
                    ))
                }) {
                    Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                    Err(_) => vec![],
                },
                Err(_) => vec![],
            };

            for (local_type, create_time, content, compression_marker, packed_info) in rows {
                let time_str = time::format_local_timestamp(create_time);

                let decoded = message::decode_message(
                    local_type as i32,
                    &content,
                    display_context.chat_name,
                    compression_marker,
                    &packed_info,
                    |id| display_context.resolve_sender(id),
                );

                messages.push(ExportMessage {
                    time: time_str,
                    timestamp: create_time,
                    sender: decoded.display_name,
                    msg_type: local_type,
                    type_name: decoded.msg_type.to_string(),
                    content: decoded.content,
                    raw_content: content,
                    packed_info,
                });
            }
            messages
        })
    };

    for messages in per_db_messages {
        all_messages.extend(messages);
    }

    all_messages.sort_by_key(|message| message.timestamp);
    if let Some(limit) = limit {
        if all_messages.len() > limit {
            all_messages.sort_by_key(|message| Reverse(message.timestamp));
            all_messages.truncate(limit);
            all_messages.sort_by_key(|message| message.timestamp);
        }
    }

    if all_messages.is_empty() {
        return Err(format!("No messages found for '{}'", username));
    }

    // Create output directory
    std::fs::create_dir_all(output_dir).map_err(|e| format!("Cannot create output dir: {}", e))?;

    let mut results = Vec::new();

    match format {
        "txt" => {
            let path = output_dir.join("chat.txt");
            export_txt(&path, &username, display_name, &all_messages)?;
            results.push(("txt", path));
        }
        "csv" => {
            let path = output_dir.join("chat.csv");
            export_csv(&path, &all_messages)?;
            results.push(("csv", path));
        }
        "json" => {
            let path = output_dir.join("chat.json");
            export_json(&path, &all_messages)?;
            results.push(("json", path));
        }
        _ => {
            // Default: export all formats
            let path_txt = output_dir.join("chat.txt");
            export_txt(&path_txt, &username, display_name, &all_messages)?;
            results.push(("txt", path_txt));

            let path_csv = output_dir.join("chat.csv");
            export_csv(&path_csv, &all_messages)?;
            results.push(("csv", path_csv));

            let path_json = output_dir.join("chat.json");
            export_json(&path_json, &all_messages)?;
            results.push(("json", path_json));
        }
    }

    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());

    // Export media files if requested
    if let Some(mdir) = media_dir {
        let telegram_base = match media::find_telegram_base_path() {
            Some(p) => p,
            None => {
                log::warn!("Telegram data directory not found, media export skipped");
                return Ok(results);
            }
        };

        // Derive media decryption keys (V2 .dat)
        let media_keys = crate::media_key::find_media_keys(&telegram_base);
        if let Err(ref e) = media_keys {
            log::warn!("Cannot derive media decryption keys: {}", e);
        }
        let media_keys = media_keys.ok();
        let mut index_categories = vec!["Image", "Video"];
        if all_messages.iter().any(is_file_message) {
            index_categories.push("File");
        }
        let media_index = MediaIndex::load(&telegram_base, &username, &index_categories, jobs);

        let mut media_jobs = Vec::new();
        let mut next_index = 1usize;
        let category_map = [("Image", 3), ("Video", 43)];

        for (cat_name, local_type) in &category_map {
            for m in &all_messages {
                if m.msg_type != *local_type {
                    continue;
                }
                // Try protobuf filename first, then fall back to XML aeskey
                let identifier = try_protobuf_identifier(&m.packed_info)
                    .or_else(|| extract_xml_attr_str(&m.raw_content, "aeskey"))
                    .or_else(|| extract_xml_attr_str(&m.raw_content, "cdnthumburl"));
                let identifier = match identifier {
                    Some(ref s) if !s.is_empty() => s.clone(),
                    _ => continue,
                };

                if let Some(src) = media_index.find(cat_name, &identifier) {
                    media_jobs.push(MediaExportJob::Cached {
                        index: next_index,
                        src,
                        category: cat_name,
                        msg_type: m.msg_type,
                    });
                    next_index += 1;
                }
            }
        }

        for m in &all_messages {
            if m.msg_type != 47 {
                continue;
            }

            if !has_sticker_export_source(m) {
                continue;
            }

            media_jobs.push(MediaExportJob::Sticker {
                index: next_index,
                message: m.clone(),
            });
            next_index += 1;
        }

        for m in &all_messages {
            if !is_file_message(m) {
                continue;
            }

            let Some(identifier) = file_identifier(m) else {
                continue;
            };

            if let Some(src) = media_index.find("File", &identifier) {
                media_jobs.push(MediaExportJob::Cached {
                    index: next_index,
                    src,
                    category: "File",
                    msg_type: file_export_type(m.msg_type),
                });
                next_index += 1;
            }
        }

        let media_job_count = parallel::job_count(jobs, 4);
        let media_results = parallel::map_ordered(media_jobs, media_job_count, |job| match job {
            MediaExportJob::Cached {
                index,
                src,
                category,
                msg_type,
            } => MediaExportResult {
                index,
                is_sticker: false,
                result: export_media_with_decrypt(
                    &src,
                    mdir,
                    &username,
                    category,
                    msg_type,
                    index,
                    media_keys.as_ref(),
                ),
            },
            MediaExportJob::Sticker { index, message } => MediaExportResult {
                index,
                is_sticker: true,
                result: export_sticker_message(&message, &telegram_base, mdir, &username, index),
            },
        });

        let mut exported = 0;
        for media_result in media_results {
            match media_result.result {
                Ok(path) => {
                    out.line(format_args!(
                        "  Media #{}: {}",
                        media_result.index,
                        path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
                    ))?;
                    exported += 1;
                }
                Err(e) => {
                    if media_result.is_sticker {
                        log::warn!("  Sticker #{} export failed: {}", media_result.index, e);
                    } else {
                        log::warn!("  Media #{} export failed: {}", media_result.index, e);
                    }
                }
            }
        }

        if exported > 0 {
            out.line(format_args!("Exported {} media files", exported))?;
        }
    }

    out.line(format_args!(
        "Exported {} messages for {} ({})",
        all_messages.len(),
        display_context.chat_name,
        username
    ))?;
    out.flush()?;
    Ok(results)
}

fn load_indexed_export_messages(
    index: &message_index::HotIndex,
    username: &str,
    display_context: &MessageDisplayContext<'_>,
    since: i64,
    limit: Option<usize>,
) -> Result<Vec<ExportMessage>, String> {
    let conn = Connection::open(&index.path)
        .map_err(|e| format!("Cannot open message index {}: {}", index.path.display(), e))?;
    let limit_clause = limit.map(|n| format!(" LIMIT {}", n)).unwrap_or_default();
    let sql = format!(
        "SELECT local_type, create_time, body, marker, packed_info
         FROM messages
         WHERE session_id = ?1 AND create_time >= ?2
         ORDER BY create_time DESC{limit_clause}"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("Prepare indexed export: {}", e))?;
    let rows = stmt
        .query_map(params![username, since], |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?.unwrap_or(-1),
                row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?.unwrap_or_default(),
            ))
        })
        .map_err(|e| format!("Read indexed export: {}", e))?;

    let mut messages = rows
        .filter_map(|row| row.ok())
        .map(
            |(local_type, create_time, content, compression_marker, packed_info)| {
                let decoded = message::decode_message(
                    local_type as i32,
                    &content,
                    display_context.chat_name,
                    compression_marker,
                    &packed_info,
                    |id| display_context.resolve_sender(id),
                );
                ExportMessage {
                    time: time::format_local_timestamp(create_time),
                    timestamp: create_time,
                    sender: decoded.display_name,
                    msg_type: local_type,
                    type_name: decoded.msg_type.to_string(),
                    content: decoded.content,
                    raw_content: content,
                    packed_info,
                }
            },
        )
        .collect::<Vec<_>>();
    messages.sort_by_key(|message| message.timestamp);
    Ok(messages)
}

/// Export readable image files for a session.
pub fn export_images(
    decrypted_dir: &Path,
    session_query: &str,
    config: ImageExportConfig<'_>,
) -> Result<Vec<PathBuf>, String> {
    if config.limit == 0 {
        return Err("--limit must be greater than 0".to_string());
    }
    if config.id.is_some() && (config.list || config.all || config.index.is_some()) {
        return Err("--id cannot be used with --list, --all, or --index".to_string());
    }
    if let Some(index) = config.index {
        if index == 0 {
            return Err("--index is 1-based and must be greater than 0".to_string());
        }
    }
    if let Some(identifier) = config.id {
        return export_image_by_identifier(decrypted_dir, session_query, identifier, &config);
    }

    let scan_limit = config.limit.max(config.index.unwrap_or(1));
    let (username, messages) = load_image_messages(
        decrypted_dir,
        session_query,
        config.since,
        scan_limit,
        config.jobs,
    )?;
    if messages.is_empty() {
        return Err(format!("No image messages found for '{}'", username));
    }

    let telegram_base = media::find_telegram_base_path()
        .ok_or_else(|| "Telegram data directory not found".to_string())?;
    let media_index = MediaIndex::load(&telegram_base, &username, &["Image"], config.jobs);

    let candidates: Vec<ImageCandidate> = messages
        .into_iter()
        .enumerate()
        .map(|(i, message)| {
            let identifier = image_identifier(&message);
            let source = identifier
                .as_deref()
                .and_then(|id| media_index.find("Image", id));
            ImageCandidate {
                index: i + 1,
                message,
                identifier,
                source,
            }
        })
        .collect();

    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());

    if config.list {
        out.line(format_args!(
            "{:<5} {:<19} {:<8} Source",
            "Index", "Time", "Status"
        ))?;
        out.line(format_args!("{}", "-".repeat(100)))?;
        for candidate in &candidates {
            let status = if candidate.source.is_some() {
                "cached"
            } else {
                "missing"
            };
            let source = candidate
                .source
                .as_ref()
                .map(|p| p.display().to_string())
                .or_else(|| candidate.identifier.clone())
                .unwrap_or_else(|| "(no identifier)".to_string());
            out.line(format_args!(
                "{:<5} {:<19} {:<8} {}",
                candidate.index, candidate.message.time, status, source
            ))?;
        }
        out.flush()?;
        return Ok(Vec::new());
    }

    let selected = if config.all {
        candidates
            .into_iter()
            .filter(|candidate| candidate.source.is_some())
            .take(config.limit)
            .collect::<Vec<_>>()
    } else if let Some(index) = config.index {
        let Some(candidate) = candidates
            .into_iter()
            .find(|candidate| candidate.index == index)
        else {
            return Err(format!(
                "Image index {} is outside the scanned window",
                index
            ));
        };
        if candidate.source.is_none() {
            return Err(format!(
                "Image #{} is not available in local Telegram cache",
                index
            ));
        }
        vec![candidate]
    } else {
        let Some(candidate) = candidates
            .into_iter()
            .find(|candidate| candidate.source.is_some())
        else {
            return Err(format!(
                "No locally cached images found in the latest {} image messages",
                config.limit
            ));
        };
        vec![candidate]
    };

    if selected.is_empty() {
        return Err(format!(
            "No locally cached images found in the latest {} image messages",
            config.limit
        ));
    }

    let images = selected
        .into_iter()
        .filter_map(|candidate| {
            candidate.source.map(|src| CachedImageSource {
                index: candidate.index,
                src,
            })
        })
        .collect::<Vec<_>>();

    export_cached_images(
        &telegram_base,
        &username,
        config.output_dir,
        config.jobs,
        images,
    )
}

fn export_image_by_identifier(
    decrypted_dir: &Path,
    session_query: &str,
    identifier: &str,
    config: &ImageExportConfig<'_>,
) -> Result<Vec<PathBuf>, String> {
    let identifier = normalize_image_id(identifier)?;
    let (username, _) = resolve_image_session(decrypted_dir, session_query, config.jobs)?;
    let telegram_base = media::find_telegram_base_path()
        .ok_or_else(|| "Telegram data directory not found".to_string())?;
    let media_index = MediaIndex::load(&telegram_base, &username, &["Image"], config.jobs);
    let Some(src) = media_index.find("Image", &identifier) else {
        return Err(format!(
            "Image id '{}' is not available in local Telegram cache",
            identifier
        ));
    };

    export_cached_images(
        &telegram_base,
        &username,
        config.output_dir,
        config.jobs,
        vec![CachedImageSource { index: 1, src }],
    )
}

struct CachedImageSource {
    index: usize,
    src: PathBuf,
}

fn export_cached_images(
    telegram_base: &Path,
    username: &str,
    output_dir: &Path,
    jobs: usize,
    images: Vec<CachedImageSource>,
) -> Result<Vec<PathBuf>, String> {
    let media_keys = crate::media_key::find_media_keys(telegram_base);
    if let Err(ref e) = media_keys {
        log::warn!("Cannot derive media decryption keys: {}", e);
    }
    let media_keys = media_keys.ok();

    let image_jobs = images
        .into_iter()
        .map(|image| MediaExportJob::Cached {
            index: image.index,
            src: image.src,
            category: "Image",
            msg_type: 3,
        })
        .collect::<Vec<_>>();
    let image_job_count = parallel::job_count(jobs, 4);
    let image_results = parallel::map_ordered(image_jobs, image_job_count, |job| match job {
        MediaExportJob::Cached {
            index,
            src,
            category,
            msg_type,
        } => MediaExportResult {
            index,
            is_sticker: false,
            result: export_media_with_decrypt(
                &src,
                output_dir,
                username,
                category,
                msg_type,
                index,
                media_keys.as_ref(),
            ),
        },
        MediaExportJob::Sticker { .. } => unreachable!("image command only exports images"),
    });

    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());
    let mut paths = Vec::new();
    for image_result in image_results {
        match image_result.result {
            Ok(path) => {
                out.line(format_args!("{}", path.display()))?;
                paths.push(path);
            }
            Err(e) => log::warn!("Image #{} export failed: {}", image_result.index, e),
        }
    }
    out.flush()?;

    if paths.is_empty() {
        return Err("No images were exported".to_string());
    }

    Ok(paths)
}

fn normalize_image_id(identifier: &str) -> Result<String, String> {
    let identifier = identifier.trim();
    if identifier.is_empty() {
        Err("--id must not be empty".to_string())
    } else {
        Ok(identifier.to_string())
    }
}

fn resolve_image_session(
    decrypted_dir: &Path,
    session_query: &str,
    jobs: usize,
) -> Result<(String, Vec<PathBuf>), String> {
    let (contact_db, message_dbs) = db::find_decrypted_dbs(decrypted_dir);
    let username = db::resolve_username_for_messages(
        session_query,
        contact_db.as_deref(),
        &message_dbs,
        jobs,
    )?;
    Ok((username, message_dbs))
}

fn load_image_messages(
    decrypted_dir: &Path,
    session_query: &str,
    since: Option<i64>,
    limit: usize,
    jobs: usize,
) -> Result<(String, Vec<ImageMessage>), String> {
    let (username, message_dbs) = resolve_image_session(decrypted_dir, session_query, jobs)?;
    let table_name = db::msg_table_name(&username);
    let since_clause = since
        .map(|ts| format!(" AND create_time >= {}", ts))
        .unwrap_or_default();

    let db_jobs = parallel::job_count(jobs, 8);
    let per_db_messages = parallel::map_ordered(message_dbs, db_jobs, |db_path| {
        let mut messages = Vec::new();
        let conn = match Connection::open(&db_path) {
            Ok(c) => c,
            Err(_) => return messages,
        };

        let body_col = dictionary::msg_body_column();
        let marker_col = dictionary::msg_compression_marker_column();
        let packed_col = dictionary::msg_packed_meta_column();
        let table = db::quote_identifier(&table_name);
        let sql = format!(
            "SELECT create_time, {body_col}, {marker_col}, {packed_col} \
             FROM {table} WHERE create_time > 0 AND local_type = 3{since_clause} ORDER BY create_time DESC LIMIT {limit}"
        );

        let rows: Vec<(i64, String, Vec<u8>)> = match conn.prepare(&sql) {
            Ok(mut stmt) => match stmt.query_map([], |row| {
                let compression_marker: Option<i64> = row.get::<_, Option<i64>>(2)?;
                let content: String = if compression_marker == Some(4) {
                    if let Ok(b) = row.get::<_, Vec<u8>>(1) {
                        message::try_decompress(&b).unwrap_or_default()
                    } else {
                        String::new()
                    }
                } else {
                    match row.get::<_, Option<String>>(1) {
                        Ok(Some(s)) => s,
                        _ => match row.get::<_, Option<Vec<u8>>>(1) {
                            Ok(Some(b)) => String::from_utf8(b).unwrap_or_default(),
                            _ => String::new(),
                        },
                    }
                };
                let packed_info: Vec<u8> = row.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default();
                Ok((
                    row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    content,
                    packed_info,
                ))
            }) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(_) => vec![],
            },
            Err(_) => vec![],
        };

        for (timestamp, raw_content, packed_info) in rows {
            let time = time::format_local_timestamp(timestamp);
            messages.push(ImageMessage {
                time,
                timestamp,
                raw_content,
                packed_info,
            });
        }
        messages
    });

    let mut messages = Vec::new();
    for db_messages in per_db_messages {
        messages.extend(db_messages);
    }
    messages.sort_by_key(|message| Reverse(message.timestamp));
    messages.truncate(limit);

    Ok((username, messages))
}

fn load_file_messages(
    decrypted_dir: &Path,
    session_query: &str,
    since: Option<i64>,
    limit: usize,
    jobs: usize,
) -> Result<(String, Vec<FileMessage>), String> {
    let (username, message_dbs) = resolve_image_session(decrypted_dir, session_query, jobs)?;
    let table_name = db::msg_table_name(&username);
    let since_clause = since
        .map(|ts| format!(" AND create_time >= {}", ts))
        .unwrap_or_default();

    let db_jobs = parallel::job_count(jobs, 8);
    let per_db_messages = parallel::map_ordered(message_dbs, db_jobs, |db_path| {
        let mut messages = Vec::new();
        let conn = match Connection::open(&db_path) {
            Ok(c) => c,
            Err(_) => return messages,
        };

        let body_col = dictionary::msg_body_column();
        let marker_col = dictionary::msg_compression_marker_column();
        let packed_col = dictionary::msg_packed_meta_column();
        let table = db::quote_identifier(&table_name);
        let app_file_type = app_file_local_type();
        let app_file_alt_type = app_local_type(62);
        let sql = format!(
            "SELECT local_type, create_time, {body_col}, {marker_col}, {packed_col} \
             FROM {table} \
             WHERE create_time > 0 \
               AND (local_type = 62 \
                    OR local_type = {app_file_type} \
                    OR local_type = {app_file_alt_type} \
                    OR (local_type = 49 AND {marker_col} IS NOT 4 \
                        AND ({body_col} LIKE '%<type>6</type>%' OR {body_col} LIKE '%<type>62</type>%')))\
               {since_clause} \
             ORDER BY create_time DESC LIMIT {limit}"
        );

        let rows: Vec<(i64, i64, String, Vec<u8>)> = match conn.prepare(&sql) {
            Ok(mut stmt) => match stmt.query_map([], |row| {
                let compression_marker: Option<i64> = row.get::<_, Option<i64>>(3)?;
                let content: String = if compression_marker == Some(4) {
                    if let Ok(b) = row.get::<_, Vec<u8>>(2) {
                        message::try_decompress(&b).unwrap_or_default()
                    } else {
                        String::new()
                    }
                } else {
                    match row.get::<_, Option<String>>(2) {
                        Ok(Some(s)) => s,
                        _ => match row.get::<_, Option<Vec<u8>>>(2) {
                            Ok(Some(b)) => String::from_utf8(b).unwrap_or_default(),
                            _ => String::new(),
                        },
                    }
                };
                let packed_info: Vec<u8> = row.get::<_, Option<Vec<u8>>>(4)?.unwrap_or_default();
                Ok((
                    row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    content,
                    packed_info,
                ))
            }) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(_) => vec![],
            },
            Err(_) => vec![],
        };

        for (msg_type, timestamp, raw_content, packed_info) in rows {
            if !is_file_message_parts(msg_type, &raw_content) {
                continue;
            }
            let time = time::format_local_timestamp(timestamp);
            messages.push(FileMessage {
                time,
                timestamp,
                raw_content,
                packed_info,
                msg_type,
            });
        }
        messages
    });

    let mut messages = Vec::new();
    for db_messages in per_db_messages {
        messages.extend(db_messages);
    }
    messages.sort_by_key(|message| Reverse(message.timestamp));
    messages.truncate(limit);

    Ok((username, messages))
}

fn image_identifier(message: &ImageMessage) -> Option<String> {
    try_protobuf_identifier(&message.packed_info)
        .or_else(|| extract_xml_attr_str(&message.raw_content, "aeskey"))
        .or_else(|| extract_xml_attr_str(&message.raw_content, "cdnthumburl"))
}

/// Try to extract media cache identifier from packed_info protobuf.
fn try_protobuf_identifier(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }
    if let Some(v2) = crate::media_pb::parse_img2(data) {
        if let Some(img) = v2.image {
            if !img.filename.is_empty() {
                return Some(img.filename);
            }
        }
        if let Some(vid) = v2.video {
            if !vid.filename.is_empty() {
                return Some(vid.filename);
            }
        }
    }
    if let Some(v1) = crate::media_pb::parse_img(data) {
        if !v1.filename.is_empty() {
            return Some(v1.filename);
        }
    }
    None
}

fn try_protobuf_file_identifier(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }
    let v2 = crate::media_pb::parse_img2(data)?;
    let file = v2.file?;
    crate::media_pb::file_identifier(&file).map(ToString::to_string)
}

fn file_identifier(message: &ExportMessage) -> Option<String> {
    file_identifier_from_parts(&message.packed_info, &message.raw_content)
}

fn file_message_identifier(message: &FileMessage) -> Option<String> {
    file_identifier_from_parts(&message.packed_info, &message.raw_content)
}

fn file_identifier_from_parts(packed_info: &[u8], raw_content: &str) -> Option<String> {
    try_protobuf_file_identifier(packed_info)
        .or_else(|| {
            media::parse_file_info(raw_content)
                .and_then(|info| (!info.title.trim().is_empty()).then_some(info.title))
        })
        .or_else(|| file_path_basename(raw_content))
}

fn file_path_basename(content: &str) -> Option<String> {
    let content = content.trim();
    if content.is_empty() || content.starts_with('<') {
        return None;
    }
    let name = content.rsplit('/').next().unwrap_or(content).trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn is_file_message(message: &ExportMessage) -> bool {
    is_file_message_parts(message.msg_type, &message.raw_content)
}

fn is_file_message_parts(msg_type: i64, raw_content: &str) -> bool {
    msg_type == 62
        || matches!(app_message_subtype(msg_type), Some(6 | 62))
        || (((msg_type as u64) & 0xffff_ffff) == 49
            && matches!(
                media::extract_xml_tag_int(raw_content, "type"),
                Some(6 | 62)
            ))
}

fn app_message_subtype(local_type: i64) -> Option<i64> {
    let encoded = local_type as u64;
    if encoded & 0xffff_ffff != 49 {
        return None;
    }
    let subtype = encoded >> 32;
    if subtype == 0 {
        None
    } else {
        Some(subtype as i64)
    }
}

fn file_export_type(local_type: i64) -> i64 {
    if local_type == 62 || app_message_subtype(local_type) == Some(62) {
        62
    } else {
        49
    }
}

fn app_file_local_type() -> i64 {
    app_local_type(6)
}

fn app_local_type(subtype: i64) -> i64 {
    (subtype << 32) | 49
}

/// Export cached file attachments for a session.
pub fn export_files(
    decrypted_dir: &Path,
    session_query: &str,
    config: FileExportConfig<'_>,
) -> Result<Vec<PathBuf>, String> {
    if config.limit == 0 {
        return Err("--limit must be greater than 0".to_string());
    }
    if config.id.is_some() && (config.list || config.all || config.index.is_some()) {
        return Err("--id cannot be used with --list, --all, or --index".to_string());
    }
    if let Some(index) = config.index {
        if index == 0 {
            return Err("--index is 1-based and must be greater than 0".to_string());
        }
    }
    if let Some(identifier) = config.id {
        return export_file_by_identifier(decrypted_dir, session_query, identifier, &config);
    }

    let scan_limit = config.limit.max(config.index.unwrap_or(1));
    let (username, messages) = load_file_messages(
        decrypted_dir,
        session_query,
        config.since,
        scan_limit,
        config.jobs,
    )?;
    if messages.is_empty() {
        return Err(format!("No file messages found for '{}'", username));
    }

    let telegram_base = media::find_telegram_base_path()
        .ok_or_else(|| "Telegram data directory not found".to_string())?;
    let media_index = MediaIndex::load(&telegram_base, &username, &["File"], config.jobs);

    let candidates = messages
        .into_iter()
        .enumerate()
        .map(|(i, message)| {
            let identifier = file_message_identifier(&message);
            let source = identifier
                .as_deref()
                .and_then(|id| media_index.find("File", id));
            FileCandidate {
                index: i + 1,
                message,
                identifier,
                source,
            }
        })
        .collect::<Vec<_>>();

    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());

    if config.list {
        out.line(format_args!(
            "{:<5} {:<19} {:<8} {:>9} Source",
            "Index", "Time", "Status", "Size"
        ))?;
        out.line(format_args!("{}", "-".repeat(100)))?;
        for candidate in &candidates {
            let status = if candidate.source.is_some() {
                "cached"
            } else {
                "missing"
            };
            let source = candidate
                .source
                .as_ref()
                .map(|p| p.display().to_string())
                .or_else(|| candidate.identifier.clone())
                .unwrap_or_else(|| "(no identifier)".to_string());
            out.line(format_args!(
                "{:<5} {:<19} {:<8} {:>9} {}",
                candidate.index,
                candidate.message.time,
                status,
                file_size_label(&candidate.message),
                source
            ))?;
        }
        out.flush()?;
        return Ok(Vec::new());
    }

    let selected = if config.all {
        candidates
            .into_iter()
            .filter(|candidate| candidate.source.is_some())
            .take(config.limit)
            .collect::<Vec<_>>()
    } else if let Some(index) = config.index {
        let Some(candidate) = candidates
            .into_iter()
            .find(|candidate| candidate.index == index)
        else {
            return Err(format!(
                "File index {} is outside the scanned window",
                index
            ));
        };
        if candidate.source.is_none() {
            return Err(format!(
                "File #{} is not available in local Telegram cache",
                index
            ));
        }
        vec![candidate]
    } else {
        let Some(candidate) = candidates
            .into_iter()
            .find(|candidate| candidate.source.is_some())
        else {
            return Err(format!(
                "No locally cached files found in the latest {} file messages",
                config.limit
            ));
        };
        vec![candidate]
    };

    if selected.is_empty() {
        return Err(format!(
            "No locally cached files found in the latest {} file messages",
            config.limit
        ));
    }

    let files = selected
        .into_iter()
        .filter_map(|candidate| {
            candidate.source.map(|src| CachedFileSource {
                index: candidate.index,
                src,
                identifier: candidate.identifier,
                msg_type: file_export_type(candidate.message.msg_type),
            })
        })
        .collect::<Vec<_>>();

    export_cached_files(&username, config.output_dir, config.jobs, files)
}

fn export_file_by_identifier(
    decrypted_dir: &Path,
    session_query: &str,
    identifier: &str,
    config: &FileExportConfig<'_>,
) -> Result<Vec<PathBuf>, String> {
    let identifier = normalize_file_id(identifier)?;
    let (username, _) = resolve_image_session(decrypted_dir, session_query, config.jobs)?;
    let telegram_base = media::find_telegram_base_path()
        .ok_or_else(|| "Telegram data directory not found".to_string())?;
    let media_index = MediaIndex::load(&telegram_base, &username, &["File"], config.jobs);
    let Some(src) = media_index.find("File", &identifier) else {
        return Err(format!(
            "File id '{}' is not available in local Telegram cache",
            identifier
        ));
    };

    export_cached_files(
        &username,
        config.output_dir,
        config.jobs,
        vec![CachedFileSource {
            index: 1,
            src,
            identifier: Some(identifier),
            msg_type: 49,
        }],
    )
}

fn normalize_file_id(identifier: &str) -> Result<String, String> {
    let identifier = identifier.trim();
    if identifier.is_empty() {
        Err("--id must not be empty".to_string())
    } else {
        Ok(identifier.to_string())
    }
}

fn file_size_label(message: &FileMessage) -> String {
    media::parse_file_info(&message.raw_content)
        .map(|info| media::format_file_size(info.total_len))
        .filter(|size| !size.is_empty())
        .unwrap_or_else(|| "-".to_string())
}

struct CachedFileSource {
    index: usize,
    src: PathBuf,
    identifier: Option<String>,
    msg_type: i64,
}

fn export_cached_files(
    username: &str,
    output_dir: &Path,
    jobs: usize,
    files: Vec<CachedFileSource>,
) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(output_dir).map_err(|e| format!("Cannot create file dir: {}", e))?;

    let file_job_count = parallel::job_count(jobs, 4);
    let file_results = parallel::map_ordered(files, file_job_count, |file| {
        let index = file.index;
        MediaExportResult {
            index,
            is_sticker: false,
            result: export_file_attachment(output_dir, username, file),
        }
    });

    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());
    let mut paths = Vec::new();
    for file_result in file_results {
        match file_result.result {
            Ok(path) => {
                out.line(format_args!("{}", path.display()))?;
                paths.push(path);
            }
            Err(e) => log::warn!("File #{} export failed: {}", file_result.index, e),
        }
    }
    out.flush()?;

    if paths.is_empty() {
        return Err("No files were exported".to_string());
    }

    Ok(paths)
}

fn export_file_attachment(
    output_dir: &Path,
    session_name: &str,
    file: CachedFileSource,
) -> Result<PathBuf, String> {
    let ext = file_extension(&file.src, file.identifier.as_deref());
    let filename = format!(
        "{}_File_{}_{:04}.{}",
        media::sanitize_filename(session_name),
        file.msg_type,
        file.index,
        ext
    );
    let dest = output_dir.join(filename);
    std::fs::copy(&file.src, &dest).map_err(|e| {
        format!(
            "Cannot copy file {} to {}: {}",
            file.src.display(),
            dest.display(),
            e
        )
    })?;
    Ok(dest)
}

fn file_extension(src: &Path, identifier: Option<&str>) -> String {
    src.extension()
        .and_then(OsStr::to_str)
        .and_then(clean_file_extension)
        .or_else(|| {
            identifier
                .and_then(|id| Path::new(id).extension())
                .and_then(OsStr::to_str)
                .and_then(clean_file_extension)
        })
        .unwrap_or_else(|| "bin".to_string())
}

fn clean_file_extension(ext: &str) -> Option<String> {
    let ext = ext.trim().trim_start_matches('.');
    if ext.is_empty() || ext.len() > 24 || !ext.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        None
    } else {
        Some(ext.to_ascii_lowercase())
    }
}

/// Export cached voice messages for a session.
pub fn export_voices(
    decrypted_dir: &Path,
    session_query: &str,
    config: VoiceExportConfig<'_>,
) -> Result<Vec<PathBuf>, String> {
    if config.limit == 0 {
        return Err("--limit must be greater than 0".to_string());
    }
    if config.id.is_some() && (config.list || config.all || config.index.is_some()) {
        return Err("--id cannot be used with --list, --all, or --index".to_string());
    }
    if let Some(index) = config.index {
        if index == 0 {
            return Err("--index is 1-based and must be greater than 0".to_string());
        }
    }
    if let Some(id) = config.id {
        let (username, message) =
            load_voice_message_by_id(decrypted_dir, session_query, id, config.jobs)?;
        return export_cached_voices(
            &username,
            config.output_dir,
            config.format,
            config.decoder,
            config.sample_rate,
            vec![CachedVoiceSource { index: 1, message }],
        );
    }

    let scan_limit = config.limit.max(config.index.unwrap_or(1));
    let (username, messages) = load_voice_messages(
        decrypted_dir,
        session_query,
        config.since,
        scan_limit,
        config.jobs,
    )?;
    if messages.is_empty() {
        return Err(format!("No voice messages found for '{}'", username));
    }

    let candidates = messages
        .into_iter()
        .enumerate()
        .map(|(i, message)| {
            let format = voice_payload(&message.voice_data).map(|payload| payload.format);
            VoiceCandidate {
                index: i + 1,
                message,
                format,
            }
        })
        .collect::<Vec<_>>();

    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());

    if config.list {
        out.line(format_args!(
            "{:<5} {:<10} {:<19} {:<8} {:>9} Format",
            "Index", "ID", "Time", "Status", "Bytes"
        ))?;
        out.line(format_args!("{}", "-".repeat(78)))?;
        for candidate in &candidates {
            let status = if candidate.format.is_some() {
                "cached"
            } else {
                "empty"
            };
            let format = candidate.format.map(VoiceFormat::label).unwrap_or("-");
            out.line(format_args!(
                "{:<5} {:<10} {:<19} {:<8} {:>9} {}",
                candidate.index,
                candidate.message.local_id,
                candidate.message.time,
                status,
                candidate.message.voice_data.len(),
                format
            ))?;
        }
        out.flush()?;
        return Ok(Vec::new());
    }

    let selected = if config.all {
        candidates
            .into_iter()
            .filter(|candidate| candidate.format.is_some())
            .take(config.limit)
            .collect::<Vec<_>>()
    } else if let Some(index) = config.index {
        let Some(candidate) = candidates
            .into_iter()
            .find(|candidate| candidate.index == index)
        else {
            return Err(format!(
                "Voice index {} is outside the scanned window",
                index
            ));
        };
        if candidate.format.is_none() {
            return Err(format!("Voice #{} has no local audio data", index));
        }
        vec![candidate]
    } else {
        let Some(candidate) = candidates
            .into_iter()
            .find(|candidate| candidate.format.is_some())
        else {
            return Err(format!(
                "No local voice data found in the latest {} voice messages",
                config.limit
            ));
        };
        vec![candidate]
    };

    if selected.is_empty() {
        return Err(format!(
            "No local voice data found in the latest {} voice messages",
            config.limit
        ));
    }

    let voices = selected
        .into_iter()
        .map(|candidate| CachedVoiceSource {
            index: candidate.index,
            message: candidate.message,
        })
        .collect::<Vec<_>>();

    export_cached_voices(
        &username,
        config.output_dir,
        config.format,
        config.decoder,
        config.sample_rate,
        voices,
    )
}

struct CachedVoiceSource {
    index: usize,
    message: VoiceMessage,
}

fn export_cached_voices(
    username: &str,
    output_dir: &Path,
    output_format: VoiceOutputFormat,
    decoder: Option<&Path>,
    sample_rate: u32,
    voices: Vec<CachedVoiceSource>,
) -> Result<Vec<PathBuf>, String> {
    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());
    let mut paths = Vec::new();

    for voice in voices {
        match export_voice_file(
            output_dir,
            username,
            voice.index,
            &voice.message,
            output_format,
            decoder,
            sample_rate,
        ) {
            Ok(path) => {
                out.line(format_args!("{}", path.display()))?;
                paths.push(path);
            }
            Err(e) => log::warn!("Voice #{} export failed: {}", voice.index, e),
        }
    }
    out.flush()?;

    if paths.is_empty() {
        return Err("No voices were exported".to_string());
    }

    Ok(paths)
}

fn export_voice_file(
    output_dir: &Path,
    session_name: &str,
    index: usize,
    message: &VoiceMessage,
    output_format: VoiceOutputFormat,
    decoder: Option<&Path>,
    sample_rate: u32,
) -> Result<PathBuf, String> {
    let payload =
        voice_payload(&message.voice_data).ok_or_else(|| "Voice data is empty".to_string())?;
    std::fs::create_dir_all(output_dir).map_err(|e| format!("Cannot create voice dir: {}", e))?;

    let filename = format!(
        "{}_Voice_34_{:04}_{}.{}",
        media::sanitize_filename(session_name),
        index,
        message.local_id,
        output_format.extension(payload.format)
    );
    let dest = output_dir.join(filename);

    match output_format {
        VoiceOutputFormat::Native => {
            std::fs::write(&dest, payload.bytes)
                .map_err(|e| format!("Cannot write voice file: {}", e))?;
        }
        VoiceOutputFormat::Wav | VoiceOutputFormat::Pcm => {
            export_decoded_voice(payload, &dest, output_format, decoder, sample_rate)?;
        }
    }

    Ok(dest)
}

fn export_decoded_voice(
    payload: VoicePayload<'_>,
    dest: &Path,
    output_format: VoiceOutputFormat,
    decoder: Option<&Path>,
    sample_rate: u32,
) -> Result<(), String> {
    if payload.format != VoiceFormat::NativeEncoded {
        return Err(format!(
            "--format {} currently requires native voice data; source format is {}",
            output_format.extension(payload.format),
            payload.format.label()
        ));
    }

    let decoder = resolve_voice_decoder(decoder)?;
    let mut native_file = tempfile::Builder::new()
        .prefix("tg-voice-")
        .suffix(".voice")
        .tempfile()
        .map_err(|e| format!("Create temporary native voice file: {}", e))?;
    native_file
        .write_all(payload.bytes)
        .map_err(|e| format!("Write temporary native voice file: {}", e))?;
    native_file
        .flush()
        .map_err(|e| format!("Flush temporary native voice file: {}", e))?;

    match decoder.kind {
        VoiceDecoderKind::NativeRust => run_native_rust_decoder(
            &decoder.path,
            native_file.path(),
            dest,
            output_format,
            sample_rate,
        ),
        VoiceDecoderKind::NativeGo => {
            if output_format == VoiceOutputFormat::Pcm {
                run_native_go_decoder(&decoder.path, native_file.path(), dest, sample_rate)
            } else {
                let pcm = tempfile::Builder::new()
                    .prefix("tg-voice-")
                    .suffix(".pcm")
                    .tempfile()
                    .map_err(|e| format!("Create temporary pcm file: {}", e))?;
                run_native_go_decoder(&decoder.path, native_file.path(), pcm.path(), sample_rate)?;
                write_wav_from_pcm(pcm.path(), dest, sample_rate)
            }
        }
        VoiceDecoderKind::NativeV3 => {
            if output_format == VoiceOutputFormat::Pcm {
                run_native_v3_decoder(&decoder.path, native_file.path(), dest, sample_rate)
            } else {
                let pcm = tempfile::Builder::new()
                    .prefix("tg-voice-")
                    .suffix(".pcm")
                    .tempfile()
                    .map_err(|e| format!("Create temporary pcm file: {}", e))?;
                run_native_v3_decoder(&decoder.path, native_file.path(), pcm.path(), sample_rate)?;
                write_wav_from_pcm(pcm.path(), dest, sample_rate)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoiceDecoderKind {
    NativeRust,
    NativeGo,
    NativeV3,
}

struct VoiceDecoder {
    path: PathBuf,
    kind: VoiceDecoderKind,
}

fn resolve_voice_decoder(explicit: Option<&Path>) -> Result<VoiceDecoder, String> {
    if let Some(path) = explicit {
        return Ok(VoiceDecoder {
            path: path.to_path_buf(),
            kind: classify_voice_decoder(path),
        });
    }

    if let Ok(value) = env::var("TG_VOICE_DECODER").or_else(|_| env::var(native_decoder_env_name()))
    {
        let path = PathBuf::from(value);
        return Ok(VoiceDecoder {
            kind: classify_voice_decoder(&path),
            path,
        });
    }

    for name in native_decoder_command_names() {
        if let Some(path) = find_command_in_path(&name) {
            return Ok(VoiceDecoder {
                kind: classify_voice_decoder(&path),
                path,
            });
        }
    }

    Err(
        "No native voice decoder found. Pass --decoder /path/to/decoder \
         or set TG_VOICE_DECODER."
            .to_string(),
    )
}

fn classify_voice_decoder(path: &Path) -> VoiceDecoderKind {
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let token = native_codec_token();
    if name.contains(&format!("rust-{}", token)) {
        VoiceDecoderKind::NativeRust
    } else if name.contains(&format!("{}-decoder", token)) {
        VoiceDecoderKind::NativeGo
    } else {
        VoiceDecoderKind::NativeV3
    }
}

fn native_codec_token() -> String {
    ["si", "lk"].concat()
}

fn native_decoder_env_name() -> String {
    format!("TG_{}_DECODER", native_codec_token().to_ascii_uppercase())
}

fn native_decoder_command_names() -> Vec<String> {
    let token = native_codec_token();
    vec![
        format!("rust-{}", token),
        format!("{}-decoder", token),
        format!("{}_v3_decoder", token),
        "decoder".to_string(),
    ]
}

fn find_command_in_path(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    for dir in env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn run_native_rust_decoder(
    decoder: &Path,
    input: &Path,
    output: &Path,
    output_format: VoiceOutputFormat,
    sample_rate: u32,
) -> Result<(), String> {
    let mut command = Command::new(decoder);
    command
        .arg("decode")
        .arg("-i")
        .arg(input)
        .arg("-o")
        .arg(output);
    command
        .arg("--sample-rate")
        .arg(sample_rate.to_string())
        .arg("--tolerant")
        .arg("skip");
    if output_format == VoiceOutputFormat::Wav {
        command.arg("--wav");
    }
    run_decoder_command(command, decoder)
}

fn run_native_go_decoder(
    decoder: &Path,
    input: &Path,
    output: &Path,
    sample_rate: u32,
) -> Result<(), String> {
    let mut command = Command::new(decoder);
    command
        .arg("-i")
        .arg(input)
        .arg("-mp3=false")
        .arg("-sampleRate")
        .arg(sample_rate.to_string())
        .arg("-o")
        .arg(output);
    run_decoder_command(command, decoder)
}

fn run_native_v3_decoder(
    decoder: &Path,
    input: &Path,
    output: &Path,
    sample_rate: u32,
) -> Result<(), String> {
    let mut command = Command::new(decoder);
    command
        .arg(input)
        .arg(output)
        .arg("-Fs_API")
        .arg(sample_rate.to_string());
    run_decoder_command(command, decoder)
}

fn run_decoder_command(mut command: Command, decoder: &Path) -> Result<(), String> {
    let output = command
        .output()
        .map_err(|e| format!("Run voice decoder {}: {}", decoder.display(), e))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "Voice decoder {} failed with status {}{}",
        decoder.display(),
        output.status,
        if stderr.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", truncate_for_log(stderr.trim(), 500))
        }
    ))
}

fn write_wav_from_pcm(pcm_path: &Path, wav_path: &Path, sample_rate: u32) -> Result<(), String> {
    let pcm = std::fs::read(pcm_path).map_err(|e| format!("Read decoded PCM: {}", e))?;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    append_wav_header(&mut wav, pcm.len() as u32, sample_rate, 1, 16);
    wav.extend_from_slice(&pcm);
    std::fs::write(wav_path, wav).map_err(|e| format!("Write WAV file: {}", e))
}

fn append_wav_header(
    out: &mut Vec<u8>,
    data_len: u32,
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
) {
    let byte_rate = sample_rate * channels as u32 * bits_per_sample as u32 / 8;
    let block_align = channels * bits_per_sample / 8;
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
}

fn truncate_for_log(value: &str, max_chars: usize) -> String {
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

fn load_voice_message_by_id(
    decrypted_dir: &Path,
    session_query: &str,
    id: i64,
    jobs: usize,
) -> Result<(String, VoiceMessage), String> {
    let (username, conn, chat_name_id) =
        open_voice_db_for_session(decrypted_dir, session_query, jobs)?;
    let Some(chat_name_id) = chat_name_id else {
        return Err(format!("Voice id {} was not found for '{}'", id, username));
    };
    let mut stmt = conn
        .prepare(
            "SELECT create_time, local_id, svr_id, voice_data \
             FROM VoiceInfo \
             WHERE chat_name_id = ?1 AND local_id = ?2 \
             LIMIT 1",
        )
        .map_err(|e| format!("Prepare voice id query: {}", e))?;
    let message = stmt
        .query_row(params![chat_name_id, id], read_voice_row)
        .optional()
        .map_err(|e| format!("Read voice id {}: {}", id, e))?
        .ok_or_else(|| format!("Voice id {} was not found for '{}'", id, username))?;
    Ok((username, message))
}

fn load_voice_messages(
    decrypted_dir: &Path,
    session_query: &str,
    since: Option<i64>,
    limit: usize,
    jobs: usize,
) -> Result<(String, Vec<VoiceMessage>), String> {
    let (username, conn, chat_name_id) =
        open_voice_db_for_session(decrypted_dir, session_query, jobs)?;
    let Some(chat_name_id) = chat_name_id else {
        return Ok((username, Vec::new()));
    };

    let since_clause = since
        .map(|ts| format!(" AND create_time >= {}", ts))
        .unwrap_or_default();
    let sql = format!(
        "SELECT create_time, local_id, svr_id, voice_data \
         FROM VoiceInfo \
         WHERE chat_name_id = {} AND create_time > 0{} \
         ORDER BY create_time DESC LIMIT {}",
        chat_name_id, since_clause, limit
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("Prepare voice query: {}", e))?;
    let rows = stmt
        .query_map([], read_voice_row)
        .map_err(|e| format!("Read voice rows: {}", e))?;

    let mut messages = rows.filter_map(|row| row.ok()).collect::<Vec<_>>();
    messages.sort_by_key(|message| {
        (
            Reverse(message.timestamp),
            Reverse(message.local_id),
            Reverse(message.svr_id),
        )
    });
    messages.truncate(limit);
    Ok((username, messages))
}

fn open_voice_db_for_session(
    decrypted_dir: &Path,
    session_query: &str,
    jobs: usize,
) -> Result<(String, Connection, Option<i64>), String> {
    let (username, _) = resolve_image_session(decrypted_dir, session_query, jobs)?;
    let media_db = decrypted_dir.join("message/media_0.db");
    if !media_db.is_file() {
        return Err(format!("Media database not found: {}", media_db.display()));
    }

    let conn = open_immutable_connection(&media_db)?;
    let chat_name_id: Option<i64> = conn
        .query_row(
            "SELECT rowid FROM Name2Id WHERE user_name = ?1",
            params![username],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| format!("Resolve voice chat id: {}", e))?;

    Ok((username, conn, chat_name_id))
}

fn read_voice_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<VoiceMessage> {
    let timestamp = row.get::<_, Option<i64>>(0)?.unwrap_or(0);
    let voice_data = row.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default();
    Ok(VoiceMessage {
        time: time::format_local_timestamp(timestamp),
        timestamp,
        local_id: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
        svr_id: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
        voice_data,
    })
}

fn voice_payload(data: &[u8]) -> Option<VoicePayload<'_>> {
    const AMR_HEADER: &[u8] = b"#!AMR\n";

    if data.is_empty() {
        return None;
    }
    let native_header = native_voice_header();
    if data.starts_with(&native_header) {
        return Some(VoicePayload {
            bytes: data,
            format: VoiceFormat::NativeEncoded,
        });
    }
    if data.len() > 1 && data[1..].starts_with(&native_header) {
        return Some(VoicePayload {
            bytes: &data[1..],
            format: VoiceFormat::NativeEncoded,
        });
    }
    if data.starts_with(AMR_HEADER) {
        return Some(VoicePayload {
            bytes: data,
            format: VoiceFormat::Amr,
        });
    }

    Some(VoicePayload {
        bytes: data,
        format: VoiceFormat::Raw,
    })
}

fn native_voice_header() -> Vec<u8> {
    [b"#!SI".as_slice(), b"LK_V3".as_slice()].concat()
}

fn open_immutable_connection(path: &Path) -> Result<Connection, String> {
    let uri = format!("file:{}?immutable=1", sqlite_uri_path(path));
    Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| format!("Open media database {}: {}", path.display(), e))
}

fn sqlite_uri_path(path: &Path) -> String {
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

/// Extract attribute value from XML-like string within content.
fn extract_xml_attr_str(content: &str, attr: &str) -> Option<String> {
    let pattern = format!("{}=\"", attr);
    let mut search_from = 0;

    while let Some(relative_start) = content[search_from..].find(&pattern) {
        let start = search_from + relative_start;
        let has_attr_boundary = content[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c == '<' || c.is_whitespace());
        if !has_attr_boundary {
            search_from = start + 1;
            continue;
        }

        let value_start = start + pattern.len();
        if value_start >= content.len() {
            return None;
        }
        let rest = &content[value_start..];
        let end = rest.find('"')?;
        let value = rest[..end].to_string();
        return if value.is_empty() { None } else { Some(value) };
    }

    None
}

fn export_txt(
    path: &Path,
    username: &str,
    display_name: &str,
    messages: &[ExportMessage],
) -> Result<(), String> {
    let mut f = std::fs::File::create(path)
        .map_err(|e| format!("Cannot create {}: {}", path.display(), e))?;

    writeln!(f, "Telegram 聊天记录: {} ({})", display_name, username).ok();
    writeln!(f, "总消息数: {}", messages.len()).ok();
    writeln!(
        f,
        "时间范围: {} ~ {}",
        messages.first().map(|m| &m.time).unwrap_or(&"".to_string()),
        messages.last().map(|m| &m.time).unwrap_or(&"".to_string())
    )
    .ok();
    writeln!(f, "{}", "=".repeat(60)).ok();
    writeln!(f).ok();

    for m in messages {
        writeln!(f, "[{}] {}: {}", m.time, m.sender, m.content).ok();
    }

    Ok(())
}

fn export_csv(path: &Path, messages: &[ExportMessage]) -> Result<(), String> {
    let mut f = std::fs::File::create(path)
        .map_err(|e| format!("Cannot create {}: {}", path.display(), e))?;

    // Write BOM for Excel compatibility
    f.write_all(&[0xEF, 0xBB, 0xBF]).ok();
    writeln!(f, "时间,发送者,类型,内容").ok();

    for m in messages {
        // Escape CSV fields
        let time = escape_csv(&m.time);
        let sender = escape_csv(&m.sender);
        let type_name = escape_csv(&m.type_name);
        let content = escape_csv(&m.content);
        writeln!(f, "{},{},{},{}", time, sender, type_name, content).ok();
    }

    Ok(())
}

fn export_json(path: &Path, messages: &[ExportMessage]) -> Result<(), String> {
    let json = serde_json::to_string_pretty(messages)
        .map_err(|e| format!("JSON serialization error: {}", e))?;
    std::fs::write(path, json).map_err(|e| format!("Cannot write {}: {}", path.display(), e))?;
    Ok(())
}

/// Export a media file, decrypting .dat files on the fly.
fn export_media_with_decrypt(
    src: &Path,
    output_dir: &Path,
    session_name: &str,
    cat_name: &str,
    msg_type: i64,
    index: usize,
    media_keys: Option<&crate::media_key::MediaKeys>,
) -> Result<PathBuf, String> {
    let use_decrypt = src.extension().and_then(|e| e.to_str()) == Some("dat");

    if use_decrypt {
        if let Some(keys) = media_keys {
            // Decrypt directly to the output location with .dat extension
            let filename = format!(
                "{}_{}_{}_{:04}.dat",
                sanitize_filename(session_name),
                cat_name,
                msg_type,
                index
            );
            let dest = output_dir.join(&filename);
            let ext = crate::media_decrypt::decrypt_v2_dat(src, &dest, keys)?;
            // Rename to the correct extension
            let final_dest = dest.with_extension(ext);
            std::fs::rename(&dest, &final_dest)
                .map_err(|e| format!("Rename to .{}: {}", ext, e))?;
            return Ok(final_dest);
        }
        // No keys available: fall through to plain copy
    }

    // Plain copy for non-.dat files or when keys are unavailable
    media::export_media_file(
        src,
        output_dir,
        session_name,
        &format!("{}_{}", cat_name, msg_type),
        index,
    )
}

fn export_sticker_message(
    message: &ExportMessage,
    telegram_base: &Path,
    output_dir: &Path,
    session_name: &str,
    index: usize,
) -> Result<PathBuf, String> {
    let info = media::parse_sticker_info(&message.raw_content);
    if info.md5.is_empty()
        && info.url.is_empty()
        && info.cdn_url.is_empty()
        && info.encrypt_url.is_empty()
        && info.extern_url.is_empty()
        && info.thumb_url.is_empty()
    {
        return Err("no sticker md5 or URL in message XML".to_string());
    }

    if !info.md5.is_empty() {
        if let Some(src) = media::find_cached_sticker(telegram_base, &info.md5) {
            let data = std::fs::read(&src)
                .map_err(|e| format!("Read cached sticker {}: {}", src.display(), e))?;
            if let Some(path) =
                write_sticker_candidate(&data, &info, output_dir, session_name, index)?
            {
                return Ok(path);
            }
        }
    }

    let mut tried_urls = HashSet::new();
    for url in [&info.cdn_url, &info.extern_url, &info.url, &info.thumb_url] {
        if url.is_empty() || !tried_urls.insert(url.clone()) {
            continue;
        }
        match download_url(url) {
            Ok(data) => {
                if let Some(path) =
                    write_sticker_candidate(&data, &info, output_dir, session_name, index)?
                {
                    return Ok(path);
                }
            }
            Err(e) => log::warn!("  Sticker download skipped: {}", e),
        }
    }

    if !info.encrypt_url.is_empty() && tried_urls.insert(info.encrypt_url.clone()) {
        let data = download_url(&info.encrypt_url)?;
        if let Some(path) = write_sticker_candidate(&data, &info, output_dir, session_name, index)?
        {
            return Ok(path);
        }
    }

    let id = if info.md5.is_empty() {
        "unknown"
    } else {
        &info.md5
    };
    Err(format!("cannot decode sticker {}", id))
}

fn has_sticker_export_source(message: &ExportMessage) -> bool {
    let info = media::parse_sticker_info(&message.raw_content);
    !info.md5.is_empty()
        || !info.url.is_empty()
        || !info.cdn_url.is_empty()
        || !info.encrypt_url.is_empty()
        || !info.extern_url.is_empty()
        || !info.thumb_url.is_empty()
}

fn write_sticker_candidate(
    data: &[u8],
    info: &media::StickerInfo,
    output_dir: &Path,
    session_name: &str,
    index: usize,
) -> Result<Option<PathBuf>, String> {
    let ext = crate::media_decrypt::detect_ext(data);
    if ext == "tggf" {
        let jpg = crate::media_decrypt::convert_tggf_to_jpg(data)?;
        return write_sticker_bytes(&jpg, "jpg", output_dir, session_name, index).map(Some);
    }
    if ext != "bin" {
        return write_sticker_bytes(data, ext, output_dir, session_name, index).map(Some);
    }

    if !info.aes_key.is_empty() {
        if let Some(decoded) = media::decrypt_sticker_aes_cbc(data, &info.aes_key) {
            let ext = crate::media_decrypt::detect_ext(&decoded);
            if ext == "tggf" {
                let jpg = crate::media_decrypt::convert_tggf_to_jpg(&decoded)?;
                return write_sticker_bytes(&jpg, "jpg", output_dir, session_name, index).map(Some);
            }
            if ext != "bin" {
                return write_sticker_bytes(&decoded, ext, output_dir, session_name, index)
                    .map(Some);
            }
        }
    }

    Ok(None)
}

fn write_sticker_bytes(
    data: &[u8],
    ext: &str,
    output_dir: &Path,
    session_name: &str,
    index: usize,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(output_dir).map_err(|e| format!("Cannot create media dir: {}", e))?;

    let filename = format!(
        "{}_Sticker_47_{:04}.{}",
        sanitize_filename(session_name),
        index,
        ext
    );
    let dest = output_dir.join(filename);
    std::fs::write(&dest, data).map_err(|e| format!("Write sticker {}: {}", dest.display(), e))?;
    Ok(dest)
}

fn download_url(url: &str) -> Result<Vec<u8>, String> {
    let url = url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("unsupported URL: {}", url));
    }

    let output = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--max-time",
            "20",
            "--max-filesize",
            "52428800",
            "--retry",
            "2",
            "--compressed",
            "--silent",
            "--show-error",
            url,
        ])
        .output()
        .map_err(|e| format!("run curl: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("curl failed for {}: {}", url, stderr.trim()));
    }
    if output.stdout.is_empty() {
        return Err(format!("empty response from {}", url));
    }

    Ok(output.stdout)
}

fn sanitize_filename(s: &str) -> String {
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

fn escape_csv(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::{tempdir, TempDir};

    fn packed_image(filename: &str) -> Vec<u8> {
        let mut packed = Vec::new();
        packed.push(8);
        packed.push(1);

        let mut img = vec![8, 1, 16, 1, 34, filename.len() as u8];
        img.extend_from_slice(filename.as_bytes());

        packed.push(26);
        packed.push(img.len() as u8);
        packed.extend_from_slice(&img);
        packed
    }

    fn packed_file(filename: &str) -> Vec<u8> {
        let mut inner = vec![8, 0, 18, filename.len() as u8];
        inner.extend_from_slice(filename.as_bytes());

        let mut file = vec![10, inner.len() as u8];
        file.extend_from_slice(&inner);

        let mut packed = vec![8, 3, 16, 6, 58, file.len() as u8];
        packed.extend_from_slice(&file);
        packed
    }

    fn create_export_decrypted_dir() -> TempDir {
        let dir = tempdir().unwrap();
        let contact_dir = dir.path().join("contact");
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&contact_dir).unwrap();
        std::fs::create_dir_all(&message_dir).unwrap();

        let contact_conn = Connection::open(contact_dir.join("contact.db")).unwrap();
        contact_conn
            .execute(
                "CREATE TABLE contact (
                    username TEXT,
                    nick_name TEXT,
                    remark TEXT,
                    alias TEXT
                )",
                [],
            )
            .unwrap();
        contact_conn
            .execute(
                "INSERT INTO contact (username, nick_name, remark, alias)
                 VALUES ('tgid_export', 'Export Nick', 'Export Remark', '')",
                [],
            )
            .unwrap();
        drop(contact_conn);

        let message_conn = Connection::open(message_dir.join("message_0.db")).unwrap();
        let table_name = db::msg_table_name("tgid_export");
        let body_col = db::quote_identifier(dictionary::msg_body_column());
        let marker_col = db::quote_identifier(dictionary::msg_compression_marker_column());
        let packed_col = db::quote_identifier(dictionary::msg_packed_meta_column());
        message_conn
            .execute(
                &format!(
                    "CREATE TABLE {} (
                        local_type INTEGER,
                        create_time INTEGER,
                        {} TEXT,
                        {} INTEGER,
                        {} BLOB
                    )",
                    table_name, body_col, marker_col, packed_col
                ),
                [],
            )
            .unwrap();
        for (timestamp, content) in [
            (1001, "second, \"quoted\" message"),
            (1000, "first message"),
        ] {
            message_conn
                .execute(
                    &format!(
                        "INSERT INTO {} (local_type, create_time, {}, {}, {})
                         VALUES (1, ?1, ?2, NULL, x'')",
                        table_name, body_col, marker_col, packed_col
                    ),
                    params![timestamp, content],
                )
                .unwrap();
        }
        drop(message_conn);

        dir
    }

    fn create_image_decrypted_dir() -> TempDir {
        let dir = tempdir().unwrap();
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();

        let message_conn = Connection::open(message_dir.join("message_0.db")).unwrap();
        let table_name = db::msg_table_name("tgid_images");
        let body_col = db::quote_identifier(dictionary::msg_body_column());
        let marker_col = db::quote_identifier(dictionary::msg_compression_marker_column());
        let packed_col = db::quote_identifier(dictionary::msg_packed_meta_column());
        message_conn
            .execute(
                &format!(
                    "CREATE TABLE {} (
                        local_type INTEGER,
                        create_time INTEGER,
                        {} TEXT,
                        {} INTEGER,
                        {} BLOB
                    )",
                    table_name, body_col, marker_col, packed_col
                ),
                [],
            )
            .unwrap();

        for (msg_type, timestamp, content, packed) in [
            (
                3,
                1000,
                r#"<msg><img aeskey="old-xml-key" /></msg>"#,
                packed_image("old.dat"),
            ),
            (1, 1001, "not an image", Vec::new()),
            (
                3,
                1002,
                r#"<msg><img aeskey="new-xml-key" /></msg>"#,
                packed_image("new.dat"),
            ),
        ] {
            message_conn
                .execute(
                    &format!(
                        "INSERT INTO {} (local_type, create_time, {}, {}, {})
                         VALUES (?1, ?2, ?3, NULL, ?4)",
                        table_name, body_col, marker_col, packed_col
                    ),
                    params![msg_type, timestamp, content, packed],
                )
                .unwrap();
        }
        drop(message_conn);

        dir
    }

    fn create_file_decrypted_dir() -> TempDir {
        let dir = tempdir().unwrap();
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();

        let message_conn = Connection::open(message_dir.join("message_0.db")).unwrap();
        let table_name = db::msg_table_name("tgid_files");
        let body_col = db::quote_identifier(dictionary::msg_body_column());
        let marker_col = db::quote_identifier(dictionary::msg_compression_marker_column());
        let packed_col = db::quote_identifier(dictionary::msg_packed_meta_column());
        message_conn
            .execute(
                &format!(
                    "CREATE TABLE {} (
                        local_type INTEGER,
                        create_time INTEGER,
                        {} TEXT,
                        {} INTEGER,
                        {} BLOB
                    )",
                    table_name, body_col, marker_col, packed_col
                ),
                [],
            )
            .unwrap();

        let app_file_type = app_file_local_type();
        let app_file_alt_type = app_local_type(62);
        for (msg_type, timestamp, content, packed) in [
            (
                49,
                1000,
                r#"<msg><appmsg><title>old.pdf</title><type>6</type><totallen>2048</totallen></appmsg></msg>"#,
                Vec::new(),
            ),
            (
                49,
                1001,
                r#"<msg><appmsg><title>link</title><type>5</type></appmsg></msg>"#,
                Vec::new(),
            ),
            (
                app_file_type,
                1002,
                r#"<msg><appmsg><title>xml.pdf</title><type>6</type><totallen>1024</totallen></appmsg></msg>"#,
                packed_file("new.pdf"),
            ),
            (
                app_file_alt_type,
                1003,
                r#"<msg><appmsg><title>alt.pdf</title><type>62</type><totallen>4096</totallen></appmsg></msg>"#,
                packed_file("alt.pdf"),
            ),
        ] {
            message_conn
                .execute(
                    &format!(
                        "INSERT INTO {} (local_type, create_time, {}, {}, {})
                         VALUES (?1, ?2, ?3, NULL, ?4)",
                        table_name, body_col, marker_col, packed_col
                    ),
                    params![msg_type, timestamp, content, packed],
                )
                .unwrap();
        }
        drop(message_conn);

        dir
    }

    fn create_voice_decrypted_dir() -> TempDir {
        let dir = tempdir().unwrap();
        let message_dir = dir.path().join("message");
        std::fs::create_dir_all(&message_dir).unwrap();

        let conn = Connection::open(message_dir.join("media_0.db")).unwrap();
        conn.execute("CREATE TABLE Name2Id (user_name TEXT PRIMARY KEY)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO Name2Id (rowid, user_name) VALUES (1, 'tgid_voices')",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE VoiceInfo (
                chat_name_id INTEGER,
                create_time INTEGER,
                local_id INTEGER,
                svr_id INTEGER,
                voice_data BLOB,
                data_index TEXT DEFAULT '0'
            )",
            [],
        )
        .unwrap();

        let mut old_voice = vec![2];
        old_voice.extend_from_slice(&native_voice_fixture(" old"));
        let mut new_voice = vec![2];
        new_voice.extend_from_slice(&native_voice_fixture(" new"));
        for (timestamp, local_id, data) in [(1000, 1, old_voice), (1002, 2, new_voice)] {
            conn.execute(
                "INSERT INTO VoiceInfo
                 (chat_name_id, create_time, local_id, svr_id, voice_data)
                 VALUES (1, ?1, ?2, ?3, ?4)",
                params![timestamp, local_id, 9000 + local_id, data],
            )
            .unwrap();
        }
        drop(conn);

        dir
    }

    fn native_voice_fixture(suffix: &str) -> Vec<u8> {
        let mut data = native_voice_header();
        data.extend_from_slice(suffix.as_bytes());
        data
    }

    fn image_message(raw_content: &str, packed_info: Vec<u8>) -> ImageMessage {
        ImageMessage {
            time: "2026-04-28 09:38:44".to_string(),
            timestamp: 1,
            raw_content: raw_content.to_string(),
            packed_info,
        }
    }

    fn export_message(msg_type: i64, raw_content: &str, packed_info: Vec<u8>) -> ExportMessage {
        ExportMessage {
            time: "2026-04-28 09:38:44".to_string(),
            timestamp: 1,
            sender: "Alice".to_string(),
            msg_type,
            type_name: "文件".to_string(),
            content: String::new(),
            raw_content: raw_content.to_string(),
            packed_info,
        }
    }

    #[test]
    fn export_messages_writes_json_in_chronological_order() {
        let decrypted = create_export_decrypted_dir();
        let output = tempdir().unwrap();

        let results = export_messages(MessageExportConfig {
            decrypted_dir: decrypted.path(),
            session_query: "tgid_export",
            format: "json",
            output_dir: output.path(),
            media_dir: None,
            since: None,
            limit: None,
            name_mode: contact::DisplayNameMode::PersonalRemark,
            jobs: 1,
        })
        .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "json");
        let data: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(output.path().join("chat.json")).unwrap(),
        )
        .unwrap();
        let messages = data.as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["timestamp"], 1000);
        assert_eq!(messages[0]["sender"], "Export Remark");
        assert_eq!(messages[0]["content"], "first message");
        assert_eq!(messages[1]["timestamp"], 1001);
        assert!(messages[0].get("raw_content").is_none());
        assert!(messages[0].get("packed_info").is_none());
    }

    #[test]
    fn export_messages_anonymous_uses_public_name() {
        let decrypted = create_export_decrypted_dir();
        let output = tempdir().unwrap();

        export_messages(MessageExportConfig {
            decrypted_dir: decrypted.path(),
            session_query: "tgid_export",
            format: "json",
            output_dir: output.path(),
            media_dir: None,
            since: None,
            limit: None,
            name_mode: contact::DisplayNameMode::Anonymous,
            jobs: 1,
        })
        .unwrap();

        let data: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(output.path().join("chat.json")).unwrap(),
        )
        .unwrap();
        let messages = data.as_array().unwrap();
        assert_eq!(messages[0]["sender"], "Export Nick");
        assert_ne!(messages[0]["sender"], "Export Remark");
    }

    #[test]
    fn export_messages_writes_all_formats_by_default() {
        let decrypted = create_export_decrypted_dir();
        let output = tempdir().unwrap();

        let results = export_messages(MessageExportConfig {
            decrypted_dir: decrypted.path(),
            session_query: "tgid_export",
            format: "all",
            output_dir: output.path(),
            media_dir: None,
            since: None,
            limit: None,
            name_mode: contact::DisplayNameMode::PersonalRemark,
            jobs: 1,
        })
        .unwrap();

        assert_eq!(
            results.iter().map(|(fmt, _)| *fmt).collect::<Vec<_>>(),
            vec!["txt", "csv", "json"]
        );
        let txt = std::fs::read_to_string(output.path().join("chat.txt")).unwrap();
        assert!(txt.contains("Export Remark (tgid_export)"));
        assert!(txt.contains("first message"));

        let csv = std::fs::read_to_string(output.path().join("chat.csv")).unwrap();
        assert!(csv.contains("时间,发送者,类型,内容"));
        assert!(csv.contains("\"second, \"\"quoted\"\" message\""));
        assert!(output.path().join("chat.json").exists());
    }

    #[test]
    fn export_messages_respects_since_and_limit() {
        let decrypted = create_export_decrypted_dir();
        let output = tempdir().unwrap();

        export_messages(MessageExportConfig {
            decrypted_dir: decrypted.path(),
            session_query: "tgid_export",
            format: "json",
            output_dir: output.path(),
            media_dir: None,
            since: Some(1000),
            limit: Some(1),
            name_mode: contact::DisplayNameMode::PersonalRemark,
            jobs: 1,
        })
        .unwrap();

        let data: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(output.path().join("chat.json")).unwrap(),
        )
        .unwrap();
        let messages = data.as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["timestamp"], 1001);
    }

    #[test]
    fn load_image_messages_returns_images_newest_first() {
        let decrypted = create_image_decrypted_dir();

        let (username, messages) =
            load_image_messages(decrypted.path(), "tgid_images", None, 10, 1).unwrap();

        assert_eq!(username, "tgid_images");
        assert_eq!(
            messages
                .iter()
                .map(|message| message.timestamp)
                .collect::<Vec<_>>(),
            vec![1002, 1000]
        );
        assert_eq!(image_identifier(&messages[0]).as_deref(), Some("new.dat"));
    }

    #[test]
    fn load_image_messages_respects_since_and_limit() {
        let decrypted = create_image_decrypted_dir();

        let (_, messages) =
            load_image_messages(decrypted.path(), "tgid_images", Some(1001), 1, 1).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].timestamp, 1002);
        assert_eq!(image_identifier(&messages[0]).as_deref(), Some("new.dat"));
    }

    #[test]
    fn load_file_messages_returns_files_newest_first() {
        let decrypted = create_file_decrypted_dir();

        let (username, messages) =
            load_file_messages(decrypted.path(), "tgid_files", None, 10, 1).unwrap();

        assert_eq!(username, "tgid_files");
        assert_eq!(
            messages
                .iter()
                .map(|message| message.timestamp)
                .collect::<Vec<_>>(),
            vec![1003, 1002, 1000]
        );
        assert_eq!(
            file_message_identifier(&messages[0]).as_deref(),
            Some("alt.pdf")
        );
        assert_eq!(file_size_label(&messages[0]), "4KB");
        assert_eq!(file_export_type(messages[0].msg_type), 62);
    }

    #[test]
    fn load_file_messages_respects_since_and_limit() {
        let decrypted = create_file_decrypted_dir();

        let (_, messages) =
            load_file_messages(decrypted.path(), "tgid_files", Some(1001), 1, 1).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].timestamp, 1003);
        assert_eq!(
            file_message_identifier(&messages[0]).as_deref(),
            Some("alt.pdf")
        );
    }

    #[test]
    fn load_voice_messages_returns_voices_newest_first() {
        let decrypted = create_voice_decrypted_dir();

        let (username, messages) =
            load_voice_messages(decrypted.path(), "tgid_voices", None, 10, 1).unwrap();

        assert_eq!(username, "tgid_voices");
        assert_eq!(
            messages
                .iter()
                .map(|message| message.timestamp)
                .collect::<Vec<_>>(),
            vec![1002, 1000]
        );
    }

    #[test]
    fn voice_payload_removes_native_padding_byte() {
        let mut data = vec![2];
        let payload_fixture = native_voice_fixture(" payload");
        data.extend_from_slice(&payload_fixture);

        let payload = voice_payload(&data).unwrap();

        assert_eq!(payload.format, VoiceFormat::NativeEncoded);
        assert_eq!(payload.bytes, payload_fixture);
    }

    #[test]
    fn export_voices_writes_normalized_native_voice() {
        let decrypted = create_voice_decrypted_dir();
        let output = tempdir().unwrap();

        let results = export_voices(
            decrypted.path(),
            "tgid_voices",
            VoiceExportConfig {
                output_dir: output.path(),
                format: VoiceOutputFormat::Native,
                decoder: None,
                list: false,
                all: false,
                index: None,
                id: None,
                limit: 20,
                since: None,
                jobs: 1,
                sample_rate: 24000,
            },
        )
        .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].extension().and_then(|ext| ext.to_str()),
            Some("voice")
        );
        let bytes = std::fs::read(&results[0]).unwrap();
        assert!(bytes.starts_with(&native_voice_header()));
        assert_ne!(bytes.first(), Some(&2));
    }

    #[test]
    fn load_voice_message_by_id_uses_listed_local_id() {
        let decrypted = create_voice_decrypted_dir();

        let (_, message) = load_voice_message_by_id(decrypted.path(), "tgid_voices", 2, 1).unwrap();

        assert_eq!(message.timestamp, 1002);
        assert_eq!(message.local_id, 2);
    }

    #[test]
    fn wav_header_wraps_pcm_payload() {
        let pcm = tempdir().unwrap();
        let pcm_path = pcm.path().join("voice.pcm");
        let wav_path = pcm.path().join("voice.wav");
        std::fs::write(&pcm_path, [1u8, 0, 2, 0]).unwrap();

        write_wav_from_pcm(&pcm_path, &wav_path, 24000).unwrap();

        let wav = std::fs::read(wav_path).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(&wav[40..44], &4u32.to_le_bytes());
        assert_eq!(&wav[44..], &[1, 0, 2, 0]);
    }

    #[test]
    fn image_identifier_prefers_protobuf_filename() {
        let message = image_message(
            r#"<msg><img aeskey="xml-key" cdnthumburl="thumb-id" /></msg>"#,
            packed_image("proto-file.dat"),
        );

        assert_eq!(
            image_identifier(&message).as_deref(),
            Some("proto-file.dat")
        );
    }

    #[test]
    fn image_identifier_uses_xml_fallbacks() {
        let message = image_message(r#"<msg><img aeskey="xml-key" /></msg>"#, Vec::new());
        assert_eq!(image_identifier(&message).as_deref(), Some("xml-key"));

        let message = image_message(r#"<msg><img cdnthumburl="thumb-id" /></msg>"#, Vec::new());
        assert_eq!(image_identifier(&message).as_deref(), Some("thumb-id"));
    }

    #[test]
    fn file_identifier_prefers_protobuf_filename() {
        let message = export_message(
            25_769_803_825,
            r#"<msg><appmsg><title>xml.pdf</title><type>6</type></appmsg></msg>"#,
            packed_file("proto.pdf"),
        );

        assert_eq!(file_identifier(&message).as_deref(), Some("proto.pdf"));
        assert!(is_file_message(&message));
        assert_eq!(file_export_type(message.msg_type), 49);
    }

    #[test]
    fn file_identifier_accepts_app_subtype_62() {
        let message = export_message(
            app_local_type(62),
            r#"<msg><appmsg><title>alt.pdf</title><type>62</type></appmsg></msg>"#,
            packed_file("alt.pdf"),
        );

        assert_eq!(file_identifier(&message).as_deref(), Some("alt.pdf"));
        assert!(is_file_message(&message));
        assert_eq!(file_export_type(message.msg_type), 62);
    }

    #[test]
    fn file_identifier_uses_xml_title_fallback() {
        let message = export_message(
            49,
            r#"<msg><appmsg><title>xml.pdf</title><type>6</type></appmsg></msg>"#,
            Vec::new(),
        );

        assert_eq!(file_identifier(&message).as_deref(), Some("xml.pdf"));
        assert!(is_file_message(&message));
    }

    #[test]
    fn file_extension_uses_identifier_when_cache_name_has_no_extension() {
        assert_eq!(
            file_extension(Path::new("/tmp/report"), Some("report.pdf")),
            "pdf"
        );
        assert_eq!(
            file_extension(Path::new("/tmp/report.bin"), Some("report.pdf")),
            "bin"
        );
    }

    #[test]
    fn normalize_file_id_rejects_empty_value() {
        assert_eq!(normalize_file_id("  abc.pdf  ").as_deref(), Ok("abc.pdf"));
        assert!(normalize_file_id("   ").unwrap_err().contains("--id"));
    }

    #[test]
    fn normalize_image_id_rejects_empty_value() {
        assert_eq!(normalize_image_id("  abc  ").as_deref(), Ok("abc"));
        assert!(normalize_image_id("   ").unwrap_err().contains("--id"));
    }

    #[test]
    fn image_id_conflicts_with_window_selection_modes() {
        let output_dir = Path::new("out");
        let err = export_images(
            Path::new("decrypted"),
            "session",
            ImageExportConfig {
                output_dir,
                list: true,
                all: false,
                index: None,
                id: Some("abc"),
                limit: 1,
                since: None,
                jobs: 1,
            },
        )
        .unwrap_err();

        assert!(err.contains("--id cannot be used"));
    }

    #[test]
    fn file_id_conflicts_with_window_selection_modes() {
        let output_dir = Path::new("out");
        let err = export_files(
            Path::new("decrypted"),
            "session",
            FileExportConfig {
                output_dir,
                list: true,
                all: false,
                index: None,
                id: Some("report.pdf"),
                limit: 1,
                since: None,
                jobs: 1,
            },
        )
        .unwrap_err();

        assert!(err.contains("--id cannot be used"));
    }
}
