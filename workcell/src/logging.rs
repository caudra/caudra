use tracing::level_filters::LevelFilter;
use tracing_subscriber::{filter::Targets, fmt, layer::SubscriberExt, util::SubscriberInitExt};

const TARGET: &str = "workcell_mcp";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Format {
    Pretty,
    Json,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoggingConfiguration {
    level: &'static str,
    format: Format,
}

impl LoggingConfiguration {
    #[must_use]
    pub const fn level(self) -> &'static str {
        self.level
    }

    #[must_use]
    pub const fn format(self) -> &'static str {
        match self.format {
            Format::Pretty => "pretty",
            Format::Json => "json",
        }
    }
}

/// Install an application-only subscriber. Dependency wire traces remain off
/// because they can contain tool arguments, results, paths, URLs, and secrets.
pub fn initialize_with<F>(mut read: F) -> Result<LoggingConfiguration, &'static str>
where
    F: FnMut(&str) -> Option<String>,
{
    let (level, level_name) = configured_level(read("WORKCELL_LOG_LEVEL"));
    let format = if read("WORKCELL_LOG_FORMAT")
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("json"))
    {
        Format::Json
    } else {
        Format::Pretty
    };
    let filter = Targets::new()
        .with_default(LevelFilter::OFF)
        .with_target(TARGET, level);
    let result = match format {
        Format::Pretty => tracing_subscriber::registry()
            .with(filter)
            .with(
                fmt::layer()
                    .with_ansi(false)
                    .with_target(false)
                    .with_writer(std::io::stderr),
            )
            .try_init(),
        Format::Json => tracing_subscriber::registry()
            .with(filter)
            .with(
                fmt::layer()
                    .json()
                    .with_ansi(false)
                    .with_target(false)
                    .with_writer(std::io::stderr),
            )
            .try_init(),
    };
    result
        .map(|()| LoggingConfiguration {
            level: level_name,
            format,
        })
        .map_err(|_| "logging could not be initialized")
}

fn configured_level(value: Option<String>) -> (LevelFilter, &'static str) {
    match value
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("debug") => (LevelFilter::DEBUG, "debug"),
        Some("warn") => (LevelFilter::WARN, "warn"),
        Some("error") => (LevelFilter::ERROR, "error"),
        Some("silent") => (LevelFilter::OFF, "silent"),
        _ => (LevelFilter::INFO, "info"),
    }
}
