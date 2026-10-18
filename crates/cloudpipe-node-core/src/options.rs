//! Validation and normalization of the options `connect()` accepts from
//! JavaScript.
//!
//! These return plain `Result<_, String>` rather than a napi error type
//! so they are unit-testable without linking napi symbols (which only
//! resolve inside a Node.js process). The binding wraps the message into
//! a `napi::Error` at the call site.

use cloudpipe_sdk::Protocol;

/// Message returned for an unrecognised protocol string.
pub const INVALID_PROTOCOL: &str = "invalid protocol — supported: http, https, tcp, udp, ssh";

/// Message returned when the requested port is outside `1..=65535`.
pub const INVALID_PORT: &str = "port must be an integer between 1 and 65535";

/// Parses the JS-supplied protocol string into the SDK enum.
///
/// An absent or blank value defaults to `http`, matching the CLI.
pub fn parse_protocol(raw: Option<&str>) -> Result<Protocol, String> {
    match raw.map(str::trim) {
        None | Some("") => Ok(Protocol::Http),
        Some(value) => Protocol::parse(value).map_err(|_| INVALID_PROTOCOL.to_string()),
    }
}

/// Coerces the JS-side `Option<u32>` port to a `u16`.
///
/// An absent value defaults to `8080`. Zero is rejected: `cloudflared`
/// would happily accept `http://localhost:0` and then fail to connect on
/// every request, which is a far worse experience than an immediate
/// `InvalidArg`.
pub fn parse_port(value: Option<u32>) -> Result<u16, String> {
    match value {
        None => Ok(8080),
        Some(0) => Err(INVALID_PORT.to_string()),
        Some(p) => u16::try_from(p).map_err(|_| format!("{INVALID_PORT} (got {p})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_defaults_to_http() {
        assert_eq!(parse_protocol(None).unwrap(), Protocol::Http);
        assert_eq!(parse_protocol(Some("")).unwrap(), Protocol::Http);
        assert_eq!(parse_protocol(Some("   ")).unwrap(), Protocol::Http);
    }

    #[test]
    fn protocol_parses_every_supported_value() {
        for (raw, expected) in [
            ("http", Protocol::Http),
            ("https", Protocol::Https),
            ("tcp", Protocol::Tcp),
            ("udp", Protocol::Udp),
            ("ssh", Protocol::Ssh),
        ] {
            assert_eq!(parse_protocol(Some(raw)).unwrap(), expected);
        }
    }

    #[test]
    fn protocol_is_case_insensitive_and_trimmed() {
        assert_eq!(parse_protocol(Some("TCP")).unwrap(), Protocol::Tcp);
        assert_eq!(parse_protocol(Some(" ssh ")).unwrap(), Protocol::Ssh);
    }

    #[test]
    fn protocol_rejects_unknown() {
        let err = parse_protocol(Some("gopher")).unwrap_err();
        assert_eq!(err, INVALID_PROTOCOL);
        assert!(err.contains("http"), "error should list valid options");
    }

    #[test]
    fn port_defaults_to_8080() {
        assert_eq!(parse_port(None).unwrap(), 8080);
    }

    #[test]
    fn port_accepts_valid_range() {
        assert_eq!(parse_port(Some(1)).unwrap(), 1);
        assert_eq!(parse_port(Some(8080)).unwrap(), 8080);
        assert_eq!(parse_port(Some(65535)).unwrap(), 65535);
    }

    #[test]
    fn port_rejects_zero() {
        let err = parse_port(Some(0)).unwrap_err();
        assert_eq!(err, INVALID_PORT);
    }

    #[test]
    fn port_rejects_above_u16_max() {
        let err = parse_port(Some(70_000)).unwrap_err();
        assert!(err.contains("70000"), "message should echo the bad value");
    }
}
