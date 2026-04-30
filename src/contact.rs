use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;

/// Contact row plus the display fields tg needs for resolving chat names.
pub(crate) struct Contact {
    pub username: String,
    pub nick_name: String,
    pub remark: String,
    pub alias: String,
    pub is_stranger: bool,
}

impl Contact {
    pub(crate) fn personal_display_name(&self) -> &str {
        let preferred = if self.remark.is_empty() {
            &self.nick_name
        } else {
            &self.remark
        };
        first_non_empty([preferred, &self.username])
    }

    pub(crate) fn display_name(&self, mode: DisplayNameMode) -> &str {
        match mode {
            DisplayNameMode::PersonalRemark => self.personal_display_name(),
            DisplayNameMode::Anonymous => first_non_empty([&self.nick_name, &self.username]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayNameMode {
    PersonalRemark,
    Anonymous,
}

/// Resolve a sender account id into the display name used by the default UI mode.
pub(crate) fn resolve_sender_name(sender_id: &str, contacts: &HashMap<String, Contact>) -> String {
    resolve_sender_name_with_mode(
        sender_id,
        contacts,
        DisplayNameMode::PersonalRemark,
        &HashMap::new(),
    )
}

pub(crate) fn resolve_sender_name_with_mode(
    sender_id: &str,
    contacts: &HashMap<String, Contact>,
    mode: DisplayNameMode,
    room_member_names: &HashMap<String, String>,
) -> String {
    if mode == DisplayNameMode::Anonymous {
        if let Some(name) = room_member_names
            .get(sender_id)
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
        {
            return name.to_string();
        }
    }

    contacts
        .get(sender_id)
        .map(|contact| contact.display_name(mode))
        .unwrap_or(sender_id)
        .to_string()
}

pub(crate) fn load_contacts(contact_db: &Path) -> Result<HashMap<String, Contact>, String> {
    let conn =
        Connection::open(contact_db).map_err(|e| format!("Cannot open contact DB: {}", e))?;

    let mut contacts = load_contact_table(&conn, ContactTable::Contact)
        .map_err(|e| format!("Contact read error: {}", e))?;
    for (username, stranger) in
        load_contact_table(&conn, ContactTable::Stranger).unwrap_or_default()
    {
        contacts
            .entry(username)
            .and_modify(|contact| contact.is_stranger = true)
            .or_insert(stranger);
    }

    Ok(contacts)
}

fn load_contact_table(
    conn: &Connection,
    table: ContactTable,
) -> rusqlite::Result<HashMap<String, Contact>> {
    let sql = format!(
        "SELECT username, nick_name, remark, alias FROM {}",
        table.name()
    );
    let mut stmt = conn.prepare(&sql)?;
    let contacts = stmt
        .query_map([], |row| {
            let username: String = row.get(0)?;
            let nick_name: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
            let remark: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
            let alias: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
            Ok((
                username.clone(),
                Contact {
                    username,
                    nick_name,
                    remark,
                    alias,
                    is_stranger: table.is_stranger(),
                },
            ))
        })?
        .filter_map(|row| row.ok())
        .collect();
    Ok(contacts)
}

#[derive(Clone, Copy)]
enum ContactTable {
    Contact,
    Stranger,
}

impl ContactTable {
    fn name(self) -> &'static str {
        match self {
            ContactTable::Contact => "contact",
            ContactTable::Stranger => "stranger",
        }
    }

    fn is_stranger(self) -> bool {
        matches!(self, ContactTable::Stranger)
    }
}

fn first_non_empty<const N: usize>(values: [&str; N]) -> &str {
    values
        .into_iter()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or("")
}

pub(crate) fn load_chat_room_member_names(
    contact_db: &Path,
    room_username: &str,
) -> Result<HashMap<String, String>, String> {
    let conn =
        Connection::open(contact_db).map_err(|e| format!("Cannot open contact DB: {}", e))?;
    let ext_buffer: Option<Vec<u8>> = conn
        .query_row(
            "SELECT ext_buffer FROM chat_room WHERE username = ?1",
            params![room_username],
            |row| row.get::<_, Option<Vec<u8>>>(0),
        )
        .optional()
        .map_err(|e| format!("Chat room member query error: {}", e))?
        .flatten();

    Ok(ext_buffer
        .as_deref()
        .map(parse_chat_room_member_names)
        .unwrap_or_default())
}

fn parse_chat_room_member_names(data: &[u8]) -> HashMap<String, String> {
    use crate::media_pb::wire::{decode_varint, skip_field, tag_field};

    let mut names = HashMap::new();
    let mut pos = 0;
    while pos < data.len() {
        let Some(tag) = decode_varint(data, &mut pos) else {
            break;
        };
        let (field, wire) = tag_field(tag);
        if (field, wire) != (1, 2) {
            if skip_field(data, &mut pos, wire).is_none() {
                break;
            }
            continue;
        }

        let Some(len) = decode_varint(data, &mut pos).map(|len| len as usize) else {
            break;
        };
        let Some(end) = pos.checked_add(len) else {
            break;
        };
        let Some(member) = data.get(pos..end) else {
            break;
        };
        pos = end;

        if let Some((username, display_name)) = parse_chat_room_member_name(member) {
            names.insert(username, display_name);
        }
    }
    names
}

fn parse_chat_room_member_name(data: &[u8]) -> Option<(String, String)> {
    use crate::media_pb::wire::{decode_string, decode_varint, skip_field, tag_field};

    let mut username = String::new();
    let mut display_name = String::new();
    let mut pos = 0;
    while pos < data.len() {
        let tag = decode_varint(data, &mut pos)?;
        let (field, wire) = tag_field(tag);
        match (field, wire) {
            (1, 2) => username = decode_string(data, &mut pos)?,
            (2, 2) => display_name = decode_string(data, &mut pos)?,
            _ => skip_field(data, &mut pos, wire)?,
        }
    }

    let username = username.trim();
    let display_name = display_name.trim();
    if username.is_empty() || display_name.is_empty() {
        return None;
    }
    Some((username.to_string(), display_name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn push_proto_varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    fn push_proto_len_field(out: &mut Vec<u8>, field: u32, value: &[u8]) {
        push_proto_varint(out, ((field as u64) << 3) | 2);
        push_proto_varint(out, value.len() as u64);
        out.extend_from_slice(value);
    }

    fn push_proto_varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
        push_proto_varint(out, (field as u64) << 3);
        push_proto_varint(out, value);
    }

    fn chat_room_member_record(username: &str, display_name: &str) -> Vec<u8> {
        let mut record = Vec::new();
        push_proto_len_field(&mut record, 1, username.as_bytes());
        push_proto_len_field(&mut record, 2, display_name.as_bytes());
        push_proto_varint_field(&mut record, 3, 1);
        record
    }

    fn chat_room_ext_buffer(members: &[(&str, &str)]) -> Vec<u8> {
        let mut data = Vec::new();
        for (username, display_name) in members {
            push_proto_len_field(
                &mut data,
                1,
                &chat_room_member_record(username, display_name),
            );
        }
        data
    }

    #[test]
    fn sender_name_modes_choose_expected_display_source() {
        let mut contacts = HashMap::new();
        contacts.insert(
            "tgid_sender".to_string(),
            Contact {
                username: "tgid_sender".to_string(),
                nick_name: "Public Nick".to_string(),
                remark: "Private Remark".to_string(),
                alias: "alias".to_string(),
                is_stranger: false,
            },
        );

        assert_eq!(
            resolve_sender_name_with_mode(
                "tgid_sender",
                &contacts,
                DisplayNameMode::PersonalRemark,
                &HashMap::new(),
            ),
            "Private Remark"
        );
        assert_eq!(
            resolve_sender_name_with_mode(
                "tgid_sender",
                &contacts,
                DisplayNameMode::Anonymous,
                &HashMap::new(),
            ),
            "Public Nick"
        );

        let room_names = HashMap::from([("tgid_sender".to_string(), "Room Card".to_string())]);
        assert_eq!(
            resolve_sender_name_with_mode(
                "tgid_sender",
                &contacts,
                DisplayNameMode::Anonymous,
                &room_names,
            ),
            "Room Card"
        );
    }

    #[test]
    fn loads_chat_room_member_names_for_room() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("contact.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE chat_room (username TEXT, ext_buffer BLOB)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chat_room (username, ext_buffer) VALUES (?1, ?2)",
            params![
                "room@chatroom",
                chat_room_ext_buffer(&[
                    ("tgid_alice", "Alice In Group"),
                    ("tgid_bob", ""),
                    ("tgid_cara", "Cara"),
                ]),
            ],
        )
        .unwrap();
        drop(conn);

        let names = load_chat_room_member_names(&path, "room@chatroom").unwrap();
        assert_eq!(
            names.get("tgid_alice").map(String::as_str),
            Some("Alice In Group")
        );
        assert_eq!(names.get("tgid_cara").map(String::as_str), Some("Cara"));
        assert!(!names.contains_key("tgid_bob"));
    }
}
