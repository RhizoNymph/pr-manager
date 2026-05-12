use tracing::Level;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::types::LogLevel;

/// Tracing subscriber that emits structured JSON to stderr.
///
/// Pino's "fatal" level has no equivalent in tracing's level set; we collapse
/// it to `error`. Every record carries a static `svc=pr-manager` field via a
/// span entered at init time -- consumers that filter or pivot on the service
/// name see the same shape as the TS implementation.
pub fn init_logger(level: LogLevel) {
    let level_filter = match level {
        LogLevel::Trace => LevelFilter::from_level(Level::TRACE),
        LogLevel::Debug => LevelFilter::from_level(Level::DEBUG),
        LogLevel::Info => LevelFilter::from_level(Level::INFO),
        LogLevel::Warn => LevelFilter::from_level(Level::WARN),
        LogLevel::Error | LogLevel::Fatal => LevelFilter::from_level(Level::ERROR),
    };

    let json_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .json()
        .flatten_event(true);

    tracing_subscriber::registry()
        .with(level_filter)
        .with(json_layer)
        .init();
}
