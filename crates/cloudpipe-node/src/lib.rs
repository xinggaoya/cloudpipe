//! napi-rs bindings for the cloudpipe Rust SDK.
//!
//! Exposes a single `connect()` entry point returning a [`Listener`] that
//! wraps `cloudpipe_sdk::TunnelHandle`. See `../README.md` for the
//! user-facing API and examples.
//!
//! # Where the logic lives
//!
//! This crate is a `cdylib`, so a `cargo test` harness for it cannot link
//! (the `napi_*` symbols only resolve inside a Node.js process). Anything
//! worth testing therefore lives in the napi-free `cloudpipe-node-core`
//! crate, and this file holds only the `#[napi]`-annotated shells that
//! delegate to it.

#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

mod error;
mod listener;

use cloudpipe_node_core::{CloudflareApiKind, CloudpipeErrorCode};
use tokio::runtime::Runtime;

pub use listener::Listener;

use napi::bindgen_prelude::*;
use napi_derive::napi;

/// Single shared tokio runtime for every async operation in this binding.
///
/// napi callbacks already run on libuv worker threads, but the SDK needs a
/// tokio runtime for its Cloudflare calls and `cloudflared` supervision.
static RUNTIME: std::sync::LazyLock<Runtime> = std::sync::LazyLock::new(|| {
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

/// Every event name the SDK may emit, in the order documented on the
/// package README. Lets JS callers validate a name before registering.
#[napi]
pub fn event_names() -> Vec<String> {
    cloudpipe_node_core::event::EVENT_NAMES
        .iter()
        .map(|s| (*s).to_string())
        .collect()
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
    cloudpipe_node_core::extract_code(&message)
}

/// The complete list of top-level cloudpipe error codes.
#[napi]
pub fn error_codes() -> Vec<String> {
    [
        CloudpipeErrorCode::MissingCredential,
        CloudpipeErrorCode::CloudflareApi,
        CloudpipeErrorCode::CloudflaredBinary,
        CloudpipeErrorCode::InvalidSubdomain,
        CloudpipeErrorCode::SubdomainInUse,
        CloudpipeErrorCode::AlreadyShutDown,
        CloudpipeErrorCode::Io,
        CloudpipeErrorCode::Other,
    ]
    .iter()
    .map(|c| c.as_str().to_string())
    .collect()
}

/// The sub-kind catalog that may follow `CLOUDFLARE_API:`.
#[napi]
pub fn cloudflare_error_kinds() -> Vec<String> {
    [
        CloudflareApiKind::RateLimited,
        CloudflareApiKind::DnsExists,
        CloudflareApiKind::AuthFailed,
        CloudflareApiKind::InvalidToken,
        CloudflareApiKind::Other,
    ]
    .iter()
    .map(|k| k.as_str().to_string())
    .collect()
}
