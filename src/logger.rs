use log::{LevelFilter, Log, Metadata, Record};
use std::io::Write;
use std::sync::Once;

static LOGGER: SimpleLogger = SimpleLogger;
static INIT: Once = Once::new();

pub fn init() {
    INIT.call_once(|| {
        let _ = log::set_logger(&LOGGER);
    });
    log::set_max_level(level_from_env());
}

struct SimpleLogger;

impl Log for SimpleLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let mut stderr = std::io::stderr().lock();
        let _ = match record.level() {
            log::Level::Error => writeln!(stderr, "error: {}", record.args()),
            log::Level::Warn => writeln!(stderr, "warning: {}", record.args()),
            log::Level::Info => writeln!(stderr, "{}", record.args()),
            log::Level::Debug | log::Level::Trace => {
                writeln!(
                    stderr,
                    "{} {}: {}",
                    record.level().as_str().to_ascii_lowercase(),
                    record.target(),
                    record.args()
                )
            }
        };
    }

    fn flush(&self) {}
}

fn level_from_env() -> LevelFilter {
    std::env::var("TG_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .as_deref()
        .and_then(parse_level_directive)
        .unwrap_or(LevelFilter::Info)
}

fn parse_level_directive(value: &str) -> Option<LevelFilter> {
    value.split(',').rev().find_map(|part| {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }

        let level = part
            .rsplit_once('=')
            .map_or(part, |(_, level)| level.trim());
        parse_level(level)
    })
}

fn parse_level(value: &str) -> Option<LevelFilter> {
    match value.to_ascii_lowercase().as_str() {
        "off" => Some(LevelFilter::Off),
        "error" => Some(LevelFilter::Error),
        "warn" | "warning" => Some(LevelFilter::Warn),
        "info" => Some(LevelFilter::Info),
        "debug" => Some(LevelFilter::Debug),
        "trace" => Some(LevelFilter::Trace),
        _ => None,
    }
}
