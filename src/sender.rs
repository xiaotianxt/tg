//! Message senders: which accounts a sender filter selects, and how the
//! signed-in account is named in message listings.

use std::collections::{BTreeMap, HashMap, HashSet};

use rusqlite::{params_from_iter, Connection};

use crate::contact::{self, Contact, DisplayNameMode};
use crate::source_store::SourceStore;

/// Name of the signed-in account in every message listing, so its messages
/// read the same in private chats, groups, and structured queries.
pub(crate) const SELF_DISPLAY_NAME: &str = "我";

/// Account usernames a sender filter selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SenderFilter {
    usernames: Vec<String>,
}

impl SenderFilter {
    /// `Name2Id` rowids of the selected accounts in one message database;
    /// empty when none of them has a message there.
    pub(crate) fn rowids(&self, conn: &Connection, db_label: &str) -> Result<Vec<i64>, String> {
        let placeholders = vec!["?"; self.usernames.len()].join(", ");
        let sql = format!("SELECT rowid FROM Name2Id WHERE user_name IN ({placeholders})");
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|error| format!("Prepare sender lookup in {db_label}: {error}"))?;
        let rows = stmt
            .query_map(params_from_iter(&self.usernames), |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|error| format!("Sender lookup in {db_label}: {error}"))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| format!("Read sender lookup in {db_label}: {error}"))
    }
}

/// SQL condition selecting rows from `rowids`; `None` when no row can match.
pub(crate) fn sender_rowid_clause(column: &str, rowids: &[i64]) -> Option<String> {
    if rowids.is_empty() {
        return None;
    }
    let ids = rowids
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("{column} IN ({ids})"))
}

/// True when `sender_id` is the signed-in account.
pub(crate) fn is_self(sender_id: &str, account_username: Option<&str>) -> bool {
    !sender_id.is_empty() && account_username == Some(sender_id)
}

/// Names a group gives its members, used to match a sender in that group.
#[derive(Default)]
pub(crate) struct GroupNames {
    /// Group nickname per member username.
    pub cards: HashMap<String, String>,
    /// Current members, to prefer them when a name matches several accounts.
    pub members: HashSet<String>,
}

/// Group nicknames and members of `session`; empty for a private chat or a
/// group the contact database does not describe.
pub(crate) fn load_group_names(
    contact_conn: Option<&Connection>,
    session: &str,
) -> Result<GroupNames, String> {
    let Some(conn) = contact_conn.filter(|_| session.ends_with("@chatroom")) else {
        return Ok(GroupNames::default());
    };
    // A group can have messages without a chat_room row.
    let described: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM chat_room WHERE username = ?1)",
            [session],
            |row| row.get(0),
        )
        .map_err(|error| format!("Chat room query error: {error}"))?;
    if !described {
        return Ok(GroupNames::default());
    }
    Ok(GroupNames {
        cards: contact::load_chat_room_member_names(conn, session)?,
        members: contact::load_chat_room_roster(conn, session)?
            .usernames
            .into_iter()
            .collect(),
    })
}

/// Resolves a sender value: `me`, an account id, or a name shown for the
/// account. Names are matched exactly, ignoring case, against the names the
/// output can show in `name_mode`, so a remote caller cannot probe contact
/// remarks. A name that several accounts share is an error that lists them.
/// A value that matches no name is taken as an account id when some message
/// database knows that id.
pub(crate) fn resolve_sender(
    value: &str,
    store: &SourceStore,
    message_dbs: &[String],
    contacts: &HashMap<String, Contact>,
    group: &GroupNames,
    name_mode: DisplayNameMode,
) -> Result<SenderFilter, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("sender cannot be empty".to_string());
    }
    if value.eq_ignore_ascii_case("me") || value == SELF_DISPLAY_NAME {
        let account = store.account_username().ok_or_else(|| {
            "Cannot tell which account is yours from the source directory name; \
             pass your account id as sender instead"
                .to_string()
        })?;
        return Ok(SenderFilter {
            usernames: vec![account.to_string()],
        });
    }
    if contacts.contains_key(value) || group.cards.contains_key(value) {
        return Ok(SenderFilter {
            usernames: vec![value.to_string()],
        });
    }

    let matches = name_matches(value, contacts, group, name_mode);
    let in_group: BTreeMap<&String, &String> = matches
        .iter()
        .filter(|(username, _)| group.members.contains(username.as_str()))
        .map(|(username, name)| (*username, *name))
        .collect();
    let matches = if in_group.is_empty() {
        matches
    } else {
        in_group
    };
    match matches.len() {
        1 => {
            let username = matches.into_keys().next().expect("one match");
            return Ok(SenderFilter {
                usernames: vec![username.clone()],
            });
        }
        0 => {}
        _ => {
            let listed = matches
                .iter()
                .map(|(username, name)| format!("{name} ({username})"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "Sender '{value}' matches several accounts: {listed}; pass one account id"
            ));
        }
    }

    for db in message_dbs {
        let filter = SenderFilter {
            usernames: vec![value.to_string()],
        };
        if !filter.rowids(&store.open(db)?, db)?.is_empty() {
            return Ok(filter);
        }
    }
    Err(format!(
        "No sender matched '{value}'. Pass me, an account id, or a name exactly as listed"
    ))
}

/// Accounts whose visible names equal `value`, with the name that matched.
fn name_matches<'a>(
    value: &str,
    contacts: &'a HashMap<String, Contact>,
    group: &'a GroupNames,
    name_mode: DisplayNameMode,
) -> BTreeMap<&'a String, &'a String> {
    let wanted = value.to_lowercase();
    let same = |name: &str| !name.is_empty() && name.trim().to_lowercase() == wanted;
    let mut matches = BTreeMap::new();
    for (username, card) in &group.cards {
        if same(card) {
            matches.insert(username, card);
        }
    }
    for contact in contacts.values() {
        let remark = (name_mode == DisplayNameMode::PersonalRemark).then_some(&contact.remark);
        let matched = [Some(&contact.nick_name), Some(&contact.alias), remark]
            .into_iter()
            .flatten()
            .find(|name| same(name));
        if let Some(name) = matched {
            matches.entry(&contact.username).or_insert(name);
        }
    }
    matches
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(username: &str, nick_name: &str, remark: &str) -> (String, Contact) {
        (
            username.to_string(),
            Contact {
                username: username.to_string(),
                nick_name: nick_name.to_string(),
                remark: remark.to_string(),
                alias: String::new(),
                is_stranger: false,
            },
        )
    }

    fn store_with_name2id(names: &[&str]) -> (tempfile::TempDir, SourceStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("message_0.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE Name2Id(user_name TEXT PRIMARY KEY);")
            .unwrap();
        for name in names {
            conn.execute("INSERT INTO Name2Id(user_name) VALUES (?1)", [name])
                .unwrap();
        }
        drop(conn);
        let store = SourceStore::from_plaintext_paths([("message/message_0.db".to_string(), path)])
            .with_account_username("tgid_me");
        (dir, store)
    }

    fn resolve(
        value: &str,
        contacts: &HashMap<String, Contact>,
        group: &GroupNames,
        mode: DisplayNameMode,
    ) -> Result<SenderFilter, String> {
        let (_dir, store) = store_with_name2id(&["tgid_me", "tgid_left"]);
        resolve_sender(
            value,
            &store,
            &["message/message_0.db".to_string()],
            contacts,
            group,
            mode,
        )
    }

    fn selects(usernames: &[&str]) -> SenderFilter {
        SenderFilter {
            usernames: usernames.iter().map(|name| name.to_string()).collect(),
        }
    }

    #[test]
    fn resolves_me_ids_and_visible_names() {
        let contacts = HashMap::from([
            contact("tgid_bob", "Bob", "Robert"),
            contact("tgid_me", "wolf", ""),
        ]);
        let group = GroupNames {
            cards: HashMap::from([("tgid_carol".to_string(), "Carol".to_string())]),
            members: HashSet::new(),
        };
        let mode = DisplayNameMode::PersonalRemark;
        assert_eq!(
            resolve("me", &contacts, &group, mode),
            Ok(selects(&["tgid_me"]))
        );
        assert_eq!(
            resolve("tgid_bob", &contacts, &group, mode),
            Ok(selects(&["tgid_bob"]))
        );
        assert_eq!(
            resolve("robert", &contacts, &group, mode),
            Ok(selects(&["tgid_bob"]))
        );
        assert_eq!(
            resolve("Carol", &contacts, &group, mode),
            Ok(selects(&["tgid_carol"]))
        );
        // Known only from message history, as a member who left would be.
        assert_eq!(
            resolve("tgid_left", &contacts, &group, mode),
            Ok(selects(&["tgid_left"]))
        );
        assert!(resolve("Nobody", &contacts, &group, mode)
            .unwrap_err()
            .contains("No sender matched"));
    }

    #[test]
    fn anonymous_names_never_match_remarks() {
        let contacts = HashMap::from([contact("tgid_bob", "Bob", "Robert")]);
        let group = GroupNames::default();
        let mode = DisplayNameMode::Anonymous;
        assert_eq!(
            resolve("Bob", &contacts, &group, mode),
            Ok(selects(&["tgid_bob"]))
        );
        assert!(resolve("Robert", &contacts, &group, mode).is_err());
    }

    #[test]
    fn shared_names_prefer_group_members_then_fail() {
        let contacts = HashMap::from([contact("tgid_a", "Sam", ""), contact("tgid_b", "Sam", "")]);
        let outside = GroupNames::default();
        let error =
            resolve("Sam", &contacts, &outside, DisplayNameMode::PersonalRemark).unwrap_err();
        assert!(error.contains("Sam (tgid_a), Sam (tgid_b)"), "{error}");

        let group = GroupNames {
            cards: HashMap::new(),
            members: HashSet::from(["tgid_b".to_string()]),
        };
        assert_eq!(
            resolve("Sam", &contacts, &group, DisplayNameMode::PersonalRemark),
            Ok(selects(&["tgid_b"]))
        );
    }
}
