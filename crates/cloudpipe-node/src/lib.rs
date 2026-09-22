//! napi-rs bindings for the cloudpipe Rust SDK.
//!
//! Exposes a single `connect()` entry point returning a [`Listener`] that
//! wraps `cloudpipe_sdk::TunnelHandle`. See `../README.md` for the user-facing
//! API and examples.

#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

mod error;
mod event;
mod listener;

use std::sync::Arc;

use cloudpipe_sdk::Protocol;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use once_cell::sync::Lazy;
use tokio::runtime::Runtime;

pub use error::CloudpipeErrorCode;
pub use listener::Listener;

/// Single shared tokio runtime for every async operation in this binding.
///
/// Mirrors the `ngrok-js` approach: napi callbacks are already running on
/// libuv worker threads, but `cloudpipe-sdk` needs a tokio runtime for its
/// async tasks (Cloudflare API calls, `cloudflared` process supervision).
static RUNTIME: Lazy<Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .thread_name("cloudpipe-node")
        .build()
        .expect("cloudpipe-node: failed to build tokio runtime")
});

/// Returns the shared tokio runtime handle. Used by `Listener` and async
/// napi methods to spawn background work.
pub(crate) fn runtime() -> &'static Runtime {
    &RUNTIME
}

/// Options accepted by [`connect`]. Mirrors the fields exposed by
/// `cloudpipe_sdk::TunnelBuilder`.
#[napi(object)]
pub struct ConnectOptions {
    /// Cloudflare API token. **Required.**
    pub token: String,

    /// Base domain (e.g. `example.com`). If the token has access to exactly
    /// one zone, this can be omitted and the SDK will auto-discover.
    #[napi(js_name = "domain")]
    pub domain: Option<String>,

    /// Local service protocol. One of `"http"`, `"https"`, `"tcp"`, `"udp"`,
    /// `"ssh"`. Defaults to `"http"`.
    #[napi(js_name = "protocol")]
    pub protocol: Option<String>,

    /// Local port to expose. Defaults to `8080`.
    pub port: Option<u32>,

    /// Desired subdomain. If omitted or empty, a random one is generated.
    #[napi(js_name = "subdomain")]
    pub subdomain: Option<String>,

    /// When `true`, the SDK automatically respawns `cloudflared` on the
    /// same public URL if it crashes. Defaults to `false`.
    #[napi(js_name = "autoRestart")]
    pub auto_restart: Option<bool>,

    /// Path to a pre-installed `cloudflared` binary. When omitted, the SDK
    /// searches `PATH`, then falls back to `~/.cfp/bin/cloudflared`, then
    /// downloads from GitHub releases.
    #[napi(js_name = "cloudflaredPath")]
    pub cloudflared_path: Option<String>,

    /// Override the GitHub mirror prefix used when downloading `cloudflared`.
    /// Pass an empty string to disable mirroring.
    #[napi(js_name = "githubProxy")]
    pub github_proxy: Option<String>,
}

impl ConnectOptions {
    /// Parses the protocol string into the SDK enum, returning a plain
    /// `&'static str` so this function is unit-testable without linking
    /// the napi native symbols.
    fn protocol_enum(&self) -> std::result::Result<Protocol, &'static str> {
        let raw = self.protocol.as_deref().unwrap_or("http");
        Protocol::parse(raw).map_err(|_| "invalid protocol — supported: http, https, tcp, udp, ssh")
    }
}

/// Opens a new tunnel and returns a handle to it.
///
/// ```text
/// const listener = await connect({
///   token: 'cf_xxx',
///   domain: 'example.com',
///   port: 8080,
/// });
///
/// console.log(listener.url());
/// await listener.close();
/// ```
#[napi]
pub async fn connect(options: ConnectOptions) -> Result<Listener> {
    Listener::start(options).await
}

/// Library version, mirrored from `Cargo.toml` for diagnostics.
#[napi]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Extracts the `[CODE]` prefix from a cloudpipe error message.
///
/// Cloudpipe surfaces its error taxonomy as a `[CODE]` prefix on
/// `error.message` (e.g. `"[CLOUDFLARE_API:RATE_LIMITED] ..."`). This
/// helper splits the prefix off without forcing JS callers to write
/// their own regex.
///
/// Returns:
///
/// - the bare code (`"MISSING_CREDENTIAL"`) for top-level codes,
/// - the composed `PARENT:KIND` form for `CloudflareApi` errors
///   (`"CLOUDFLARE_API:RATE_LIMITED"`),
/// - or `null` when no `[CODE]` prefix is present.
#[napi]
pub fn parse_error_code(message: String) -> Option<String> {
    use crate::error::{CloudflareApiKind, CloudpipeErrorCode};
    let raw = message.strip_prefix('[')?;
    let end = raw.find(']')?;
    let inner = &raw[..end];
    let mut parts = inner.splitn(2, ':');
    let parent = parts.next()?;

    // Validate against the typed enum so a future code rename/refactor
    // surfaces as a Rust compile error instead of a silent JS-side
    // string drift.
    let parent_code = match parent {
        "MISSING_CREDENTIAL" => CloudpipeErrorCode::MissingCredential,
        "CLOUDFLARE_API" => CloudpipeErrorCode::CloudflareApi,
        "CLOUDFLARE_BINARY" => CloudpipeErrorCode::CloudflaredBinary,
        "INVALID_SUBDOMAIN" => CloudpipeErrorCode::InvalidSubdomain,
        "SUBDOMAIN_IN_USE" => CloudpipeErrorCode::SubdomainInUse,
        "ALREADY_SHUT_DOWN" => CloudpipeErrorCode::AlreadyShutDown,
        "IO" => CloudpipeErrorCode::Io,
        "OTHER" => CloudpipeErrorCode::Other,
        // Unknown parent — fall through and return the raw text. We
        // deliberately don't fail so forward-compat works both ways:
        // newer SDK emits an unknown parent -> older JS helper still
        // gets a string back, not an exception.
        _ => return Some(parent.to_string()),
    };
    let _ = parent_code;

    // For CloudflareApi, surface the parent + kind verbatim. Other
    // codes don't carry a sub-kind.
    let suffix = match parts.next() {
        Some(s) => s,
        None => return Some(parent.to_string()),
    };
    // Validate the sub-kind against the typed enum; if it doesn't match,
    // return the raw text (forward compat again).
    let suffix_code = match suffix {
        "RATE_LIMITED" => CloudflareApiKind::RateLimited,
        "DNS_EXISTS" => CloudflareApiKind::DnsExists,
        "AUTH_FAILED" => CloudflareApiKind::AuthFailed,
        "INVALID_TOKEN" => CloudflareApiKind::InvalidToken,
        "OTHER" => CloudflareApiKind::Other,
        _ => return Some(format!("{parent}:{suffix}")),
    };
    let _ = suffix_code;
    Some(format!("{parent}:{suffix}"))
}

// Silence unused-Arc warning while we wire things up incrementally.
#[allow(dead_code)]
fn _arc<T>(t: T) -> Arc<T> {
    Arc::new(t)
}

#[cfg(test)]
mod tests {
    use super::parse_error_code;

    #[test]
    fn parses_simple_code() {
        assert_eq!(
            parse_error_code("[MISSING_CREDENTIAL] need a token".into()),
            Some("MISSING_CREDENTIAL".to_string())
        );
    }

    #[test]
    fn parses_compound_kind() {
        assert_eq!(
            parse_error_code("[CLOUDFLARE_API:RATE_LIMITED] too many".into()),
            Some("CLOUDFLARE_API:RATE_LIMITED".to_string())
        );
    }

    #[test]
    fn returns_null_on_plain_message() {
        assert_eq!(parse_error_code("no prefix here".into()), None);
    }

    #[test]
    fn returns_null_on_malformed_prefix() {
        assert_eq!(parse_error_code("[not closed here".into()), None);
    }

    #[test]
    fn unknown_code_passes_through() {
        // Forward-compat: a future SDK version emits a brand-new code
        // that this helper doesn't know about -> return it verbatim
        // instead of throwing.
        assert_eq!(
            parse_error_code("[FUTURE_CODE] blah".into()),
            Some("FUTURE_CODE".to_string())
        );
        assert_eq!(
            parse_error_code("[FUTURE_CODE:NEXT_KIND] blah".into()),
            Some("FUTURE_CODE:NEXT_KIND".to_string())
        );
    }

    #[test]
    fn known_parent_with_unknown_subkind_passes_through() {
        assert_eq!(
            parse_error_code("[CLOUDFLARE_API:RATE_LIMITED_EXTENDED] blah".into()),
            Some("CLOUDFLARE_API:RATE_LIMITED_EXTENDED".to_string())
        );
    }
}
