use chrono::{DateTime, Datelike, Local, LocalResult, NaiveDate, NaiveDateTime, TimeZone};

const DATETIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";
const MINUTE_FORMAT: &str = "%Y-%m-%d %H:%M";
const HOUR_FORMAT: &str = "%Y-%m-%d %H:00";
const DATE_FORMAT: &str = "%Y-%m-%d";
const MONTH_FORMAT: &str = "%Y-%m";
const YEAR_FORMAT: &str = "%Y";
pub(crate) const DEFAULT_RECENT_DAYS: i64 = 365;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MessageTimeBucket {
    PerMessage,
    None,
    Minute(i64),
    Hour(i64),
    Day,
    Month,
    Year,
}

/// Parse a time expression into a Unix timestamp.
///
/// Accepts ISO 8601 formats (e.g. "2024-01-01T00:00:00", "2024-01-01 00:00:00")
/// and relative expressions (e.g. "5min", "1h", "30s", "2d", "1w", "1y", "today").
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

    // Try single-char suffixes: s, h, d, w, y
    if s.len() >= 2 {
        let (num_part, unit) = s.split_at(s.len() - 1);
        if let Ok(num) = num_part.parse::<i64>() {
            return match unit {
                "s" => Ok(now.timestamp() - num),
                "h" => Ok(now.timestamp() - num * 3600),
                "d" => Ok(now.timestamp() - num * 86400),
                "w" => Ok(now.timestamp() - num * 604800),
                "y" => Ok(now.timestamp() - num * 365 * 86400),
                _ => Err(format!("Unknown time unit '{}' in '{}'", unit, s)),
            };
        }
    }

    Err(format!(
        "Cannot parse time expression '{}'. Use ISO 8601 (e.g. '2024-01-01T00:00:00') \
         or relative (e.g. '5min', '1h', '7d', '1y', 'today').",
        s
    ))
}

/// Parse an optional time expression string into a Unix timestamp.
pub fn parse_since_opt(since: Option<&str>) -> Result<Option<i64>, String> {
    since.map(parse_relative_time).transpose()
}

pub(crate) fn default_recent_since() -> i64 {
    now_timestamp() - DEFAULT_RECENT_DAYS * 86400
}

pub(crate) fn now_timestamp() -> i64 {
    Local::now().timestamp()
}

pub(crate) fn parse_message_time_bucket(s: &str) -> Result<MessageTimeBucket, String> {
    let value = s.trim().to_ascii_lowercase();
    match value.as_str() {
        "full" | "always" => return Ok(MessageTimeBucket::PerMessage),
        "none" | "off" => return Ok(MessageTimeBucket::None),
        _ => {}
    }

    let split_at = value
        .find(|ch: char| !ch.is_ascii_digit())
        .ok_or_else(|| format!("Missing time unit in '{}'", s))?;
    let (num_str, unit) = value.split_at(split_at);
    let num: i64 = num_str
        .parse()
        .map_err(|_| format!("Invalid number in '{}'", s))?;
    if num <= 0 {
        return Err(format!("Time bucket must be positive in '{}'", s));
    }

    match unit {
        "m" | "min" | "mins" | "minute" | "minutes" => Ok(MessageTimeBucket::Minute(num)),
        "h" | "hour" | "hours" => Ok(MessageTimeBucket::Hour(num)),
        "d" | "day" | "days" if num == 1 => Ok(MessageTimeBucket::Day),
        "mo" | "mon" | "month" | "months" if num == 1 => Ok(MessageTimeBucket::Month),
        "y" | "year" | "years" if num == 1 => Ok(MessageTimeBucket::Year),
        "d" | "day" | "days" | "mo" | "mon" | "month" | "months" | "y" | "year" | "years" => {
            Err(format!("Only 1{} is supported for calendar buckets.", unit))
        }
        _ => Err(format!(
            "Unknown time bucket '{}'. Use 1m, 1min, 1h, 1d, 1mo, 1y, full, or none.",
            s
        )),
    }
}

pub fn format_local_timestamp(timestamp: i64) -> String {
    format_local_timestamp_with(timestamp, DATETIME_FORMAT)
}

pub fn format_local_timestamp_minutes(timestamp: i64) -> String {
    format_local_timestamp_with(timestamp, MINUTE_FORMAT)
}

pub(crate) fn parse_local_timestamp_minutes(s: &str) -> Option<i64> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M")
        .or_else(|_| NaiveDateTime::parse_from_str(s, DATETIME_FORMAT))
        .ok()
        .and_then(|dt| local_timestamp(dt).ok())
}

fn format_local_timestamp_with(timestamp: i64, format: &str) -> String {
    DateTime::from_timestamp(timestamp, 0)
        .map(|t| t.with_timezone(&Local).format(format).to_string())
        .unwrap_or_else(|| timestamp.to_string())
}

pub(crate) fn message_time_key(timestamp: i64, bucket: MessageTimeBucket) -> Option<i64> {
    match bucket {
        MessageTimeBucket::PerMessage => Some(timestamp),
        MessageTimeBucket::None => None,
        MessageTimeBucket::Minute(minutes) => Some(timestamp.div_euclid(minutes * 60)),
        MessageTimeBucket::Hour(hours) => Some(timestamp.div_euclid(hours * 3600)),
        MessageTimeBucket::Day => DateTime::from_timestamp(timestamp, 0).map(|dt| {
            let local = dt.with_timezone(&Local);
            local.year() as i64 * 400 + local.ordinal() as i64
        }),
        MessageTimeBucket::Month => DateTime::from_timestamp(timestamp, 0).map(|dt| {
            let local = dt.with_timezone(&Local);
            local.year() as i64 * 12 + local.month0() as i64
        }),
        MessageTimeBucket::Year => {
            DateTime::from_timestamp(timestamp, 0).map(|dt| dt.with_timezone(&Local).year() as i64)
        }
    }
}

pub(crate) fn format_message_time_bucket(timestamp: i64, bucket: MessageTimeBucket) -> String {
    match bucket {
        MessageTimeBucket::PerMessage => format_local_timestamp(timestamp),
        MessageTimeBucket::None => String::new(),
        MessageTimeBucket::Minute(minutes) => format_local_timestamp_with(
            timestamp.div_euclid(minutes * 60) * minutes * 60,
            MINUTE_FORMAT,
        ),
        MessageTimeBucket::Hour(hours) => format_local_timestamp_with(
            timestamp.div_euclid(hours * 3600) * hours * 3600,
            HOUR_FORMAT,
        ),
        MessageTimeBucket::Day => format_local_timestamp_with(timestamp, DATE_FORMAT),
        MessageTimeBucket::Month => format_local_timestamp_with(timestamp, MONTH_FORMAT),
        MessageTimeBucket::Year => format_local_timestamp_with(timestamp, YEAR_FORMAT),
    }
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

    #[test]
    fn parses_year_relative_time() {
        let before = Local::now().timestamp() - 365 * 86400;
        let parsed = parse_relative_time("1y").unwrap();
        let after = Local::now().timestamp() - 365 * 86400;

        assert!(parsed >= before && parsed <= after);
    }

    #[test]
    fn parses_message_time_buckets() {
        assert_eq!(
            parse_message_time_bucket("1m"),
            Ok(MessageTimeBucket::Minute(1))
        );
        assert_eq!(
            parse_message_time_bucket("1min"),
            Ok(MessageTimeBucket::Minute(1))
        );
        assert_eq!(
            parse_message_time_bucket("2h"),
            Ok(MessageTimeBucket::Hour(2))
        );
        assert_eq!(parse_message_time_bucket("1d"), Ok(MessageTimeBucket::Day));
        assert_eq!(
            parse_message_time_bucket("1mo"),
            Ok(MessageTimeBucket::Month)
        );
        assert_eq!(parse_message_time_bucket("1y"), Ok(MessageTimeBucket::Year));
        assert_eq!(
            parse_message_time_bucket("full"),
            Ok(MessageTimeBucket::PerMessage)
        );
        assert_eq!(
            parse_message_time_bucket("none"),
            Ok(MessageTimeBucket::None)
        );
        assert!(parse_message_time_bucket("2d").is_err());
    }

    #[test]
    fn message_time_key_coalesces_minutes_without_formatting() {
        let bucket = MessageTimeBucket::Minute(1);
        let timestamp = 1_775_000_000;

        assert_eq!(
            message_time_key(timestamp, bucket),
            message_time_key(timestamp + 30, bucket)
        );
        assert_ne!(
            message_time_key(timestamp, bucket),
            message_time_key(timestamp + 60, bucket)
        );
    }
}
