//! Cross-boundary error type for the napi binding.
//!
//! napi-rs's async-fn machinery hard-codes `Into<napi::Error>`, which
//! defaults to `Into<Error<Status>>`. There is no way for an `async fn`
//! exported via `#[napi]` to surface a custom status type to JavaScript.
//! So all errors leave the Rust side as `napi::Error<Status>`, with the
//! cloudpipe-specific taxonomy encoded as a `[CODE]` prefix on the
//! message.
//!
//! JS callers should split the prefix off the front of `error.message`
//! (a tiny helper is published alongside the TypeScript definitions)
//! before falling through to the raw SDK text.
//!
//! [`CloudpipeErrorCode`] is still re-exported as a Rust enum so other
//! Rust code (and the TypeScript types) can name the codes by identity.

use napi::Error as NapiError;
use napi::Status;

/// Alias that pins the napi error's `status` slot to the
/// `napi::Status` enum. `napi::Error<S>` is generic over `S: AsRef<str>`,
/// and the bare `napi::Error` in async-fn return-position forces the
/// compiler to fix the slot — sometimes to `String`, sometimes to
/// `Status`. Pinning it once at the module boundary removes the churn.
pub(crate) type FixedError = NapiError<Status>;

/// Stable error codes exposed to JavaScript as a `[CODE]` prefix on
/// `error.message`. The codes intentionally mirror the
/// `cloudpipe_sdk::Error` variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudpipeErrorCode {
    /// A required credential (token, domain, account/zone) was missing.
    MissingCredential,
    /// The Cloudflare HTTP API returned an error. Sub-kind is appended
    /// to the message in square brackets.
    CloudflareApi,
    /// The `cloudflared` binary could not be located, downloaded or
    /// spawned.
    CloudflaredBinary,
    /// The provided subdomain violates the DNS label rules.
    InvalidSubdomain,
    /// A healthy tunnel with the same name already exists on Cloudflare.
    SubdomainInUse,
    /// `close()` (or its SDK equivalent) was called twice on a handle.
    AlreadyShutDown,
    /// Generic I/O failure (file, network, process spawn).
    Io,
    /// Anything else — surfaces the SDK's underlying message.
    Other,
}

impl CloudpipeErrorCode {
    /// Stable UPPER_SNAKE_CASE string used in `[CODE]` prefixes`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingCredential => "MISSING_CREDENTIAL",
            Self::CloudflareApi => "CLOUDFLARE_API",
            Self::CloudflaredBinary => "CLOUDFLARE_BINARY",
            Self::InvalidSubdomain => "INVALID_SUBDOMAIN",
            Self::SubdomainInUse => "SUBDOMAIN_IN_USE",
            Self::AlreadyShutDown => "ALREADY_SHUT_DOWN",
            Self::Io => "IO",
            Self::Other => "OTHER",
        }
    }
}

/// Sub-classification of [`CloudpipeErrorCode::CloudflareApi`].
/// Appended in square brackets after the parent code, e.g.
/// `"[CLOUDFLARE_API:RATE_LIMITED] ..."`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudflareApiKind {
    /// 10429 — hit too many requests.
    RateLimited,
    /// 81053 — DNS record already exists.
    DnsExists,
    /// 10001 / 10000 / 9109 — auth failed.
    AuthFailed,
    /// 1038 — token itself is invalid.
    InvalidToken,
    /// Any other numeric code.
    Other,
}

impl CloudflareApiKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "RATE_LIMITED",
            Self::DnsExists => "DNS_EXISTS",
            Self::AuthFailed => "AUTH_FAILED",
            Self::InvalidToken => "INVALID_TOKEN",
            Self::Other => "OTHER",
        }
    }
}

/// Translates any [`cloudpipe_sdk::Error`] into a napi `Error` whose
/// `message` carries the cloudpipe code as a `[CODE]` prefix.
pub fn from_sdk_error(err: cloudpipe_sdk::Error) -> FixedError {
    let (code, suffix) = match &err {
        cloudpipe_sdk::Error::MissingCredential(_) => (CloudpipeErrorCode::MissingCredential, None),
        cloudpipe_sdk::Error::Cloudflare(api_err) => {
            let suffix = if let cloudpipe_sdk::ApiError::Api { kind, .. } = api_err {
                Some(kind_to_str(kind.clone()))
            } else {
                None
            };
            (CloudpipeErrorCode::CloudflareApi, suffix)
        }
        cloudpipe_sdk::Error::Cloudflared(_) => (CloudpipeErrorCode::CloudflaredBinary, None),
        cloudpipe_sdk::Error::InvalidSubdomain(_) => (CloudpipeErrorCode::InvalidSubdomain, None),
        cloudpipe_sdk::Error::SubdomainInUse(_) => (CloudpipeErrorCode::SubdomainInUse, None),
        cloudpipe_sdk::Error::AlreadyShutDown => (CloudpipeErrorCode::AlreadyShutDown, None),
        cloudpipe_sdk::Error::Io(_) => (CloudpipeErrorCode::Io, None),
        cloudpipe_sdk::Error::Join(_) | cloudpipe_sdk::Error::Other(_) => {
            (CloudpipeErrorCode::Other, None)
        }
    };

    let prefix = match suffix {
        Some(s) => format!("[{}:{}] ", code.as_str(), s),
        None => format!("[{}] ", code.as_str()),
    };
    NapiError::new(Status::GenericFailure, format!("{prefix}{err}"))
}

fn kind_to_str(kind: cloudpipe_sdk::CloudflareErrorKind) -> &'static str {
    use cloudpipe_sdk::CloudflareErrorKind as K;
    match kind {
        K::RateLimited => CloudflareApiKind::RateLimited.as_str(),
        K::DnsExists => CloudflareApiKind::DnsExists.as_str(),
        K::AuthFailed => CloudflareApiKind::AuthFailed.as_str(),
        K::InvalidToken => CloudflareApiKind::InvalidToken.as_str(),
        K::Other(_) => CloudflareApiKind::Other.as_str(),
    }
}

/// Generic fall-through for things that have no SDK equivalent (e.g.
/// a tokio join error or a thread-spawn failure).
pub fn other(msg: String) -> FixedError {
    let prefix = format!("[{}] ", CloudpipeErrorCode::Other.as_str());
    NapiError::new(Status::GenericFailure, format!("{prefix}{msg}"))
}

/// Maps a [`Status::InvalidArg`] when the JS side supplies a value
/// outside the Rust SDK's accepted range (e.g. port > 65535 or an
/// unknown protocol string).
pub fn invalid_arg<S: Into<String>>(msg: S) -> FixedError {
    NapiError::new(Status::InvalidArg, msg.into())
}

/// Channel dropped its reply before the session task could answer —
/// surfaces as `OTHER`.
pub fn channel_dropped() -> FixedError {
    other("session task dropped the reply channel".to_string())
}

/// The session task's sender side is gone (handle was dropped, thread
/// panicked and shut down, etc.).
pub fn session_gone() -> FixedError {
    other("session task is gone".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_strings_are_stable() {
        assert_eq!(
            CloudpipeErrorCode::MissingCredential.as_str(),
            "MISSING_CREDENTIAL"
        );
        assert_eq!(CloudpipeErrorCode::CloudflareApi.as_str(), "CLOUDFLARE_API");
        assert_eq!(
            CloudpipeErrorCode::CloudflaredBinary.as_str(),
            "CLOUDFLARE_BINARY"
        );
        assert_eq!(
            CloudpipeErrorCode::InvalidSubdomain.as_str(),
            "INVALID_SUBDOMAIN"
        );
        assert_eq!(
            CloudpipeErrorCode::SubdomainInUse.as_str(),
            "SUBDOMAIN_IN_USE"
        );
        assert_eq!(
            CloudpipeErrorCode::AlreadyShutDown.as_str(),
            "ALREADY_SHUT_DOWN"
        );
        assert_eq!(CloudpipeErrorCode::Io.as_str(), "IO");
        assert_eq!(CloudpipeErrorCode::Other.as_str(), "OTHER");
    }

    #[test]
    fn kind_strings_are_stable() {
        assert_eq!(CloudflareApiKind::RateLimited.as_str(), "RATE_LIMITED");
        assert_eq!(CloudflareApiKind::DnsExists.as_str(), "DNS_EXISTS");
        assert_eq!(CloudflareApiKind::AuthFailed.as_str(), "AUTH_FAILED");
        assert_eq!(CloudflareApiKind::InvalidToken.as_str(), "INVALID_TOKEN");
        assert_eq!(CloudflareApiKind::Other.as_str(), "OTHER");
    }

    #[test]
    fn invalid_arg_uses_invalidarg_status() {
        let e = invalid_arg("bad port".to_string());
        assert_eq!(e.status, Status::InvalidArg);
        assert_eq!(e.reason, "bad port");
    }

    #[test]
    fn other_wraps_with_other_code_prefix() {
        let e = other("boom".to_string());
        assert_eq!(e.status, Status::GenericFailure);
        assert!(e.reason.starts_with("[OTHER] "));
        assert!(e.reason.ends_with("boom"));
    }
}
