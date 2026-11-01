//! Maps Rust SDK [`cloudpipe_sdk::Event`] values into JS-friendly
//! `(eventName, payload)` pairs. Each `Event` variant gets a stable
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

use cloudpipe_sdk::{Event, LogLevel, Protocol, ShutdownReason};
use serde_json::{json, Value};

/// Converts an SDK [`Event`] into the `(eventName, payload)` pair we hand
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
        Event::BootstrapFailed { attempt, error } => (
            "bootstrapFailed",
            json!({ "attempt": attempt, "error": error }),
        ),
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
        _ => ("unknown", json!({})),
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

/// Every event name [`event_to_payload`] can return, in the same order the
/// `index.d.ts` `ListenerEventName` union lists them. Used by the binding
/// to validate handler registrations.
pub const EVENT_NAMES: &[&str] = &[
    "banner",
    "resolvingConflicts",
    "creatingTunnel",
    "ingressConfigured",
    "dnsCreated",
    "cloudflaredStarted",
    "restarting",
    "restarted",
    "restartGivingUp",
    "bootstrapFailed",
    "edgeConnected",
    "cloudflaredLog",
    "shuttingDown",
    "cleaned",
];

/// A queue of events that were produced before any listener existed.
///
/// `cloudflared`'s startup events fire while the tunnel is being created,
/// which is strictly before a JS caller can register a handler — the
/// `Listener` object does not exist yet. Dropping them made the
/// `dnsCreated` / `cloudflaredStarted` / `edgeConnected` events
/// unreachable from JavaScript.
#[derive(Debug, Default)]
pub struct PendingEvents {
    items: std::collections::VecDeque<(String, String)>,
}

impl PendingEvents {
    /// How many events are held before the oldest is dropped.
    pub const CAPACITY: usize = 512;

    /// Records an event that had no listener.
    pub fn push(&mut self, name: String, payload: String) {
        if self.items.len() >= Self::CAPACITY {
            self.items.pop_front();
        }
        self.items.push_back((name, payload));
    }

    /// Removes and returns every held event with the given name, in the
    /// order it was produced.
    pub fn drain_named(&mut self, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut kept = std::collections::VecDeque::new();
        while let Some((n, payload)) = self.items.pop_front() {
            if n == name {
                out.push(payload);
            } else {
                kept.push_back((n, payload));
            }
        }
        self.items = kept;
        out
    }

    /// Drops every held event with the given name.
    pub fn remove_named(&mut self, name: &str) {
        self.items.retain(|(n, _)| n != name);
    }

    /// How many events are currently held.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Convenience accessor so the binding crate can name a protocol without
/// importing `cloudpipe_sdk` directly.
pub fn protocol_name(protocol: Protocol) -> &'static str {
    protocol.as_str()
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
    fn dns_created_uses_camelcase_fields() {
        let event = Event::DnsCreated {
            full_name: "a.example.com".into(),
        };
        let (name, payload) = event_to_payload(&event);
        assert_eq!(name, "dnsCreated");
        let value: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["fullName"], "a.example.com");
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
    fn restarting_carries_structured_reason() {
        let event = Event::Restarting {
            reason: ShutdownReason::Error("boom".into()),
            attempt: 3,
        };
        let (name, payload) = event_to_payload(&event);
        assert_eq!(name, "restarting");
        let value: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["attempt"], 3);
        assert_eq!(value["reason"]["kind"], "error");
        assert_eq!(value["reason"]["message"], "boom");
    }

    #[test]
    fn every_documented_event_name_is_reachable() {
        let cases = [
            (Event::Banner, "banner"),
            (Event::ResolvingConflicts, "resolvingConflicts"),
            (Event::CreatingTunnel { name: "n".into() }, "creatingTunnel"),
            (
                Event::IngressConfigured {
                    protocol: Protocol::Tcp,
                    port: 22,
                },
                "ingressConfigured",
            ),
            (
                Event::DnsCreated {
                    full_name: "f".into(),
                },
                "dnsCreated",
            ),
            (Event::CloudflaredStarted, "cloudflaredStarted"),
            (
                Event::Restarting {
                    reason: ShutdownReason::ChildExited,
                    attempt: 2,
                },
                "restarting",
            ),
            (Event::Restarted { attempt: 2 }, "restarted"),
            (
                Event::RestartGivingUp {
                    attempts: 5,
                    last_error: "e".into(),
                },
                "restartGivingUp",
            ),
            (
                Event::BootstrapFailed {
                    attempt: 2,
                    error: "e".into(),
                },
                "bootstrapFailed",
            ),
            (
                Event::EdgeConnected {
                    conn_index: 0,
                    total: 1,
                },
                "edgeConnected",
            ),
            (
                Event::CloudflaredLog {
                    level: LogLevel::Warn,
                    line: "l".into(),
                },
                "cloudflaredLog",
            ),
            (
                Event::ShuttingDown {
                    reason: ShutdownReason::UserRequested,
                },
                "shuttingDown",
            ),
            (Event::Cleaned, "cleaned"),
        ];
        assert_eq!(cases.len(), EVENT_NAMES.len());
        for (event, expected) in cases {
            let (name, payload) = event_to_payload(&event);
            assert_eq!(name, expected);
            assert!(!payload.is_empty());
            assert!(
                EVENT_NAMES.contains(&name),
                "{name} missing from EVENT_NAMES catalog"
            );
        }
    }

    #[test]
    fn event_names_covers_every_named_variant() {
        // `EVENT_NAMES` is what `Listener#on` validates against, so a
        // variant missing from it would be silently unregisterable. The
        // `every_documented_event_name_is_reachable` case above asserts
        // the other direction; this asserts the sizes agree.
        for event in [
            Event::Banner,
            Event::ResolvingConflicts,
            Event::CreatingTunnel { name: "n".into() },
            Event::IngressConfigured {
                protocol: Protocol::Http,
                port: 1,
            },
            Event::DnsCreated {
                full_name: "f".into(),
            },
            Event::CloudflaredStarted,
            Event::Restarting {
                reason: ShutdownReason::ChildExited,
                attempt: 1,
            },
            Event::Restarted { attempt: 1 },
            Event::RestartGivingUp {
                attempts: 1,
                last_error: "e".into(),
            },
            Event::BootstrapFailed {
                attempt: 1,
                error: "e".into(),
            },
            Event::EdgeConnected {
                conn_index: 0,
                total: 1,
            },
            Event::CloudflaredLog {
                level: LogLevel::Info,
                line: "l".into(),
            },
            Event::ShuttingDown {
                reason: ShutdownReason::UserRequested,
            },
            Event::Cleaned,
        ] {
            let (name, _) = event_to_payload(&event);
            assert_ne!(name, "unknown", "a known variant fell into the catch-all");
            assert!(
                EVENT_NAMES.contains(&name),
                "EVENT_NAMES is missing \"{name}\" — Listener#on would reject it"
            );
        }
    }

    #[test]
    fn pending_events_are_drained_in_production_order() {
        let mut pending = PendingEvents::default();
        pending.push("dnsCreated".into(), "{\"fullName\":\"a\"}".into());
        pending.push("banner".into(), "{}".into());
        pending.push("dnsCreated".into(), "{\"fullName\":\"b\"}".into());

        let drained = pending.drain_named("dnsCreated");
        assert_eq!(
            drained,
            vec![
                "{\"fullName\":\"a\"}".to_string(),
                "{\"fullName\":\"b\"}".to_string()
            ]
        );
        // The unrelated event survives.
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn pending_events_are_capped() {
        let mut pending = PendingEvents::default();
        for i in 0..(PendingEvents::CAPACITY + 10) {
            pending.push("x".into(), i.to_string());
        }
        assert_eq!(pending.len(), PendingEvents::CAPACITY);
    }

    #[test]
    fn removing_a_name_drops_its_pending_events() {
        let mut pending = PendingEvents::default();
        pending.push("a".into(), "1".into());
        pending.push("b".into(), "2".into());
        pending.remove_named("a");
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn protocol_name_matches_sdk() {
        assert_eq!(protocol_name(Protocol::Ssh), "ssh");
        assert_eq!(protocol_name(Protocol::Udp), "udp");
    }
}
