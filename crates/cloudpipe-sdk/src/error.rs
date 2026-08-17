//! Public error types exposed by the SDK.
//!
//! All fallible operations in the SDK return [`Result<T>`] where the error is
//! one of the variants of [`Error`]. Callers can `match` on it without
//! resorting to string parsing.

use thiserror::Error;

/// Convenience alias for `std::result::Result<T, Error>`.
pub type Result<T> = std::result::Result<T, Error>;

/// Top-level error type returned by every fallible SDK call.
#[derive(Debug, Error)]
pub enum Error {
    /// A required credential (token / account / zone / domain) was missing.
    #[error("missing credential: {0}")]
    MissingCredential(&'static str),

    /// The Cloudflare API returned an error. See [`ApiError`] for variants.
    #[error("cloudflare api error: {0}")]
    Cloudflare(#[from] ApiError),

    /// The `cloudflared` binary could not be installed or spawned.
    #[error("cloudflared binary: {0}")]
    Cloudflared(#[from] InstallError),

    /// Underlying I/O failure (reading/writing files, spawning processes, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The provided subdomain violates the DNS label rules.
    #[error("invalid subdomain: {0}")]
    InvalidSubdomain(String),

    /// A healthy tunnel with the same name already exists.
    #[error("subdomain already in use: {0}")]
    SubdomainInUse(String),

    /// `stop()` was called on an already-shut-down handle.
    #[error("tunnel already shut down")]
    AlreadyShutDown,

    /// The background session task panicked.
    #[error("background task panicked: {0}")]
    Join(#[from] tokio::task::JoinError),

    /// Anything else — wrapped from `anyhow::Error` at module boundaries.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Errors originating from the Cloudflare HTTP API.
#[derive(Debug, Error)]
pub enum ApiError {
    /// Network or transport failure.
    #[error("http transport: {0}")]
    Http(#[from] reqwest::Error),

    /// The server responded with a non-JSON body.
    #[error("non-JSON response (HTTP {status}): {body}")]
    NonJson {
        /// HTTP status code.
        status: u16,
        /// Raw response body, truncated to a reasonable size.
        body: String,
    },

    /// The server returned a structured Cloudflare API error.
    #[error("api error: {message}")]
    Api {
        /// Numeric error code from the Cloudflare envelope (may be `None`).
        code: Option<u64>,
        /// Human-readable message from Cloudflare.
        message: String,
        /// Classified semantic kind — see [`CloudflareErrorKind`].
        kind: CloudflareErrorKind,
    },
}

/// Semantic classification of a Cloudflare API error.
///
/// The numeric codes change occasionally; the variant names are stable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudflareErrorKind {
    /// Rate limit hit (10429). Retry after a backoff.
    RateLimited,
    /// DNS record already exists (81053). Caller should reconcile.
    DnsExists,
    /// Authentication failed (10001 / 10000 / 9109). Check the token.
    AuthFailed,
    /// Token itself is invalid (1038).
    InvalidToken,
    /// Any other code. Use [`CloudflareErrorKind::code`] to inspect it.
    Other(u64),
}

impl CloudflareErrorKind {
    /// The numeric Cloudflare error code, when known.
    pub fn code(&self) -> Option<u64> {
        match self {
            Self::RateLimited => Some(10429),
            Self::DnsExists => Some(81053),
            Self::AuthFailed => None,
            Self::InvalidToken => Some(1038),
            Self::Other(code) => Some(*code),
        }
    }
}

/// Errors raised while locating, downloading or spawning `cloudflared`.
#[derive(Debug, Error)]
pub enum InstallError {
    /// `cloudflared` could not be found and the download from every
    /// candidate URL failed. The wrapped errors are the per-URL failures.
    #[error("download failed: {0}")]
    Download(#[source] anyhow::Error),

    /// The downloaded archive did not contain the expected binary.
    #[error("archive did not contain the cloudflared binary")]
    MissingBinary,

    /// The current platform/architecture combination is not supported by
    /// the upstream release asset list.
    #[error("unsupported platform {platform}/{arch}")]
    UnsupportedPlatform {
        /// Operating system name (`std::env::consts::OS`).
        platform: String,
        /// CPU architecture (`std::env::consts::ARCH`).
        arch: String,
    },

    /// `cloudflared` could not be spawned.
    #[error("spawn failed: {0}")]
    Spawn(#[source] std::io::Error),
}

/// Translates a numeric Cloudflare error code into a stable [`CloudflareErrorKind`].
pub(crate) fn classify_error_code(code: u64) -> CloudflareErrorKind {
    match code {
        10429 => CloudflareErrorKind::RateLimited,
        81053 => CloudflareErrorKind::DnsExists,
        10001 | 10000 | 9109 => CloudflareErrorKind::AuthFailed,
        1038 => CloudflareErrorKind::InvalidToken,
        other => CloudflareErrorKind::Other(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_covers_known_codes() {
        assert_eq!(classify_error_code(10429), CloudflareErrorKind::RateLimited);
        assert_eq!(classify_error_code(81053), CloudflareErrorKind::DnsExists);
        assert_eq!(classify_error_code(10001), CloudflareErrorKind::AuthFailed);
        assert_eq!(classify_error_code(9109), CloudflareErrorKind::AuthFailed);
        assert_eq!(classify_error_code(1038), CloudflareErrorKind::InvalidToken);
        assert_eq!(classify_error_code(1234), CloudflareErrorKind::Other(1234));
    }

    #[test]
    fn kind_code_is_stable() {
        assert_eq!(CloudflareErrorKind::RateLimited.code(), Some(10429));
        assert_eq!(CloudflareErrorKind::DnsExists.code(), Some(81053));
        assert_eq!(CloudflareErrorKind::AuthFailed.code(), None);
        assert_eq!(CloudflareErrorKind::Other(42).code(), Some(42));
    }
}
