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
}

struct MediaItem {
    msg_type: i64,
    raw_content: String,
    seq: usize,
}

fn get_message_dbs(decrypted_dir: &Path) -> Vec<PathBuf> {
    let msg_dir = decrypted_dir.join("message");
    let mut dbs = Vec::new();
    if msg_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&msg_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path.file_name()
                    .and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with("message_") && name.ends_with(".db") && !name.contains("fts") {
                    dbs.push(path);
                }
            }
        }
    }
    dbs.sort();
    dbs
}

pub fn export_messages(
    decrypted_dir: &Path,
    session_query: &str,
    format: &str,
    output_dir: &Path,
    media_dir: Option<&Path>,
) -> Result<Vec<(&'static str, PathBuf)>, String> {
    let (contact_db, _) = {
        let contact_db = decrypted_dir.join("contact/contact.db");
        let contact_db = if contact_db.exists() { Some(contact_db) }
            else { None };
        let msg_dbs = get_message_dbs(decrypted_dir);
        (contact_db, msg_dbs)
    };

    let contact_db_path = contact_db.as_deref();
    let contacts = contact_db_path
        .and_then(|p| db::load_contacts(p).ok())
        .unwrap_or_default();

    let username = match contact_db_path.and_then(|_| resolve_session(session_query, &contacts)) {
        Some(u) => u,
        None if session_query.starts_with("tgid_") || session_query.starts_with("gh_") || session_query.contains("@chatroom") => session_query.to_string(),
        _ => return Err(format!("No contact found matching '{}'", session_query)),
    };

    let display_name = contacts.get(&username)
        .map(|c| c.display.as_str())
        .unwrap_or(&username);

    let table_name = db::msg_table_name(&username);

    let message_dbs = get_message_dbs(decrypted_dir);
    let mut all_messages: Vec<ExportMessage> = Vec::new();
    let mut media_items: Vec<MediaItem> = Vec::new();
    let mut seq: usize = 0;
    let cst_offset = chrono::FixedOffset::east_opt(8 * 3600).unwrap();

    for db_path in &message_dbs {
        let conn = match Connection::open(db_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let sql = format!(
            "SELECT local_type, create_time, message_content, WCDB_CT_message_content \
             FROM {} WHERE create_time > 0 ORDER BY create_time ASC",
            table_name
        );

        let rows: Vec<(i64, i64, String, Option<i64>)> = match conn.prepare(&sql) {
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
                Ok((
                    row.get::<_, Option<i64>>(0)?.unwrap_or(-1),
                    row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    content,
                    wcdb_ct,
                ))
            }) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(_) => vec![],
            },
            Err(_) => vec![],
        };

        for (local_type, create_time, content, wcdb_ct) in rows {
            seq += 1;
            let time_str = chrono::DateTime::from_timestamp(create_time, 0)
                .map(|t| t.with_timezone(&cst_offset).format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default();

            let decoded = message::decode_message(
                local_type as i32,
                &content,
                display_name,
                wcdb_ct,
                |id| db::resolve_sender_name(id, &contacts),
            );

            all_messages.push(ExportMessage {
                time: time_str,
                timestamp: create_time,
                sender: decoded.display_name,
                msg_type: local_type,
                type_name: decoded.msg_type.to_string(),
                content: decoded.content,
            });

            if media_dir.is_some() && matches!(local_type, 3 | 43 | 47) && !content.is_empty() {
                media_items.push(MediaItem {
                    msg_type: local_type,
                    raw_content: content,
                    seq,
                });
            }
        }
    }

    all_messages.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

    if all_messages.is_empty() {
        return Err(format!("No messages found for '{}'", username));
    }

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

    if let Some(mdir) = media_dir {
        let count = export_session_media(mdir, &username, &media_items)?;
        if count > 0 {
            println!("Exported {} media files to {}", count, mdir.display());
        }
    }

    println!("Exported {} messages for {} ({})", all_messages.len(), display_name, username);
    Ok(results)
}

fn resolve_session(query: &str, contacts: &std::collections::HashMap<String, db::Contact>) -> Option<String> {
    if let Some(c) = contacts.get(query) {
        return Some(c.username.clone());
    }
    let results: Vec<_> = contacts.values()
        .filter(|c| c.display.contains(query) || c.nick_name.contains(query))
        .collect();
    results.first().map(|c| c.username.clone())
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

    f.write_all(&[0xEF, 0xBB, 0xBF]).ok();
    writeln!(f, "时间,发送者,类型,内容").ok();

    for m in messages {
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

fn escape_csv(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn export_session_media(
    output_dir: &Path,
    session_tgid: &str,
    items: &[MediaItem],
) -> Result<usize, String> {
    let telegram_base = match media::find_telegram_base_path() {
        Some(p) => p,
        None => {
            eprintln!("Warning: Telegram data directory not found, media export skipped");
            return Ok(0);
        }
    };

    let mut exported = 0;
    for item in items {
        let (category, identifier, type_name) = match item.msg_type {
            3 => {
                let info = media::parse_image_info(&item.raw_content);
                ("Image", info.aes_key.clone(), "image")
            }
            43 => {
                let info = media::parse_video_info(&item.raw_content);
                ("Video", info.aes_key.clone(), "video")
            }
            47 => {
                let info = media::parse_sticker_info(&item.raw_content);
                let id = if !info.product_id.is_empty() { info.product_id.clone() } else { info.url.clone() };
                ("Image", id, "sticker")
            }
            _ => continue,
        };

        if identifier.is_empty() {
            continue;
        }

        if let Some(src) = media::find_cached_media(&telegram_base, session_tgid, category, &identifier) {
            match media::export_media_file(&src, output_dir, session_tgid, type_name, item.seq) {
                Ok(path) => {
                    println!("  Media #{}: {}", item.seq, path.file_name().and_then(|n| n.to_str()).unwrap_or("?"));
                    exported += 1;
                }
                Err(e) => {
                    eprintln!("  Media #{} export failed: {}", item.seq, e);
                }
            }
        }
    }

    Ok(exported)
}
