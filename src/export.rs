use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::contact;
use crate::db;
use crate::dictionary;
use crate::media;
use crate::media_index::MediaIndex;
use crate::message;
use crate::parallel;
use crate::source_store::SourceStore;
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
        info: Box<media::StickerInfo>,
        cached_source: Option<PathBuf>,
    },
}

struct MediaExportResult {
    index: usize,
    is_sticker: bool,
    result: Result<PathBuf, String>,
}

pub struct MessageExportConfig<'a> {
    pub store: &'a SourceStore,
    pub session_query: &'a str,
    pub output_dir: &'a Path,
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

struct VoiceMessage {
    local_id: i64,
    voice_data: Vec<u8>,
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
}

struct VoicePayload<'a> {
    bytes: &'a [u8],
    format: VoiceFormat,
}

const DEFAULT_VOICE_SAMPLE_RATE: u32 = 24_000;

/// Export messages for a session.
pub fn export_messages(
    config: MessageExportConfig<'_>,
) -> Result<Vec<(&'static str, PathBuf)>, String> {
    let MessageExportConfig {
        store,
        session_query,
        output_dir,
        jobs,
    } = config;

    let message_dbs = db::message_databases(store);
    let contacts = if db::has_database(store, "contact/contact.db") {
        let conn = store.open("contact/contact.db")?;
        contact::load_contacts(&conn)?
    } else {
        HashMap::new()
    };
    let username =
        db::resolve_username_for_messages(session_query, &contacts, store, &message_dbs, jobs)?;
    let display_name = contacts
        .get(&username)
        .map(|c| c.display_name(contact::DisplayNameMode::PersonalRemark))
        .unwrap_or(&username);
    let room_member_names = HashMap::new();
    let display_context = MessageDisplayContext {
        chat_name: display_name,
        contacts: &contacts,
        name_mode: contact::DisplayNameMode::PersonalRemark,
        room_member_names: &room_member_names,
    };

    // Table name
    let table_name = db::msg_table_name(&username);
    let mut all_messages: Vec<ExportMessage> = Vec::new();

    let db_jobs = parallel::job_count(jobs, parallel::SOURCE_DB_AUTO_CAP);
    let per_db_messages = parallel::map_ordered(
        message_dbs.clone(),
        db_jobs,
        |db_path| -> Result<Vec<ExportMessage>, String> {
            let mut messages = Vec::new();
            let conn = store.open(&db_path)?;
            let table_exists = conn
                .query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
                    )",
                    params![&table_name],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(|error| format!("Check message table in {db_path}: {error}"))?;
            if !table_exists {
                return Ok(messages);
            }

            let body_col = dictionary::msg_body_column();
            let marker_col = dictionary::msg_compression_marker_column();
            let packed_col = dictionary::msg_packed_meta_column();
            let table = db::quote_identifier(&table_name);
            let sql = format!(
                "SELECT local_type, create_time, {body_col}, {marker_col}, {packed_col} \
             FROM {table} WHERE create_time > 0 ORDER BY create_time ASC"
            );

            let mut stmt = conn
                .prepare(&sql)
                .map_err(|error| format!("Prepare messages from {db_path}: {error}"))?;
            let rows = stmt
                .query_map([], |row| {
                    let compression_marker: Option<i64> = row.get::<_, Option<i64>>(3)?;
                    let content = db::read_message_body(row, 2, compression_marker)?;
                    let packed_info: Vec<u8> =
                        row.get::<_, Option<Vec<u8>>>(4)?.unwrap_or_default();
                    Ok((
                        row.get::<_, Option<i64>>(0)?.unwrap_or(-1),
                        row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        content,
                        compression_marker,
                        packed_info,
                    ))
                })
                .map_err(|error| format!("Read messages from {db_path}: {error}"))?;

            for row in rows {
                let (local_type, create_time, content, compression_marker, packed_info):
                ExportMessageRow = row
                .map_err(|error| format!("Decode message from {db_path}: {error}"))?;
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
            Ok(messages)
        },
    );

    for messages in per_db_messages {
        all_messages.extend(messages?);
    }

    all_messages.sort_by_key(|message| message.timestamp);

    if all_messages.is_empty() {
        return Err(format!("No messages found for '{}'", username));
    }

    // Create output directory
    std::fs::create_dir_all(output_dir).map_err(|e| format!("Cannot create output dir: {}", e))?;

    let mut results = Vec::new();

    let path_txt = output_dir.join("chat.txt");
    export_txt(&path_txt, &username, display_name, &all_messages)?;
    results.push(("txt", path_txt));

    let path_csv = output_dir.join("chat.csv");
    export_csv(&path_csv, &all_messages)?;
    results.push(("csv", path_csv));

    let path_json = output_dir.join("chat.json");
    export_json(&path_json, &all_messages)?;
    results.push(("json", path_json));

    let stdout = std::io::stdout();
    let mut out = crate::output::Output::new(stdout.lock());

    let mdir = output_dir.join("media");
    let mut exported = 0;

    // Export cached media files when local Telegram media cache is available.
    let mut next_index = 1usize;
    if let Some(telegram_base) = media::find_telegram_base_path() {
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
        let category_map = [("Image", 3), ("Video", 43)];

        for (cat_name, local_type) in &category_map {
            for m in &all_messages {
                if m.msg_type != *local_type {
                    continue;
                }
                // Try protobuf filename first, then fall back to XML aeskey
                let identifier = try_protobuf_identifier(&m.packed_info)
                    .or_else(|| media::extract_xml_attr(&m.raw_content, "aeskey"))
                    .or_else(|| media::extract_xml_attr(&m.raw_content, "cdnthumburl"));
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

        let mut seen_stickers = HashSet::new();
        for m in &all_messages {
            if m.msg_type != 47 {
                continue;
            }

            let info = media::parse_sticker_info(&m.raw_content);
            if info.md5.is_empty() || !record_new_sticker(&mut seen_stickers, &info) {
                continue;
            }

            let Some(cached_source) =
                media::find_cached_sticker_in_layout(&telegram_base, &info.md5)
            else {
                continue;
            };

            media_jobs.push(MediaExportJob::Sticker {
                index: next_index,
                info: Box::new(info),
                cached_source: Some(cached_source),
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

        let media_job_count = parallel::job_count(jobs, 8);
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
                    &mdir,
                    &username,
                    category,
                    msg_type,
                    index,
                    media_keys.as_ref(),
                ),
            },
            MediaExportJob::Sticker {
                index,
                info,
                cached_source,
            } => MediaExportResult {
                index,
                is_sticker: true,
                result: export_sticker_info(
                    &info,
                    cached_source.as_deref(),
                    &mdir,
                    &username,
                    index,
                ),
            },
        });

        let mut skipped_stickers = 0usize;
        let mut first_sticker_error = None;
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
                        skipped_stickers += 1;
                        first_sticker_error.get_or_insert(e);
                    } else {
                        log::warn!("  Media #{} export failed: {}", media_result.index, e);
                    }
                }
            }
        }
        if let Some(error) = first_sticker_error {
            log::warn!("Skipped {} cached stickers: {}", skipped_stickers, error);
        }
    } else {
        log::warn!("Telegram data directory not found, cached image/video/file export skipped");
    }

    for (index, path) in export_all_voice_messages(store, &username, &mdir, next_index)? {
        out.line(format_args!(
            "  Media #{}: {}",
            index,
            path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
        ))?;
        exported += 1;
    }

    if exported > 0 {
        out.line(format_args!("Exported {} media files", exported))?;
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
    media::message_media_type(msg_type, raw_content) == Some("file")
}

fn file_export_type(local_type: i64) -> i64 {
    if local_type == 62 || media::app_message_subtype(local_type) == Some(62) {
        62
    } else {
        49
    }
}

fn record_new_sticker(seen: &mut HashSet<String>, info: &media::StickerInfo) -> bool {
    info.identifier()
        .is_none_or(|identifier| seen.insert(identifier.to_string()))
}

fn export_all_voice_messages(
    store: &SourceStore,
    username: &str,
    output_dir: &Path,
    start_index: usize,
) -> Result<Vec<(usize, PathBuf)>, String> {
    if !db::has_database(store, "message/media_0.db") {
        log::warn!("Voice media database not found; cached voice export skipped");
        return Ok(Vec::new());
    }
    let decoder = match resolve_voice_decoder() {
        Ok(path) => Some(path),
        Err(error) => {
            log::warn!(
                "Voice WAV decoder unavailable; preserving native voice files: {}",
                error
            );
            None
        }
    };
    export_all_voice_messages_with_decoder(
        store,
        username,
        output_dir,
        start_index,
        decoder.as_deref(),
    )
}

fn export_all_voice_messages_with_decoder(
    store: &SourceStore,
    username: &str,
    output_dir: &Path,
    start_index: usize,
    decoder: Option<&Path>,
) -> Result<Vec<(usize, PathBuf)>, String> {
    let messages = load_voice_messages(store, username)?;
    let mut paths = Vec::new();
    for (offset, message) in messages.iter().enumerate() {
        let index = start_index + offset;
        let path = export_archive_voice(output_dir, username, index, message, decoder)
            .map_err(|error| format!("Voice #{index} export failed: {error}"))?;
        paths.push((index, path));
    }
    Ok(paths)
}

fn export_archive_voice(
    output_dir: &Path,
    session_name: &str,
    index: usize,
    message: &VoiceMessage,
    decoder: Option<&Path>,
) -> Result<PathBuf, String> {
    let payload =
        voice_payload(&message.voice_data).ok_or_else(|| "Voice data is empty".to_string())?;
    std::fs::create_dir_all(output_dir).map_err(|e| format!("Cannot create voice dir: {}", e))?;

    let native_path = voice_output_path(
        output_dir,
        session_name,
        index,
        message.local_id,
        payload.format.extension(),
    );
    if payload.format != VoiceFormat::NativeEncoded {
        write_voice_payload(&native_path, payload.bytes)?;
        return Ok(native_path);
    }

    let Some(decoder) = decoder else {
        write_voice_payload(&native_path, payload.bytes)?;
        return Ok(native_path);
    };

    let wav_path = voice_output_path(output_dir, session_name, index, message.local_id, "wav");
    match export_voice_wav(&payload, &wav_path, decoder) {
        Ok(()) => Ok(wav_path),
        Err(error) => {
            if wav_path.exists() {
                if let Err(cleanup_error) = std::fs::remove_file(&wav_path) {
                    log::warn!(
                        "Cannot remove failed voice WAV {}: {}",
                        wav_path.display(),
                        cleanup_error
                    );
                }
            }
            log::warn!(
                "Voice #{} WAV decode failed; preserving native voice: {}",
                index,
                error
            );
            write_voice_payload(&native_path, payload.bytes)?;
            Ok(native_path)
        }
    }
}

fn voice_output_path(
    output_dir: &Path,
    session_name: &str,
    index: usize,
    local_id: i64,
    extension: &str,
) -> PathBuf {
    output_dir.join(format!(
        "{}_Voice_34_{:04}_{}.{}",
        media::sanitize_filename(session_name),
        index,
        local_id,
        extension
    ))
}

fn write_voice_payload(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("Cannot write voice file: {}", e))
}

fn export_voice_wav(
    payload: &VoicePayload<'_>,
    destination: &Path,
    decoder: &Path,
) -> Result<(), String> {
    match std::fs::remove_file(destination) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Remove previous WAV: {error}")),
    }
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
    run_native_rust_decoder(decoder, native_file.path(), destination)?;
    let mut header = [0; 44];
    std::fs::File::open(destination)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(|error| format!("Voice decoder did not produce a complete WAV header: {error}"))?;
    if &header[..4] == b"RIFF" && &header[8..12] == b"WAVE" {
        Ok(())
    } else {
        Err("Voice decoder produced an invalid WAV header".to_string())
    }
}

fn resolve_voice_decoder() -> Result<PathBuf, String> {
    if let Ok(value) = env::var("TG_VOICE_DECODER") {
        return Ok(PathBuf::from(value));
    }

    let command = current_voice_decoder_command();
    find_command_in_path(&command).ok_or_else(|| {
        "No native voice decoder found. Install the Homebrew decoder or set TG_VOICE_DECODER."
            .to_string()
    })
}

fn native_codec_token() -> String {
    ["si", "lk"].concat()
}

fn current_voice_decoder_command() -> String {
    format!("rust-{}", native_codec_token())
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

fn run_native_rust_decoder(decoder: &Path, input: &Path, output: &Path) -> Result<(), String> {
    let mut command = Command::new(decoder);
    command
        .arg("decode")
        .arg("-i")
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--sample-rate")
        .arg(DEFAULT_VOICE_SAMPLE_RATE.to_string())
        .arg("--tolerant")
        .arg("skip")
        .arg("--wav");
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

fn load_voice_messages(store: &SourceStore, username: &str) -> Result<Vec<VoiceMessage>, String> {
    let (conn, chat_name_id) = open_voice_db_for_session(store, username)?;
    let Some(chat_name_id) = chat_name_id else {
        return Ok(Vec::new());
    };

    let mut stmt = conn
        .prepare(
            "SELECT local_id, voice_data \
             FROM VoiceInfo \
             WHERE chat_name_id = ?1 AND create_time > 0 \
             ORDER BY create_time ASC, local_id ASC, svr_id ASC",
        )
        .map_err(|e| format!("Prepare voice query: {}", e))?;
    let rows = stmt
        .query_map(params![chat_name_id], read_voice_row)
        .map_err(|e| format!("Read voice rows: {}", e))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| format!("Decode voice row: {error}"))
}

fn open_voice_db_for_session(
    store: &SourceStore,
    username: &str,
) -> Result<(Connection, Option<i64>), String> {
    if !db::has_database(store, "message/media_0.db") {
        return Err("Media database not found".to_string());
    }

    let conn = store.open("message/media_0.db")?;
    let chat_name_id: Option<i64> = conn
        .query_row(
            "SELECT rowid FROM Name2Id WHERE user_name = ?1",
            params![username],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| format!("Resolve voice chat id: {}", e))?;

    Ok((conn, chat_name_id))
}

fn read_voice_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<VoiceMessage> {
    Ok(VoiceMessage {
        local_id: row.get::<_, Option<i64>>(0)?.unwrap_or(0),
        voice_data: row.get::<_, Option<Vec<u8>>>(1)?.unwrap_or_default(),
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

fn export_txt(
    path: &Path,
    username: &str,
    display_name: &str,
    messages: &[ExportMessage],
) -> Result<(), String> {
    let file = std::fs::File::create(path)
        .map_err(|e| format!("Cannot create {}: {}", path.display(), e))?;
    let mut f = BufWriter::new(file);
    let write_error = |error| format!("Cannot write {}: {error}", path.display());

    writeln!(f, "Telegram 聊天记录: {} ({})", display_name, username).map_err(&write_error)?;
    writeln!(f, "总消息数: {}", messages.len()).map_err(&write_error)?;
    writeln!(
        f,
        "时间范围: {} ~ {}",
        messages.first().map(|m| m.time.as_str()).unwrap_or(""),
        messages.last().map(|m| m.time.as_str()).unwrap_or("")
    )
    .map_err(&write_error)?;
    writeln!(f, "{}", "=".repeat(60)).map_err(&write_error)?;
    writeln!(f).map_err(&write_error)?;

    for m in messages {
        writeln!(f, "[{}] {}: {}", m.time, m.sender, m.content).map_err(&write_error)?;
    }

    f.flush().map_err(write_error)
}

fn export_csv(path: &Path, messages: &[ExportMessage]) -> Result<(), String> {
    let file = std::fs::File::create(path)
        .map_err(|e| format!("Cannot create {}: {}", path.display(), e))?;
    let mut f = BufWriter::new(file);
    let write_error = |error| format!("Cannot write {}: {error}", path.display());

    // Write BOM for Excel compatibility
    f.write_all(&[0xEF, 0xBB, 0xBF]).map_err(&write_error)?;
    writeln!(f, "时间,发送者,类型,内容").map_err(&write_error)?;

    for m in messages {
        // Escape CSV fields
        let time = escape_csv(&m.time);
        let sender = escape_csv(&m.sender);
        let type_name = escape_csv(&m.type_name);
        let content = escape_csv(&m.content);
        writeln!(f, "{},{},{},{}", time, sender, type_name, content).map_err(&write_error)?;
    }

    f.flush().map_err(write_error)
}

fn export_json(path: &Path, messages: &[ExportMessage]) -> Result<(), String> {
    let file = std::fs::File::create(path)
        .map_err(|e| format!("Cannot create {}: {}", path.display(), e))?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, messages)
        .map_err(|e| format!("JSON serialization error: {}", e))?;
    writer
        .flush()
        .map_err(|e| format!("Cannot write {}: {}", path.display(), e))
}

/// Export a media file, decrypting V2 files on the fly.
fn export_media_with_decrypt(
    src: &Path,
    output_dir: &Path,
    session_name: &str,
    cat_name: &str,
    msg_type: i64,
    index: usize,
    media_keys: Option<&crate::media_key::MediaKeys>,
) -> Result<PathBuf, String> {
    let use_decrypt = crate::media_decrypt::file_has_v2_magic(src)?;

    if use_decrypt {
        if let Some(keys) = media_keys {
            // Decrypt directly to a temporary output location, then rename
            // based on the decrypted payload magic.
            let filename = format!(
                "{}_{}_{}_{:04}.dat",
                media::sanitize_filename(session_name),
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

fn export_sticker_info(
    info: &media::StickerInfo,
    cached_source: Option<&Path>,
    output_dir: &Path,
    session_name: &str,
    index: usize,
) -> Result<PathBuf, String> {
    let source = cached_source.ok_or_else(|| "cached sticker is unavailable".to_string())?;
    let data = std::fs::read(source)
        .map_err(|e| format!("Read cached sticker {}: {}", source.display(), e))?;
    write_sticker_candidate(&data, info, output_dir, session_name, index)?
        .ok_or_else(|| "cached sticker could not be decoded".to_string())
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
        media::sanitize_filename(session_name),
        index,
        ext
    );
    let dest = output_dir.join(filename);
    std::fs::write(&dest, data).map_err(|e| format!("Write sticker {}: {}", dest.display(), e))?;
    Ok(dest)
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
    use std::os::unix::fs::PermissionsExt;
    use tempfile::{tempdir, TempDir};

    fn export_all_voice_messages_with_decoder(
        root: &Path,
        username: &str,
        output_dir: &Path,
        start_index: usize,
        decoder: Option<&Path>,
    ) -> Result<Vec<(usize, PathBuf)>, String> {
        let store = SourceStore::from_plaintext(root)?;
        super::export_all_voice_messages_with_decoder(
            &store,
            username,
            output_dir,
            start_index,
            decoder,
        )
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
                voice_data BLOB
            )",
            [],
        )
        .unwrap();
        for (timestamp, local_id, suffix) in [(1000, 1, " old"), (1002, 2, " new")] {
            let mut data = vec![2];
            data.extend_from_slice(&native_voice_fixture(suffix));
            conn.execute(
                "INSERT INTO VoiceInfo
                 (chat_name_id, create_time, local_id, svr_id, voice_data)
                 VALUES (1, ?1, ?2, ?3, ?4)",
                params![timestamp, local_id, 9000 + local_id, data],
            )
            .unwrap();
        }
        dir
    }

    fn native_voice_fixture(suffix: &str) -> Vec<u8> {
        let mut data = native_voice_header();
        data.extend_from_slice(suffix.as_bytes());
        data
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

    fn fixture_media_keys() -> crate::media_key::MediaKeys {
        crate::media_key::MediaKeys {
            aes_key: *b"0123456789abcdef",
            xor_key: 0,
        }
    }

    fn v2_media_fixture(plaintext: &[u8], keys: &crate::media_key::MediaKeys) -> Vec<u8> {
        use aes::cipher::{Block, BlockCipherEncrypt, KeyInit};

        let mut encrypted = plaintext.to_vec();
        let remainder = encrypted.len() % 16;
        let pad_len = if remainder == 0 { 16 } else { 16 - remainder };
        encrypted.extend(std::iter::repeat_n(pad_len as u8, pad_len));
        let cipher = aes::Aes128::new_from_slice(&keys.aes_key).unwrap();
        for block in encrypted.chunks_exact_mut(16) {
            let block: &mut Block<aes::Aes128> =
                block.try_into().expect("AES block chunk is 16 bytes");
            cipher.encrypt_block(block);
        }
        let mut data = crate::media_decrypt::V2_MAGIC.to_vec();
        data.extend_from_slice(&(plaintext.len() as u32).to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.push(1);
        data.extend_from_slice(&encrypted);
        data
    }

    fn executable_script(body: &str) -> (TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let path = dir.path().join(current_voice_decoder_command());
        std::fs::write(&path, body).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        (dir, path)
    }

    #[test]
    fn export_messages_writes_complete_chronological_archive() {
        let decrypted = create_export_decrypted_dir();
        let output = tempdir().unwrap();
        let store = SourceStore::from_plaintext(decrypted.path()).unwrap();
        let results = export_messages(MessageExportConfig {
            store: &store,
            session_query: "tgid_export",
            output_dir: output.path(),
            jobs: 1,
        })
        .unwrap();

        assert_eq!(
            results
                .iter()
                .map(|(format, _)| *format)
                .collect::<Vec<_>>(),
            vec!["txt", "csv", "json"]
        );
        let data: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(output.path().join("chat.json")).unwrap(),
        )
        .unwrap();
        let messages = data.as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["timestamp"], 1000);
        assert_eq!(messages[0]["sender"], "Export Remark");
        assert_eq!(messages[1]["timestamp"], 1001);
        assert!(output.path().join("chat.txt").exists());
        assert!(output.path().join("chat.csv").exists());
    }

    #[test]
    fn export_messages_skips_shard_without_session_table() {
        let decrypted = create_export_decrypted_dir();
        Connection::open(decrypted.path().join("message/message_1.db")).unwrap();
        let output = tempdir().unwrap();
        let store = SourceStore::from_plaintext(decrypted.path()).unwrap();

        let results = export_messages(MessageExportConfig {
            store: &store,
            session_query: "tgid_export",
            output_dir: output.path(),
            jobs: 1,
        })
        .unwrap();

        assert_eq!(results.len(), 3);
    }

    #[test]
    fn export_messages_propagates_row_errors() {
        let decrypted = create_export_decrypted_dir();
        let conn = Connection::open(decrypted.path().join("message/message_0.db")).unwrap();
        let table = db::quote_identifier(&db::msg_table_name("tgid_export"));
        conn.execute(
            &format!(
                "INSERT INTO {table} (local_type, create_time, {}, {}, {})
                 VALUES (x'ff', 1002, 'broken', NULL, x'')",
                db::quote_identifier(dictionary::msg_body_column()),
                db::quote_identifier(dictionary::msg_compression_marker_column()),
                db::quote_identifier(dictionary::msg_packed_meta_column()),
            ),
            [],
        )
        .unwrap();
        let output = tempdir().unwrap();
        let store = SourceStore::from_plaintext(decrypted.path()).unwrap();

        let error = export_messages(MessageExportConfig {
            store: &store,
            session_query: "tgid_export",
            output_dir: output.path(),
            jobs: 1,
        })
        .unwrap_err();

        assert!(error.contains("Decode message"));
    }

    #[test]
    fn full_export_deduplicates_repeated_stickers() {
        let info =
            media::parse_sticker_info(r#"<emoji md5="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"></emoji>"#);
        let mut seen = HashSet::new();
        assert!(record_new_sticker(&mut seen, &info));
        assert!(!record_new_sticker(&mut seen, &info));
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
    fn archive_voice_without_decoder_falls_back_to_native() {
        let decrypted = create_voice_decrypted_dir();
        let output = tempdir().unwrap();
        let results = export_all_voice_messages_with_decoder(
            decrypted.path(),
            "tgid_voices",
            output.path(),
            3,
            None,
        )
        .unwrap();

        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|(_, path)| { path.extension().and_then(|ext| ext.to_str()) == Some("voice") }));
        assert!(results.iter().all(|(_, path)| {
            std::fs::read(path)
                .unwrap()
                .starts_with(&native_voice_header())
        }));
    }

    #[test]
    fn archive_voice_decoder_failure_falls_back_to_native() {
        let decrypted = create_voice_decrypted_dir();
        let output = tempdir().unwrap();
        let (_decoder_dir, decoder) = executable_script(
            "#!/bin/sh\nout=''\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = '-o' ]; then shift; out=\"$1\"; fi\n  shift\ndone\nprintf 'partial' > \"$out\"\nexit 1\n",
        );
        let results = export_all_voice_messages_with_decoder(
            decrypted.path(),
            "tgid_voices",
            output.path(),
            1,
            Some(&decoder),
        )
        .unwrap();

        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|(_, path)| { path.extension().and_then(|ext| ext.to_str()) == Some("voice") }));
        assert_eq!(std::fs::read_dir(output.path()).unwrap().count(), 2);
    }

    #[test]
    fn archive_voice_decoder_without_output_falls_back_to_native() {
        let decrypted = create_voice_decrypted_dir();
        let output = tempdir().unwrap();
        for (index, local_id) in [(1, 1), (2, 2)] {
            std::fs::write(
                voice_output_path(output.path(), "tgid_voices", index, local_id, "wav"),
                b"RIFF....WAVE................................",
            )
            .unwrap();
        }
        let (_decoder_dir, decoder) = executable_script("#!/bin/sh\nexit 0\n");
        let results = export_all_voice_messages_with_decoder(
            decrypted.path(),
            "tgid_voices",
            output.path(),
            1,
            Some(&decoder),
        )
        .unwrap();

        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|(_, path)| path.extension().and_then(|ext| ext.to_str()) == Some("voice")));
        assert_eq!(std::fs::read_dir(output.path()).unwrap().count(), 2);
    }

    #[test]
    fn archive_voice_decoder_invalid_output_falls_back_to_native() {
        let decrypted = create_voice_decrypted_dir();
        let output = tempdir().unwrap();
        let (_decoder_dir, decoder) = executable_script(
            "#!/bin/sh\nout=''\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = '-o' ]; then shift; out=\"$1\"; fi\n  shift\ndone\nprintf 'not a wav file' > \"$out\"\n",
        );
        let results = export_all_voice_messages_with_decoder(
            decrypted.path(),
            "tgid_voices",
            output.path(),
            1,
            Some(&decoder),
        )
        .unwrap();

        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|(_, path)| path.extension().and_then(|ext| ext.to_str()) == Some("voice")));
    }

    #[test]
    fn archive_voice_uses_current_rust_decoder_protocol() {
        let decrypted = create_voice_decrypted_dir();
        let output = tempdir().unwrap();
        let (_decoder_dir, decoder) = executable_script(
            "#!/bin/sh\n[ \"$1\" = 'decode' ] || exit 2\nout=''\nwav=0\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = '-o' ]; then shift; out=\"$1\"; fi\n  if [ \"$1\" = '--wav' ]; then wav=1; fi\n  shift\ndone\n[ \"$wav\" = 1 ] || exit 3\nprintf 'RIFF....WAVE................................' > \"$out\"\n",
        );
        let results = export_all_voice_messages_with_decoder(
            decrypted.path(),
            "tgid_voices",
            output.path(),
            3,
            Some(&decoder),
        )
        .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 3);
        assert!(results
            .iter()
            .all(|(_, path)| { path.extension().and_then(|ext| ext.to_str()) == Some("wav") }));
        assert!(std::fs::read(&results[0].1).unwrap().starts_with(b"RIFF"));
    }

    #[test]
    fn archive_voice_row_failure_fails_the_export() {
        let decrypted = create_voice_decrypted_dir();
        let conn = Connection::open(decrypted.path().join("message/media_0.db")).unwrap();
        conn.execute(
            "INSERT INTO VoiceInfo
             (chat_name_id, create_time, local_id, svr_id, voice_data)
             VALUES (1, 1003, 3, 9003, x'')",
            [],
        )
        .unwrap();
        let output = tempdir().unwrap();

        let error = export_all_voice_messages_with_decoder(
            decrypted.path(),
            "tgid_voices",
            output.path(),
            1,
            None,
        )
        .unwrap_err();

        assert!(error.contains("Voice #3 export failed"));
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
    }

    #[test]
    fn pat_app_message_is_not_file_export_candidate() {
        let message = export_message(
            (62_i64 << 32) | 49,
            r#"<msg><appmsg><title>我拍了拍 "Bob"</title><type>62</type><appattach><totallen>0</totallen></appattach><patinfo><template>我拍了拍 "\${tgid_bob}"</template></patinfo></appmsg>"#,
            Vec::new(),
        );
        assert_eq!(file_identifier(&message), None);
        assert!(!is_file_message(&message));
    }

    #[test]
    fn export_media_with_decrypt_decrypts_v2_by_magic() {
        let temp = tempdir().unwrap();
        let output = tempdir().unwrap();
        let source = temp.path().join("cached-image");
        let keys = fixture_media_keys();
        let jpeg = b"\xff\xd8\xff\xe0fixture-jpeg";
        std::fs::write(&source, v2_media_fixture(jpeg, &keys)).unwrap();
        let exported = export_media_with_decrypt(
            &source,
            output.path(),
            "session",
            "Image",
            3,
            7,
            Some(&keys),
        )
        .unwrap();
        assert_eq!(
            exported.extension().and_then(|ext| ext.to_str()),
            Some("jpg")
        );
        assert_eq!(std::fs::read(exported).unwrap(), jpeg);
    }
}
