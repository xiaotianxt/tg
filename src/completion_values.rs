use clap::builder::PossibleValue;
use clap::builder::PossibleValuesParser;

pub(crate) fn time_buckets() -> PossibleValuesParser {
    parser(&[
        ("1m", "One-minute buckets"),
        ("1min", "One-minute buckets"),
        ("1h", "Hourly buckets"),
        ("1d", "Daily buckets"),
        ("1mo", "Monthly buckets"),
        ("1y", "Yearly buckets"),
        ("full", "Full timestamp"),
        ("none", "No timestamp grouping"),
    ])
}

pub(crate) fn query_formats() -> PossibleValuesParser {
    parser(&[("table", "Table output"), ("json", "JSON output")])
}

pub(crate) fn voice_formats() -> PossibleValuesParser {
    parser(&[
        ("native", "Native voice output"),
        ("wav", "WAV output"),
        ("pcm", "Raw PCM output"),
    ])
}

pub(crate) fn orders() -> PossibleValuesParser {
    parser(&[
        ("newest", "Newest messages first"),
        ("oldest", "Oldest messages first"),
    ])
}

pub(crate) fn match_modes() -> PossibleValuesParser {
    parser(&[
        ("all", "Require every keyword"),
        ("any", "Match any keyword"),
    ])
}

pub(crate) fn media_types() -> PossibleValuesParser {
    parser(&[
        ("voice", "Voice messages"),
        ("image", "Image messages"),
        ("sticker", "Sticker messages"),
        ("file", "File attachments"),
        ("video", "Video messages"),
    ])
}

pub(crate) fn db_targets() -> PossibleValuesParser {
    parser(&[
        ("messages", "Message cache"),
        ("contact", "Contact cache"),
        ("fts", "Message index"),
    ])
}

fn parser(values: &[(&'static str, &'static str)]) -> PossibleValuesParser {
    PossibleValuesParser::new(
        values
            .iter()
            .map(|(value, description)| PossibleValue::new(*value).help(*description)),
    )
}
