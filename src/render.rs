//! Human-readable CLI rendering of command results.
//!
//! Domain modules return typed results; this module owns the text layout. The
//! layout is a compatibility surface for scripts and the bundled skill, so
//! changes here are user-visible.

use std::io::Write;

use crate::db::{
    MemberFriendReport, MemberReport, MessagePage, ReadMessagesOptions, SearchResults, SessionList,
    SessionListKind,
};
use crate::doctor::{CheckStatus, DoctorReport, SessionDiagnosis};
use crate::output::Output;
use crate::time;

pub(crate) fn sessions<W: Write>(out: &mut Output<W>, list: &SessionList) -> Result<(), String> {
    if list.matched == 0 {
        return Ok(());
    }
    out.line(format_args!(
        "{:<4} {:<8} {:<20} {:<22} Username",
        "Rank", "Unread", "Last Activity", "Display Name"
    ))?;
    out.line(format_args!("{}", "-".repeat(120)))?;
    for (index, session) in list.sessions.iter().enumerate() {
        out.line(format_args!(
            "{:<4} {:<8} {:<20} {:<22} {}",
            index + 1,
            session.unread_count,
            time::format_local_timestamp_minutes(session.last_active),
            session.display_name,
            session.username
        ))?;
    }
    out.blank_line()?;
    let label = match list.kind {
        SessionListKind::All => "sessions",
        SessionListKind::Unread => "unread sessions",
    };
    if list.filtered {
        out.line(format_args!(
            "Matched: {} of {} {label}",
            list.matched, list.total
        ))?;
    } else {
        out.line(format_args!("Total: {} {label}", list.total))?;
    }
    out.flush()
}

pub(crate) fn messages<W: Write>(
    out: &mut Output<W>,
    page: &MessagePage,
    options: &ReadMessagesOptions<'_>,
) -> Result<(), String> {
    let messages = &page.messages;
    if messages.is_empty() {
        return Ok(());
    }
    out.line(format_args!(
        "Chat with: {} ({})",
        page.display_name, page.username
    ))?;
    if let Some(query) = options.search_query {
        out.line(format_args!("Search: '{query}'"))?;
    }
    if options.tail {
        if options.limit.is_some() {
            out.line(format_args!("Showing latest {} messages", messages.len()))?;
        } else {
            out.line(format_args!("Showing {} messages", messages.len()))?;
        }
    } else if options.limit.is_some() || options.offset > 0 {
        out.line(format_args!(
            "Showing messages {}-{}",
            options.offset + 1,
            options.offset + messages.len()
        ))?;
    } else {
        out.line(format_args!("Showing {} messages", messages.len()))?;
    }
    out.blank_line()?;

    let bucket = options.time_bucket;
    let mut last_time_key = None;
    let mut last_sender: Option<&str> = None;
    for message in messages {
        if bucket == time::MessageTimeBucket::PerMessage {
            out.line(format_args!(
                "[{}] {}: {}",
                time::format_local_timestamp(message.create_time),
                message.sender,
                message.content
            ))?;
            continue;
        }
        if let Some(time_key) = time::message_time_key(message.create_time, bucket) {
            if last_time_key != Some(time_key) {
                out.line(format_args!(
                    "[{}]",
                    time::format_message_time_bucket(message.create_time, bucket)
                ))?;
                last_time_key = Some(time_key);
                last_sender = None;
            }
        }
        if last_sender == Some(message.sender.as_str()) {
            out.line(format_args!(" {}", message.content))?;
        } else {
            out.line(format_args!("{}: {}", message.sender, message.content))?;
            last_sender = Some(&message.sender);
        }
    }

    out.blank_line()?;
    out.line(format_args!("--- End of messages ---"))?;
    out.flush()
}

const SEARCH_PREVIEW_CHARS: usize = 100;

pub(crate) fn search<W: Write>(
    out: &mut Output<W>,
    query: &str,
    results: &SearchResults,
) -> Result<(), String> {
    let hits = &results.hits;
    if hits.is_empty() {
        return Ok(());
    }
    if results.has_more {
        out.line(format_args!(
            "Search results for '{}': showing latest {} matches (more available)",
            query,
            hits.len()
        ))?;
    } else {
        out.line(format_args!(
            "Search results for '{}': {} matches",
            query,
            hits.len()
        ))?;
    }
    out.blank_line()?;
    for (index, hit) in hits.iter().enumerate() {
        out.line(format_args!(
            "[{}] {} | {}: {}",
            index + 1,
            time::format_local_timestamp(hit.create_time),
            hit.session_display,
            truncate_preview(&hit.content, SEARCH_PREVIEW_CHARS)
        ))?;
    }
    if results.has_more {
        out.line(format_args!("... more older results available"))?;
    }
    out.flush()
}

pub(crate) fn members<W: Write>(out: &mut Output<W>, report: &MemberReport) -> Result<(), String> {
    out.line(format_args!(
        "Group: {} ({})",
        report.group_display_name, report.group_username
    ))?;
    out.line(format_args!("Current members: {}", report.total_members))?;
    match (report.history_start, report.history_end) {
        (Some(start), Some(end)) => out.line(format_args!(
            "Local sender history: {} to {}",
            time::format_local_timestamp_minutes(start),
            time::format_local_timestamp_minutes(end)
        ))?,
        _ => out.line(format_args!("Local sender history: none"))?,
    }
    if let Some(cutoff) = report.inactive_since {
        out.line(format_args!(
            "Filter: no recorded messages since {}",
            time::format_local_timestamp_minutes(cutoff)
        ))?;
        out.line(format_args!(
            "Note: the current roster has no join times; review recent joiners before removal."
        ))?;
        if report.history_start.is_none_or(|start| start > cutoff) {
            out.line(format_args!(
                "Warning: local sender history does not reach the cutoff; this cannot prove a full period of silence."
            ))?;
        }
    }
    out.blank_line()?;
    out.line(format_args!(
        "{:<4} {:<20} {:<10} {:<7} {:<22} Username",
        "Rank", "Last Spoke", "Messages", "Role", "Display Name"
    ))?;
    out.line(format_args!("{}", "-".repeat(130)))?;
    for (index, member) in report.members.iter().enumerate() {
        let last_spoke = member
            .last_spoke
            .map(time::format_local_timestamp_minutes)
            .unwrap_or_else(|| "never recorded".to_string());
        out.line(format_args!(
            "{:<4} {:<20} {:<10} {:<7} {:<22} {}",
            index + 1,
            last_spoke,
            member.message_count,
            if member.is_owner { "owner" } else { "" },
            member.display_name,
            member.username
        ))?;
    }
    out.blank_line()?;
    if report.inactive_since.is_some() {
        out.line(format_args!(
            "Candidates: {} of {} current members (local records only)",
            report.members.len(),
            report.total_members
        ))?;
    } else {
        out.line(format_args!(
            "Listed: {} current members",
            report.members.len()
        ))?;
    }
    out.flush()
}

pub(crate) fn member_friends<W: Write>(
    out: &mut Output<W>,
    report: &MemberFriendReport,
) -> Result<(), String> {
    out.line(format_args!(
        "Group: {} ({})",
        report.group_display_name, report.group_username
    ))?;
    out.line(format_args!("Cached members: {}", report.cached_members))?;
    out.line(format_args!(
        "Locally confirmed friends: {}",
        report.friends.len()
    ))?;
    out.line(format_args!("Note: conservative local relationship evidence only; not a live or reciprocal friendship check."))?;
    out.blank_line()?;
    out.line(format_args!("{:<24} Username", "Display Name"))?;
    for friend in &report.friends {
        out.line(format_args!(
            "{:<24} {}",
            friend.display_name.escape_debug().to_string(),
            friend.username.escape_debug()
        ))?;
    }
    out.flush()
}

pub(crate) fn doctor<W: Write>(out: &mut Output<W>, report: &DoctorReport) -> Result<(), String> {
    out.line(format_args!("tg doctor"))?;
    out.blank_line()?;
    for check in &report.checks {
        let label = check.label;
        let status = check.status.map(|status| match status {
            CheckStatus::Ok => "OK",
            CheckStatus::Missing => "MISSING",
            CheckStatus::Error => "ERROR",
        });
        match (status, check.detail.as_deref()) {
            (Some(status), Some(detail)) => {
                out.line(format_args!("{label}: {status} ({detail})"))?
            }
            (Some(status), None) => out.line(format_args!("{label}: {status}"))?,
            (None, Some(detail)) => out.line(format_args!("{label}: {detail}"))?,
            (None, None) => out.line(format_args!("{label}"))?,
        }
    }
    if let Some(session) = &report.session {
        out.blank_line()?;
        out.line(format_args!("session query: {}", session.query))?;
        match &session.diagnosis {
            SessionDiagnosis::Resolved(probe) => {
                out.line(format_args!(
                    "resolved session: {} ({})",
                    probe.display_name, probe.username
                ))?;
                out.line(format_args!("message table: {}", probe.table_name))?;
                out.line(format_args!(
                    "message rows: {} across {} database(s)",
                    probe.message_count, probe.matching_dbs
                ))?;
            }
            SessionDiagnosis::Failed { error } => {
                out.line(format_args!("session probe: ERROR ({error})"))?
            }
            SessionDiagnosis::Unavailable => {
                out.line(format_args!("session probe: unavailable"))?
            }
        }
    }
    out.flush()
}

fn truncate_preview(content: &str, max_chars: usize) -> String {
    let mut chars = content.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        content.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contact::DisplayNameMode;
    use crate::db::{ChatMessage, SessionSummary};
    use crate::doctor::DoctorCheck;

    fn render(write: impl FnOnce(&mut Output<&mut Vec<u8>>) -> Result<(), String>) -> String {
        let mut bytes = Vec::new();
        write(&mut Output::new(&mut bytes)).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn truncate_preview_handles_multibyte_content() {
        assert_eq!(truncate_preview("提取一下", 2), "提取...");
    }

    #[test]
    fn sessions_print_nothing_without_matches_and_label_filtered_totals() {
        let mut list = SessionList {
            kind: SessionListKind::Unread,
            filtered: true,
            matched: 0,
            total: 5,
            sessions: Vec::new(),
        };
        assert_eq!(render(|out| sessions(out, &list)), "");

        list.matched = 1;
        list.sessions.push(SessionSummary {
            username: "tgid_a".to_string(),
            display_name: "Alice".to_string(),
            unread_count: 2,
            last_active: 1_700_000_000,
        });
        let text = render(|out| sessions(out, &list));
        let row = format!(
            "{:<4} {:<8} {:<20} {:<22} {}",
            1,
            2,
            time::format_local_timestamp_minutes(1_700_000_000),
            "Alice",
            "tgid_a"
        );
        assert!(text.contains(&row), "{text}");
        assert!(
            text.ends_with("\nMatched: 1 of 5 unread sessions\n"),
            "{text}"
        );
    }

    #[test]
    fn messages_collapse_consecutive_senders_within_a_time_bucket() {
        let message = |sender: &str, content: &str| ChatMessage {
            local_id: None,
            local_type: 1,
            create_time: 1_700_000_000,
            sender: sender.to_string(),
            content: content.to_string(),
        };
        let page = MessagePage {
            username: "tgid_a".to_string(),
            display_name: "Alice".to_string(),
            messages: vec![
                message("Alice", "one"),
                message("Alice", "two"),
                message("我", "three"),
            ],
        };
        let options = ReadMessagesOptions {
            session_query: "Alice",
            limit: Some(3),
            offset: 0,
            search_query: None,
            since: None,
            tail: true,
            time_bucket: time::MessageTimeBucket::Minute(1),
            name_mode: DisplayNameMode::PersonalRemark,
            jobs: 1,
        };
        let text = render(|out| messages(out, &page, &options));
        let bucket = time::format_message_time_bucket(1_700_000_000, options.time_bucket);
        assert_eq!(
            text,
            format!(
                "Chat with: Alice (tgid_a)\nShowing latest 3 messages\n\n[{bucket}]\nAlice: one\n two\n我: three\n\n--- End of messages ---\n"
            )
        );
    }

    #[test]
    fn doctor_formats_status_and_fact_lines() {
        let report = DoctorReport {
            checks: vec![
                DoctorCheck {
                    label: "keys",
                    status: Some(CheckStatus::Missing),
                    detail: Some("/k.json".to_string()),
                },
                DoctorCheck {
                    label: "direct source",
                    status: Some(CheckStatus::Ok),
                    detail: None,
                },
                DoctorCheck {
                    label: "live message WAL files",
                    status: None,
                    detail: Some("3".to_string()),
                },
            ],
            session: None,
        };
        assert_eq!(
            render(|out| doctor(out, &report)),
            "tg doctor\n\nkeys: MISSING (/k.json)\ndirect source: OK\nlive message WAL files: 3\n"
        );
    }
}
