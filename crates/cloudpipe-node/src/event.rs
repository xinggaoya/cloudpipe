//! Maps Rust SDK `cloudpipe_sdk::Event` values into JS-friendly
//! `(eventName, payload)` pairs. Each Event variant gets a stable
//! camelCase event name; the body is serialized as a JSON string so the
//! JS side can `JSON.parse` it inside an `EventEmitter` handler without
//! needing a custom marshaler per variant.
//!
//! Keeping the wire format as a JSON string is a deliberate trade-off:
//! it keeps the Rust side `Send` (the events arrive via a callback on a
//! non-napi thread, where no `napi::Env` is available), at the cost of
//! forcing JS callers to parse a string. The cost is small for the
//! handful of fields each event carries, and it makes the JS contract
//! stable across SDK event additions.

use cloudpipe_sdk::{CloudflareErrorKind, Event, LogLevel, Protocol, ShutdownReason};
use serde_json::{json, Value};

/// Convert an SDK [`Event`] into the `(eventName, payload)` pair we hand
/// to JS. The payload is a JSON-encoded string; `eventName` is
/// camelCase and stable across SDK versions.
pub fn event_to_payload(event: &Event) -> (&'static str, String) {
    let (name, value) = match event {
        Event::Banner => ("banner", json!({})),
        Event::ResolvingConflicts => ("resolvingConflicts", json!({})),
        Event::CreatingTunnel { name } => ("creatingTunnel", json!({ "name": name })),
        Event::IngressConfigured { protocol, port } => (
            "ingressConfigured",
            json!({ "protocol": protocol.as_str(), "port": port }),
        ),
        Event::DnsCreated { full_name } => ("dnsCreated", json!({ "fullName": full_name })),
        Event::CloudflaredStarted => ("cloudflaredStarted", json!({})),
        Event::Restarting { reason, attempt } => (
            "restarting",
            json!({ "reason": reason_to_value(reason), "attempt": attempt }),
        ),
        Event::Restarted { attempt } => ("restarted", json!({ "attempt": attempt })),
        Event::RestartGivingUp {
            attempts,
            last_error,
        } => (
            "restartGivingUp",
            json!({ "attempts": attempts, "lastError": last_error }),
        ),
        Event::EdgeConnected { conn_index, total } => (
            "edgeConnected",
            json!({ "connIndex": conn_index, "total": total }),
        ),
        Event::CloudflaredLog { level, line } => (
            "cloudflaredLog",
            json!({ "level": log_level_str(*level), "line": line }),
        ),
        Event::ShuttingDown { reason } => {
            ("shuttingDown", json!({ "reason": reason_to_value(reason) }))
        }
        Event::Cleaned => ("cleaned", json!({})),
        // Forward-compatibility for future event variants.
        other => {
            // Returns a generic event rather than panicking on an
            // unknown variant — JS callers should treat it as opaque.
            let _ = other;
            ("unknown", json!({}))
        }
    };
    (name, value.to_string())
}

fn log_level_str(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

fn reason_to_value(reason: &ShutdownReason) -> Value {
    match reason {
        ShutdownReason::UserRequested => json!({ "kind": "userRequested" }),
        ShutdownReason::ChildExited => json!({ "kind": "childExited" }),
        ShutdownReason::Error(err) => json!({ "kind": "error", "message": err }),
        // Stable shape for future variants: emit the Debug form.
        other => json!({ "kind": "other", "debug": format!("{other:?}") }),
    }
}

/// Reserved for future use — kept here to avoid an unused-import lint
/// in builds that haven't yet needed the helper.
#[allow(dead_code)]
fn _cf_kind_name(kind: CloudflareErrorKind) -> &'static str {
    match kind {
        CloudflareErrorKind::RateLimited => "rateLimited",
        CloudflareErrorKind::DnsExists => "dnsExists",
        CloudflareErrorKind::AuthFailed => "authFailed",
        CloudflareErrorKind::InvalidToken => "invalidToken",
        CloudflareErrorKind::Other(_) => "other",
    }
}

/// Reserved for future use — kept here to avoid an unused-import lint.
#[allow(dead_code)]
fn _protocol_str(p: Protocol) -> &'static str {
    p.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_maps_to_empty_payload() {
        let (name, payload) = event_to_payload(&Event::Banner);
        assert_eq!(name, "banner");
        assert_eq!(payload, "{}");
    }

    #[test]
    fn creating_tunnel_includes_name() {
        let event = Event::CreatingTunnel {
            name: "demo".into(),
        };
        let (name, payload) = event_to_payload(&event);
        assert_eq!(name, "creatingTunnel");
        assert_eq!(payload, "{\"name\":\"demo\"}");
    }

    #[test]
    fn ingress_configured_serializes_protocol_and_port() {
        let event = Event::IngressConfigured {
            protocol: Protocol::Http,
            port: 8080,
        };
        let (name, payload) = event_to_payload(&event);
        assert_eq!(name, "ingressConfigured");
        let value: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["protocol"], "http");
        assert_eq!(value["port"], 8080);
    }

    #[test]
    fn edge_connected_uses_camelcase_fields() {
        let event = Event::EdgeConnected {
            conn_index: 1,
            total: 4,
        };
        let (name, payload) = event_to_payload(&event);
        assert_eq!(name, "edgeConnected");
        let value: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["connIndex"], 1);
        assert_eq!(value["total"], 4);
    }

    #[test]
    fn cloudflared_log_includes_level_and_line() {
        let event = Event::CloudflaredLog {
            level: LogLevel::Error,
            line: "boom".into(),
        };
        let (name, payload) = event_to_payload(&event);
        assert_eq!(name, "cloudflaredLog");
        let value: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["level"], "error");
        assert_eq!(value["line"], "boom");
    }

    #[test]
    fn every_named_variant_is_covered() {
        // Smoke-test that each documented Event variant maps to a
        // distinct, non-empty camelCase event name. Adding a new
        // variant without updating this list should panic the test.
        let cases = [
            (Event::Banner, "banner"),
            (Event::ResolvingConflicts, "resolvingConflicts"),
            (Event::CloudflaredStarted, "cloudflaredStarted"),
            (Event::Restarted { attempt: 1 }, "restarted"),
            (Event::Cleaned, "cleaned"),
        ];
        for (event, expected) in cases {
            let (name, _) = event_to_payload(&event);
            assert_eq!(name, expected);
        }
    }
}
