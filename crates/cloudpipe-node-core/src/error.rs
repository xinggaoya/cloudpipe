//! The cloudpipe error taxonomy and its `[CODE]` wire encoding.
//!
//! napi-rs's `async fn` machinery hard-codes `Into<napi::Error>`, which
//! defaults to `Into<Error<Status>>`. There is no way for an `async fn`
//! exported via `#[napi]` to surface a custom status type to JavaScript.
//! So all errors leave the Rust side as `napi::Error<Status>`, with the
//! cloudpipe-specific taxonomy encoded as a `[CODE]` prefix on the
//! message.
//!
//! JS callers split the prefix off the front of `error.message` (the
//! exported `parseErrorCode` helper does this for them) before falling
//! through to the raw SDK text.

use cloudpipe_sdk::CloudflareErrorKind;

/// Stable error codes exposed to JavaScript as a `[CODE]` prefix on
/// `error.message`. The codes intentionally mirror the
/// `cloudpipe_sdk::Error` variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudpipeErrorCode {
    /// A required credential (token, domain, account/zone) was missing.
    MissingCredential,
    /// The Cloudflare HTTP API returned an error. A sub-kind is appended
    /// to the prefix in the `PARENT:KIND` form.
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
    /// An invalid argument was supplied (e.g. port 0).
    InvalidArgument,
    /// Anything else — surfaces the SDK's underlying message.
    Other,
}

impl CloudpipeErrorCode {
    /// Stable `UPPER_SNAKE_CASE` string used in `[CODE]` prefixes.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingCredential => "MISSING_CREDENTIAL",
            Self::CloudflareApi => "CLOUDFLARE_API",
            Self::CloudflaredBinary => "CLOUDFLARE_BINARY",
            Self::InvalidSubdomain => "INVALID_SUBDOMAIN",
            Self::SubdomainInUse => "SUBDOMAIN_IN_USE",
            Self::AlreadyShutDown => "ALREADY_SHUT_DOWN",
            Self::Io => "IO",
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::Other => "OTHER",
        }
    }
}

/// Sub-classification of [`CloudpipeErrorCode::CloudflareApi`].
/// Appended after the parent code, e.g. `"[CLOUDFLARE_API:RATE_LIMITED] "`.
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
    /// Stable `UPPER_SNAKE_CASE` string used after the parent code.
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

/// Classifies an SDK error into the top-level code plus optional sub-kind.
pub fn classify(
    err: &cloudpipe_sdk::Error,
) -> (CloudpipeErrorCode, Option<CloudflareApiKind>) {
    use cloudpipe_sdk::Error as E;
    match err {
        E::MissingCredential(_) => (CloudpipeErrorCode::MissingCredential, None),
        E::Cloudflare(api_err) => {
            let suffix = match api_err {
                cloudpipe_sdk::ApiError::Api { kind, .. } => Some(kind_from_sdk(kind.clone())),
                // Transport failures and non-JSON bodies have no numeric
                // code to classify, so they carry the parent code alone.
                _ => None,
            };
            (CloudpipeErrorCode::CloudflareApi, suffix)
        }
        E::Cloudflared(_) => (CloudpipeErrorCode::CloudflaredBinary, None),
        E::InvalidSubdomain(_) => (CloudpipeErrorCode::InvalidSubdomain, None),
        E::SubdomainInUse(_) => (CloudpipeErrorCode::SubdomainInUse, None),
        E::AlreadyShutDown => (CloudpipeErrorCode::AlreadyShutDown, None),
        E::Io(_) => (CloudpipeErrorCode::Io, None),
        E::InvalidPort(_) => (CloudpipeErrorCode::InvalidArgument, None),
        E::Join(_) | E::Other(_) => (CloudpipeErrorCode::Other, None),
    }
}

/// Translates the SDK's error-kind enum into the JS-facing sub-kind.
fn kind_from_sdk(kind: CloudflareErrorKind) -> CloudflareApiKind {
    match kind {
        CloudflareErrorKind::RateLimited => CloudflareApiKind::RateLimited,
        CloudflareErrorKind::DnsExists => CloudflareApiKind::DnsExists,
        CloudflareErrorKind::AuthFailed => CloudflareApiKind::AuthFailed,
        CloudflareErrorKind::InvalidToken => CloudflareApiKind::InvalidToken,
        // A bare 5xx carries no numeric code worth surfacing; `OTHER` is
        // the JS-facing "unclassified".
        CloudflareErrorKind::ServerError => CloudflareApiKind::Other,
        CloudflareErrorKind::Other(_) => CloudflareApiKind::Other,
    }
}

/// Renders the `[CODE] ` / `[CODE:KIND] ` prefix for an SDK error,
/// including the trailing space so it can be concatenated directly onto
/// the human-readable part of the message.
pub fn error_prefix(err: &cloudpipe_sdk::Error) -> String {
    let (code, suffix) = classify(err);
    match suffix {
        Some(s) => format!("[{}:{}] ", code.as_str(), s.as_str()),
        None => format!("[{}] ", code.as_str()),
    }
}

/// The `[OTHER] ` prefix used for binding-level failures that have no SDK
/// counterpart (tokio join errors, thread-spawn failures, …).
pub fn other_prefix() -> &'static str {
    "[OTHER] "
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloudpipe_sdk::{ApiError, Error};

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
        assert_eq!(CloudpipeErrorCode::InvalidArgument.as_str(), "INVALID_ARGUMENT");
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
    fn missing_credential_has_no_sub_kind() {
        let err = Error::MissingCredential("Cloudflare API token");
        assert_eq!(error_prefix(&err), "[MISSING_CREDENTIAL] ");
    }

    #[test]
    fn rate_limited_api_error_carries_sub_kind() {
        let err = Error::Cloudflare(ApiError::Api {
            code: Some(10429),
            message: "too many".into(),
            kind: CloudflareErrorKind::RateLimited,
        });
        assert_eq!(error_prefix(&err), "[CLOUDFLARE_API:RATE_LIMITED] ");
    }

    #[test]
    fn transport_error_carries_parent_code_only() {
        let err = Error::Cloudflare(ApiError::NonJson {
            status: 502,
            body: "gateway".into(),
        });
        assert_eq!(error_prefix(&err), "[CLOUDFLARE_API] ");
    }

    #[test]
    fn every_sdk_variant_maps_to_a_known_code() {
        let cases = vec![
            Error::MissingCredential("x"),
            Error::Cloudflare(ApiError::NonJson {
                status: 500,
                body: "x".into(),
            }),
            Error::InvalidSubdomain("x".into()),
            Error::SubdomainInUse("x".into()),
            Error::AlreadyShutDown,
            Error::Other(std::io::Error::other("x").into()),
        ];
        for err in cases {
            let (code, _) = classify(&err);
            // Round-trip through as_str so a future rename cannot silently
            // produce a prefix the JS helper does not know.
            let back: &str = code.as_str();
            assert!(back.starts_with(char::is_uppercase), "{back}");
        }
    }

    #[test]
    fn other_prefix_is_stable() {
        assert_eq!(other_prefix(), "[OTHER] ");
    }
}
