use chrono::{DateTime, NaiveDate, NaiveDateTime, Duration, Utc};

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
            return Ok(dt.and_utc().timestamp());
        }
    }

    // Date-only: assume start of day
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(dt) = d.and_hms_opt(0, 0, 0) {
            return Ok(dt.and_utc().timestamp());
        }
    }

    // Named expressions
    let now = Utc::now();
    match s {
        "today" => {
            let start = now.date_naive().and_hms_opt(0, 0, 0).unwrap();
            return Ok(start.and_utc().timestamp());
        }
        "yesterday" => {
            let yesterday = (now - Duration::try_days(1).unwrap_or(Duration::hours(24))).date_naive();
            let start = yesterday.and_hms_opt(0, 0, 0).unwrap();
            return Ok(start.and_utc().timestamp());
        }
        _ => {}
    }

    // Try "min" suffix (e.g. "5min")
    if let Some(num_str) = s.strip_suffix("min") {
        let minutes: i64 = num_str.parse()
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
