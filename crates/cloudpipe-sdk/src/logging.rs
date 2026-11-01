//! A minimal `tracing` subscriber for the SDK's own events.
//!
//! The SDK emits `debug!` / `warn!` / `error!` events, and the Cloudflare
//! client emits one `debug!` per retry. Without a subscriber those are
//! dropped on the floor — the crate would depend on `tracing` for nothing.
//!
//! This module is deliberately *not* `tracing-subscriber`: the CLI's
//! whole pitch is a single small binary, and the full subscriber is a
//! meaningful chunk of compile time and size for a tool that needs no
//! spans, no async-aware formatting and no per-layer filters.
//!
//! Spans are not tracked: nothing in the codebase creates any.
//!
//! Enabled by `CFP_LOG=<level>` or by the caller's `--verbose` flag:
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

    /// The setting for this process: `verbose` bumps the default to
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
    // `set_global_default` errors if a subscriber is already installed;
    // that only happens if `init` is called twice, which is harmless here.
    let _ = tracing::subscriber::set_global_default(StderrSubscriber { level });
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
        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        let mut out = std::io::stderr().lock();
        let _ = writeln!(out, "[{}] {}", meta.level(), meta.target());
        let _ = writeln!(out, "  {}", visitor.0.trim());
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// Renders an event's fields into a single line.
///
/// Every `Visit` method has to be implemented: `tracing::field::Visit`
/// provides no-op defaults for the ones that are missing, so a subscriber
/// that only handles `record_debug` silently drops every numeric and
/// boolean field — which is how `warn!(attempts = 5, …)` used to print
/// its message and nothing else.
struct MessageVisitor(String);

impl MessageVisitor {
    /// The rendered form of one field.
    ///
    /// The `message` field is the human-readable part of the event, so it
    /// goes out bare; every other field is rendered as `name = value`.
    ///
    /// The value arrives already rendered: `record_debug` supplies the
    /// `Debug` form (which quotes a string itself), `record_str` supplies
    /// the raw text, and the numeric visitors supply their decimal form.
    /// Nothing here adds quotes of its own.
    fn render_field(name: &str, value: &str) -> String {
        if name == "message" {
            value.to_string()
        } else {
            format!("{name} = {value}")
        }
    }

    /// Appends one rendered field to the line.
    fn push(&mut self, field: &tracing::field::Field, rendered: String) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        self.0
            .push_str(&MessageVisitor::render_field(field.name(), &rendered));
    }
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.push(field, format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.push(field, value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.push(field, value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.push(field, value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.push(field, value.to_string());
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.push(field, value.to_string());
    }

    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.push(field, value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

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

    #[test]
    fn the_message_field_is_rendered_bare() {
        assert_eq!(MessageVisitor::render_field("message", "boom"), "boom");
    }

    #[test]
    fn other_fields_are_rendered_with_their_name() {
        assert_eq!(
            MessageVisitor::render_field("attempts", "5"),
            "attempts = 5"
        );
        assert_eq!(
            MessageVisitor::render_field("err", "\"boom\""),
            "err = \"boom\""
        );
    }

    #[test]
    fn a_numeric_field_survives_the_visitor() {
        // The bug this module exists to fix: a visitor that only
        // implements `record_debug` prints the message and silently
        // discards every number attached to it.
        //
        // `tracing::field::Field` cannot be constructed by hand, so the
        // visitor is driven through a real event instead. The capture
        // owns its buffer because a subscriber has to be `'static`.
        struct Capture(Arc<Mutex<String>>);

        impl tracing::Subscriber for Capture {
            fn enabled(&self, _: &Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                let mut visitor = MessageVisitor(String::new());
                event.record(&mut visitor);
                *self.0.lock().expect("capture lock") = visitor.0;
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }

        let captured = Arc::new(Mutex::new(String::new()));
        let dispatcher = tracing::dispatcher::Dispatch::new(Capture(Arc::clone(&captured)));
        tracing::dispatcher::with_default(&dispatcher, || {
            tracing::warn!(attempts = 5, "boom");
        });

        let text = captured.lock().expect("capture lock").clone();
        assert!(text.contains("boom"), "{text}");
        assert!(text.contains("attempts = 5"), "{text}");
    }
}
