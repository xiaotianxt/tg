use rusqlite::Connection;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::db;
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

/// Export messages for a session.
pub fn export_messages(
    decrypted_dir: &Path,
    session_query: &str,
    format: &str,
    output_dir: &Path,
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
            "SELECT local_type, create_time, message_content, WCDB_CT_message_content \
             FROM {} WHERE create_time > 0 ORDER BY create_time ASC",
            table_name
        );

        let rows: Vec<(i64, i64, String, Option<i64>)> = match conn.prepare(&sql) {
            Ok(mut stmt) => match stmt.query_map([], |row| {
                // message_content can be TEXT or BLOB; read as String when possible
                let content: String = match row.get::<_, Option<String>>(2) {
                    Ok(Some(s)) => s,
                    _ => match row.get::<_, Option<Vec<u8>>>(2) {
                        Ok(Some(b)) => String::from_utf8(b).unwrap_or_default(),
                        _ => String::new(),
                    },
                };
                Ok((
                    row.get::<_, Option<i64>>(0)?.unwrap_or(-1),
                    row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    content,
                    row.get::<_, Option<i64>>(3)?,
                ))
            }) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(_) => vec![],
            },
            Err(_) => vec![],
        };

        for (local_type, create_time, content, wcdb_ct) in rows {
            let time_str = chrono::DateTime::from_timestamp(create_time, 0)
                .map(|t| t.with_timezone(&cst_offset).format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default();

            let decoded = message::decode_message(
                local_type as i32,
                &content,
                display_name,
                wcdb_ct,
                |id| crate::db::resolve_sender_name(id, &contacts),
            );

            all_messages.push(ExportMessage {
                time: time_str,
                timestamp: create_time,
                sender: decoded.display_name,
                msg_type: local_type,
                type_name: decoded.msg_type.to_string(),
                content: decoded.content,
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

    println!("Exported {} messages for {} ({})", all_messages.len(), display_name, username);
    Ok(results)
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

fn escape_csv(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}
