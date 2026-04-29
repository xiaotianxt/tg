use chrono::{DateTime, Local, LocalResult, NaiveDate, NaiveDateTime, TimeZone};

const DATETIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";
const MINUTE_FORMAT: &str = "%Y-%m-%d %H:%M";

/// Parse a time expression into a Unix timestamp.
///
/// Accepts ISO 8601 formats (e.g. "2024-01-01T00:00:00", "2024-01-01 00:00:00")
/// and relative expressions (e.g. "5min", "1h", "30s", "2d", "1w", "today").
pub fn parse_relative_time(s: &str) -> Result<i64, String> {
    // Try ISO 8601 / RFC 3339
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp());
    }

    // Try common date-time formats
    for fmt in &["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return local_timestamp(dt);
        }
    }

    // Date-only: assume start of day
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(dt) = d.and_hms_opt(0, 0, 0) {
            return local_timestamp(dt);
        }
    }

    // Named expressions
    let now = Local::now();
    match s {
        "today" => {
            let start = now.date_naive().and_hms_opt(0, 0, 0).unwrap();
            return local_timestamp(start);
        }
        "yesterday" => {
            let yesterday = now
                .date_naive()
                .pred_opt()
                .unwrap_or_else(|| now.date_naive());
            let start = yesterday.and_hms_opt(0, 0, 0).unwrap();
            return local_timestamp(start);
        }
        _ => {}
    }

    // Try "min" suffix (e.g. "5min")
    if let Some(num_str) = s.strip_suffix("min") {
        let minutes: i64 = num_str
            .parse()
            .map_err(|_| format!("Invalid number in '{}'", s))?;
        return Ok(now.timestamp() - minutes * 60);
    }

    // Try single-char suffixes: s, h, d, w
    if s.len() >= 2 {
        let (num_part, unit) = s.split_at(s.len() - 1);
        if let Ok(num) = num_part.parse::<i64>() {
            return match unit {
                "s" => Ok(now.timestamp() - num),
                "h" => Ok(now.timestamp() - num * 3600),
                "d" => Ok(now.timestamp() - num * 86400),
                "w" => Ok(now.timestamp() - num * 604800),
                _ => Err(format!("Unknown time unit '{}' in '{}'", unit, s)),
            };
        }
    }

    Err(format!(
        "Cannot parse time expression '{}'. Use ISO 8601 (e.g. '2024-01-01T00:00:00') \
         or relative (e.g. '5min', '1h', 'today').",
        s
    ))
}

/// Parse an optional time expression string into a Unix timestamp.
pub fn parse_since_opt(since: Option<&str>) -> Result<Option<i64>, String> {
    since.map(parse_relative_time).transpose()
}

pub fn format_local_timestamp(timestamp: i64) -> String {
    format_local_timestamp_with(timestamp, DATETIME_FORMAT)
}

pub fn format_local_timestamp_minutes(timestamp: i64) -> String {
    format_local_timestamp_with(timestamp, MINUTE_FORMAT)
}

fn format_local_timestamp_with(timestamp: i64, format: &str) -> String {
    DateTime::from_timestamp(timestamp, 0)
        .map(|t| t.with_timezone(&Local).format(format).to_string())
        .unwrap_or_else(|| timestamp.to_string())
}

fn local_timestamp(dt: NaiveDateTime) -> Result<i64, String> {
    match Local.from_local_datetime(&dt) {
        LocalResult::Single(dt) => Ok(dt.timestamp()),
        LocalResult::Ambiguous(earliest, _) => Ok(earliest.timestamp()),
        LocalResult::None => Err(format!(
            "Local time '{}' does not exist in the system time zone.",
            dt.format(DATETIME_FORMAT)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_only_uses_local_midnight() {
        let parsed = parse_relative_time("2026-04-28").unwrap();
        let local_midnight = Local
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(2026, 4, 28)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
            )
            .earliest()
            .unwrap()
            .timestamp();

        assert_eq!(parsed, local_midnight);
    }

    #[test]
    fn formats_timestamps_in_local_time() {
        let timestamp = 1_775_000_000;
        let expected = DateTime::from_timestamp(timestamp, 0)
            .unwrap()
            .with_timezone(&Local)
            .format(DATETIME_FORMAT)
            .to_string();

        assert_eq!(format_local_timestamp(timestamp), expected);
    }
}
