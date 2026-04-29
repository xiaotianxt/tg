use rusqlite::Connection;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::db;
use crate::media;
use crate::media_index::MediaIndex;
use crate::message;
use crate::parallel;

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
    pub limit: usize,
    pub since: Option<i64>,
    pub jobs: usize,
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

/// Export messages for a session.
pub fn export_messages(
    decrypted_dir: &Path,
    session_query: &str,
    format: &str,
    output_dir: &Path,
    media_dir: Option<&Path>,
    jobs: usize,
) -> Result<Vec<(&'static str, PathBuf)>, String> {
    let (contact_db, message_dbs) = db::find_decrypted_dbs(decrypted_dir);
    let contact_db_path = contact_db.as_deref();

    // Resolve session username
    let username =
        db::resolve_username_for_messages(session_query, contact_db_path, &message_dbs, jobs)?;

    // Load contacts for display name
    let contacts = contact_db_path
        .and_then(|p| db::load_contacts(p).ok())
        .unwrap_or_default();
    let display_name = contacts
        .get(&username)
        .map(|c| c.display.as_str())
        .unwrap_or(&username);

    // Table name
    let table_name = db::msg_table_name(&username);
    let cst_offset = chrono::FixedOffset::east_opt(8 * 3600).unwrap();

    let db_jobs = parallel::job_count(jobs, 8);
    let per_db_messages = parallel::map_ordered(message_dbs.clone(), db_jobs, |db_path| {
        let mut messages = Vec::new();
        let conn = match Connection::open(&db_path) {
            Ok(c) => c,
            Err(_) => return messages,
        };

        let sql = format!(
            "SELECT local_type, create_time, message_content, WCDB_CT_message_content, packed_info_data \
             FROM {} WHERE create_time > 0 ORDER BY create_time ASC",
            table_name
        );

        let rows: Vec<ExportMessageRow> = match conn.prepare(&sql) {
            Ok(mut stmt) => match stmt.query_map([], |row| {
                let wcdb_ct: Option<i64> = row.get::<_, Option<i64>>(3)?;
                let content: String = if wcdb_ct == Some(4) {
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
                    row.get::<_, Option<i64>>(0)?.unwrap_or(-1),
                    row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    content,
                    row.get::<_, Option<i64>>(3)?,
                    packed_info,
                ))
            }) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(_) => vec![],
            },
            Err(_) => vec![],
        };

        for (local_type, create_time, content, wcdb_ct, packed_info) in rows {
            let time_str = chrono::DateTime::from_timestamp(create_time, 0)
                .map(|t| {
                    t.with_timezone(&cst_offset)
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string()
                })
                .unwrap_or_default();

            let decoded = message::decode_message(
                local_type as i32,
                &content,
                display_name,
                wcdb_ct,
                &packed_info,
                |id| crate::db::resolve_sender_name(id, &contacts),
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
    });

    let mut all_messages: Vec<ExportMessage> = Vec::new();
    for messages in per_db_messages {
        all_messages.extend(messages);
    }

    all_messages.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

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
        let media_index = MediaIndex::load(&telegram_base, &username, &["Image", "Video"], jobs);

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
        display_name,
        username
    ))?;
    out.flush()?;
    Ok(results)
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
    if let Some(index) = config.index {
        if index == 0 {
            return Err("--index is 1-based and must be greater than 0".to_string());
        }
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

    let media_keys = crate::media_key::find_media_keys(&telegram_base);
    if let Err(ref e) = media_keys {
        log::warn!("Cannot derive media decryption keys: {}", e);
    }
    let media_keys = media_keys.ok();

    let image_jobs = selected
        .into_iter()
        .filter_map(|candidate| {
            candidate.source.map(|src| MediaExportJob::Cached {
                index: candidate.index,
                src,
                category: "Image",
                msg_type: 3,
            })
        })
        .collect::<Vec<_>>();
    let image_job_count = parallel::job_count(config.jobs, 4);
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
                config.output_dir,
                &username,
                category,
                msg_type,
                index,
                media_keys.as_ref(),
            ),
        },
        MediaExportJob::Sticker { .. } => unreachable!("image command only exports images"),
    });

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

fn load_image_messages(
    decrypted_dir: &Path,
    session_query: &str,
    since: Option<i64>,
    limit: usize,
    jobs: usize,
) -> Result<(String, Vec<ImageMessage>), String> {
    let (contact_db, message_dbs) = db::find_decrypted_dbs(decrypted_dir);
    let username = db::resolve_username_for_messages(
        session_query,
        contact_db.as_deref(),
        &message_dbs,
        jobs,
    )?;
    let table_name = db::msg_table_name(&username);
    let cst_offset = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
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

        let sql = format!(
            "SELECT create_time, message_content, WCDB_CT_message_content, packed_info_data \
             FROM {} WHERE create_time > 0 AND local_type = 3{} ORDER BY create_time DESC LIMIT {}",
            table_name, since_clause, limit
        );

        let rows: Vec<(i64, String, Vec<u8>)> = match conn.prepare(&sql) {
            Ok(mut stmt) => match stmt.query_map([], |row| {
                let wcdb_ct: Option<i64> = row.get::<_, Option<i64>>(2)?;
                let content: String = if wcdb_ct == Some(4) {
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
            let time = chrono::DateTime::from_timestamp(timestamp, 0)
                .map(|t| {
                    t.with_timezone(&cst_offset)
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string()
                })
                .unwrap_or_else(|| timestamp.to_string());
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
    messages.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
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

    writeln!(f, "Telegram聊天记录: {} ({})", display_name, username).ok();
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
