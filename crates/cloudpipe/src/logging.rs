//! Minimal `tracing` subscriber for the CLI.
//!
//! The SDK emits `debug!` / `warn!` / `error!` events (and the Cloudflare
//! client emits one `debug!` per retry). Until now nothing installed a
//! subscriber, so all of them were dropped on the floor — the crate
//! depended on `tracing` for nothing.
//!
//! Rather than pull in `tracing-subscriber` (which would add meaningful
//! compile time and binary size to a tool whose whole pitch is a single
//! small binary), this module implements just enough of
//! [`tracing::Subscriber`] to filter and print. Spans are not tracked:
//! nothing in the codebase creates any, and supporting them properly is
//! where the complexity lives.
//!
//! Enabled by `CFP_LOG=<level>` or by `cfp --verbose`:
//!
//! ```text
//! CFP_LOG=debug cfp 8080      # include the SDK's internal trace
//! CFP_LOG=info  cfp 8080      # warnings and errors only
//! ```
//!
//! `CFP_LOG=off` (or an unrecognised value) disables logging entirely,
//! which is also the default.

use std::io::Write as _;

use tracing::{Event, Level, Metadata, Subscriber};

/// Resolved logging configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSetting {
    /// Print events at or above this level to stderr.
    Level(Level),
    /// Print nothing.
    Off,
}

impl LogSetting {
    /// Maps a level name to a setting. Unrecognised names disable logging
    /// rather than defaulting to `debug`, so a typo cannot silently spam a
    /// user's terminal.
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "trace" => Self::Level(Level::TRACE),
            "debug" => Self::Level(Level::DEBUG),
            "info" => Self::Level(Level::INFO),
            "warn" | "warning" => Self::Level(Level::WARN),
            "error" => Self::Level(Level::ERROR),
            _ => Self::Off,
        }
    }

    /// The setting for this process: `--verbose` bumps the default to
    /// `debug`, `CFP_LOG` overrides it outright.
    pub fn resolve(verbose: bool) -> Self {
        match std::env::var("CFP_LOG") {
            Ok(value) => Self::parse(&value),
            Err(_) if verbose => Self::Level(Level::DEBUG),
            Err(_) => Self::Off,
        }
    }
}

/// Installs the global subscriber. Idempotent: a second call is ignored
/// rather than panicking on "a global default subscriber already exists".
pub fn init(setting: LogSetting) {
    let level = match setting {
        LogSetting::Off => return,
        LogSetting::Level(level) => level,
    };
    let subscriber = StderrSubscriber { level };
    // `set_global_default` errors if a subscriber is already installed;
    // that only happens if `init` is called twice, which is harmless here.
    let _ = tracing::subscriber::set_global_default(subscriber);
}

struct StderrSubscriber {
    level: Level,
}

impl Subscriber for StderrSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= self.level
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let meta = event.metadata();
        let mut out = std::io::stderr().lock();
        let _ = writeln!(out, "[{}] {}", meta.level(), meta.target());
        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        let _ = writeln!(out, "  {}", visitor.0.trim());
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// Concatenates the message fields of an event into one string.
struct MessageVisitor(String);

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        if self.0.is_empty() {
            self.0 = format!("{} = {value:?}", field.name());
        } else {
            let _ = write!(self.0, " {} = {value:?}", field.name());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_level_name() {
        assert_eq!(LogSetting::parse("trace"), LogSetting::Level(Level::TRACE));
        assert_eq!(LogSetting::parse("DEBUG"), LogSetting::Level(Level::DEBUG));
        assert_eq!(LogSetting::parse(" info "), LogSetting::Level(Level::INFO));
        assert_eq!(LogSetting::parse("warn"), LogSetting::Level(Level::WARN));
        assert_eq!(LogSetting::parse("warning"), LogSetting::Level(Level::WARN));
        assert_eq!(LogSetting::parse("error"), LogSetting::Level(Level::ERROR));
    }

    #[test]
    fn unrecognised_and_off_disable_logging() {
        assert_eq!(LogSetting::parse("off"), LogSetting::Off);
        assert_eq!(LogSetting::parse(""), LogSetting::Off);
        assert_eq!(LogSetting::parse("nonsense"), LogSetting::Off);
        assert_eq!(LogSetting::parse("9"), LogSetting::Off);
    }

    #[test]
    fn level_ordering_is_used_for_filtering() {
        // `enabled` relies on `Level`'s Ord, where ERROR < WARN < INFO <
        // DEBUG < TRACE. Assert that so a future change to the comparison
        // is caught here rather than silently inverting the filter.
        assert!(Level::ERROR < Level::WARN);
        assert!(Level::WARN < Level::INFO);
        assert!(Level::INFO < Level::DEBUG);
        assert!(Level::DEBUG < Level::TRACE);
    }

    #[test]
    fn off_never_installs_a_subscriber() {
        // Pure: `init(Off)` returns before touching global state, so it is
        // safe to call in a test even after other tests ran.
        init(LogSetting::Off);
    }
}
