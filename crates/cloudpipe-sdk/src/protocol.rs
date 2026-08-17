//! Local service protocol schemes accepted by Cloudflare Tunnel ingress.
//!
//! Five protocols are supported: HTTP, HTTPS, TCP, UDP and SSH. The Cloudflare
//! edge always serves public traffic as HTTPS for HTTP/HTTPS tunnels, and as
//! raw TCP/UDP/SSH for the L4 protocols.

use serde::{Deserialize, Serialize};

/// The local service scheme to forward to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// `http://localhost:<port>`
    Http,
    /// `https://localhost:<port>`
    Https,
    /// `tcp://localhost:<port>`
    Tcp,
    /// `udp://localhost:<port>`
    Udp,
    /// `ssh://localhost:<port>`
    Ssh,
}

impl Protocol {
    /// Parses a string token (case-insensitive). Returns `Err(InvalidProtocol)`
    /// for unknown values.
    pub fn parse(value: &str) -> Result<Self, InvalidProtocol> {
        match value.to_ascii_lowercase().as_str() {
            "http" => Ok(Self::Http),
            "https" => Ok(Self::Https),
            "tcp" => Ok(Self::Tcp),
            "udp" => Ok(Self::Udp),
            "ssh" => Ok(Self::Ssh),
            _ => Err(InvalidProtocol(value.to_string())),
        }
    }

    /// Canonical lowercase name, suitable for display.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Tcp => "tcp",
            Self::Udp => "udp",
            Self::Ssh => "ssh",
        }
    }

    /// The local service URL this protocol produces for Cloudflare ingress,
    /// e.g. `http://localhost:8080`.
    pub fn local_service(self, port: u16) -> String {
        format!("{}://localhost:{port}", self.as_str())
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Protocol {
    type Err = InvalidProtocol;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Error returned when [`Protocol::parse`] receives an unsupported token.
#[derive(Debug, thiserror::Error)]
#[error("unknown protocol \"{0}\" — supported: http, https, tcp, udp, ssh")]
pub struct InvalidProtocol(pub String);

/// Validates a DNS label (`a-z`, `0-9`, `-`, not starting/ending with `-`,
/// 3..=63 chars).
pub fn validate_subdomain(subdomain: &str) -> Result<(), InvalidSubdomain> {
    if subdomain.len() < 3 || subdomain.len() > 63 {
        return Err(InvalidSubdomain::new(
            subdomain,
            format!(
                "must be between 3 and 63 characters (got {})",
                subdomain.len()
            ),
        ));
    }
    let valid = subdomain
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !subdomain.starts_with('-')
        && !subdomain.ends_with('-');
    if !valid {
        return Err(InvalidSubdomain::new(
            subdomain,
            "use lowercase letters, digits and dashes, not starting/ending with a dash",
        ));
    }
    Ok(())
}

/// Error returned by [`validate_subdomain`].
#[derive(Debug, thiserror::Error)]
#[error("invalid subdomain \"{value}\": {reason}")]
pub struct InvalidSubdomain {
    /// The offending value.
    pub value: String,
    /// Human-readable reason.
    pub reason: String,
}

impl InvalidSubdomain {
    /// Constructs a new `InvalidSubdomain`.
    pub fn new(value: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            reason: reason.into(),
        }
    }
}

impl From<InvalidSubdomain> for crate::Error {
    fn from(err: InvalidSubdomain) -> Self {
        crate::Error::InvalidSubdomain(format!("\"{}\": {}", err.value, err.reason))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_round_trips() {
        for proto in [
            Protocol::Http,
            Protocol::Https,
            Protocol::Tcp,
            Protocol::Udp,
            Protocol::Ssh,
        ] {
            assert_eq!(Protocol::parse(proto.as_str()).unwrap(), proto);
        }
    }

    #[test]
    fn protocol_parse_is_case_insensitive() {
        assert_eq!(Protocol::parse("HTTP").unwrap(), Protocol::Http);
        assert_eq!(Protocol::parse("Tcp").unwrap(), Protocol::Tcp);
    }

    #[test]
    fn protocol_parse_rejects_unknown() {
        assert!(Protocol::parse("ftp").is_err());
        assert!(Protocol::parse("").is_err());
    }

    #[test]
    fn protocol_local_service() {
        assert_eq!(Protocol::Http.local_service(8080), "http://localhost:8080");
        assert_eq!(
            Protocol::Https.local_service(8443),
            "https://localhost:8443"
        );
        assert_eq!(Protocol::Tcp.local_service(22), "tcp://localhost:22");
        assert_eq!(Protocol::Udp.local_service(5353), "udp://localhost:5353");
        assert_eq!(Protocol::Ssh.local_service(22), "ssh://localhost:22");
    }

    #[test]
    fn subdomain_validation() {
        assert!(validate_subdomain("myapp").is_ok());
        assert!(validate_subdomain("my-app-2").is_ok());
        assert!(validate_subdomain("MyApp").is_err());
        assert!(validate_subdomain("-myapp").is_err());
        assert!(validate_subdomain("myapp-").is_err());
        assert!(validate_subdomain("ab").is_err());
        assert!(validate_subdomain(&"a".repeat(64)).is_err());
        assert!(validate_subdomain("my app").is_err());
    }

    #[test]
    fn invalid_protocol_keeps_token() {
        let err = Protocol::parse("rdp").unwrap_err();
        assert_eq!(err.0, "rdp");
    }
}
