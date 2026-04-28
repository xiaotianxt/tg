use rusqlite::Connection;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::db;
use crate::media;
use crate::message;

#[derive(serde::Serialize)]
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

/// Export messages for a session.
pub fn export_messages(
    decrypted_dir: &Path,
    session_query: &str,
    format: &str,
    output_dir: &Path,
    media_dir: Option<&Path>,
) -> Result<Vec<(&'static str, PathBuf)>, String> {
    let (contact_db, message_dbs) = db::find_decrypted_dbs(decrypted_dir);
    let contact_db_path = contact_db.as_deref();

    // Resolve session username
    let username = db::resolve_username(session_query, contact_db_path)?;

    // Load contacts for display name
    let contacts = contact_db_path
        .and_then(|p| db::load_contacts(p).ok())
        .unwrap_or_default();
    let display_name = contacts.get(&username)
        .map(|c| c.display.as_str())
        .unwrap_or(&username);

    // Table name
    let table_name = db::msg_table_name(&username);
    let mut all_messages: Vec<ExportMessage> = Vec::new();
    let cst_offset = chrono::FixedOffset::east_opt(8 * 3600).unwrap();

    for db_path in &message_dbs {
        let conn = match Connection::open(db_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let sql = format!(
            "SELECT local_type, create_time, message_content, WCDB_CT_message_content, packed_info_data \
             FROM {} WHERE create_time > 0 ORDER BY create_time ASC",
            table_name
        );

        let rows: Vec<(i64, i64, String, Option<i64>, Vec<u8>)> = match conn.prepare(&sql) {
            Ok(mut stmt) => match stmt.query_map([], |row| {
                let wcdb_ct: Option<i64> = row.get::<_, Option<i64>>(3)?;
                let content: String = if wcdb_ct == Some(4) {
                    if let Ok(b) = row.get::<_, Vec<u8>>(2) {
                        message::try_decompress(&b).unwrap_or_default()
                    } else { String::new() }
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
                .map(|t| t.with_timezone(&cst_offset).format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default();

            let decoded = message::decode_message(
                local_type as i32,
                &content,
                display_name,
                wcdb_ct,
                &packed_info,
                |id| crate::db::resolve_sender_name(id, &contacts),
            );

            all_messages.push(ExportMessage {
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
    }

    all_messages.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

    if all_messages.is_empty() {
        return Err(format!("No messages found for '{}'", username));
    }

    // Create output directory
    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("Cannot create output dir: {}", e))?;

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

    // Export media files if requested
    if let Some(mdir) = media_dir {
        let telegram_base = match media::find_telegram_base_path() {
            Some(p) => p,
            None => {
                eprintln!("Warning: Telegram data directory not found, media export skipped");
                return Ok(results);
            }
        };

        // Derive media decryption keys (V2 .dat)
        let media_keys = crate::media_key::find_media_keys(&telegram_base);
        if let Err(ref e) = media_keys {
            eprintln!("Warning: cannot derive media decryption keys: {}", e);
        }
        let media_keys = media_keys.ok();

        let mut exported = 0;
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

                if let Some(src) = media::find_cached_media(&telegram_base, &username, cat_name, &identifier) {
                    let index = exported + 1;
                    let result = export_media_with_decrypt(&src, mdir, &username, cat_name, m.msg_type, index, media_keys.as_ref());
                    match result {
                        Ok(path) => {
                            println!("  Media #{}: {}", index, path.file_name().and_then(|n| n.to_str()).unwrap_or("?"));
                            exported += 1;
                        }
                        Err(e) => {
                            eprintln!("  Media #{} export failed: {}", index, e);
                        }
                    }
                }
            }
        }
        if exported > 0 {
            println!("Exported {} media files", exported);
        }
    }

    println!("Exported {} messages for {} ({})", all_messages.len(), display_name, username);
    Ok(results)
}

/// Try to extract media cache identifier from packed_info protobuf.
fn try_protobuf_identifier(data: &[u8]) -> Option<String> {
    if data.is_empty() { return None; }
    if let Some(v2) = crate::media_pb::parse_img2(data) {
        if let Some(img) = v2.image {
            if !img.filename.is_empty() { return Some(img.filename); }
        }
        if let Some(vid) = v2.video {
            if !vid.filename.is_empty() { return Some(vid.filename); }
        }
    }
    if let Some(v1) = crate::media_pb::parse_img(data) {
        if !v1.filename.is_empty() { return Some(v1.filename); }
    }
    None
}

/// Extract attribute value from XML-like string within content.
fn extract_xml_attr_str(content: &str, attr: &str) -> Option<String> {
    let pattern = format!(r#"{}=""#, attr);
    let start = content.find(&pattern)?;
    let value_start = start + pattern.len();
    if value_start >= content.len() { return None; }
    let rest = &content[value_start..];
    if !rest.starts_with('"') { return None; }
    let rest = &rest[1..];
    let end = rest.find('"')?;
    let value = rest[..end].to_string();
    if value.is_empty() { None } else { Some(value) }
}

fn export_txt(path: &Path, username: &str, display_name: &str, messages: &[ExportMessage]) -> Result<(), String> {
    let mut f = std::fs::File::create(path)
        .map_err(|e| format!("Cannot create {}: {}", path.display(), e))?;

    writeln!(f, "Telegram聊天记录: {} ({})", display_name, username).ok();
    writeln!(f, "总消息数: {}", messages.len()).ok();
    writeln!(f, "时间范围: {} ~ {}", messages.first().map(|m| &m.time).unwrap_or(&"".to_string()),
        messages.last().map(|m| &m.time).unwrap_or(&"".to_string())).ok();
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
    std::fs::write(path, json)
        .map_err(|e| format!("Cannot write {}: {}", path.display(), e))?;
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
    media::export_media_file(src, output_dir, session_name, &format!("{}_{}", cat_name, msg_type), index)
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn escape_csv(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}
