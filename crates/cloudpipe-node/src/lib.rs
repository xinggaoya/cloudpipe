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

/// Installs a `tracing` subscriber for the SDK's internal `debug!`/`warn!`
/// output.
///
/// Without this, a Node consumer sees nothing at all when the supervisor
/// retries a reconnect or the Cloudflare client backs off — the diagnostics
/// exist but are dropped on the floor, because nothing collects them. The
/// `cfp` CLI installs its own subscriber for the same reason.
///
/// Reads `CFP_LOG=trace|debug|info|warn|error|off`; anything else, or an
/// unset variable, disables logging so a library never writes to a
/// consumer's stderr uninvited.
///
/// Safe to call more than once; later calls are ignored.
#[napi]
pub fn init_logging() {
    let level = match std::env::var("CFP_LOG")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "trace" => tracing::Level::TRACE,
        "debug" => tracing::Level::DEBUG,
        "info" => tracing::Level::INFO,
        "warn" | "warning" => tracing::Level::WARN,
        "error" => tracing::Level::ERROR,
        _ => return,
    };
    // A duplicate install is an error, not a panic: the addon may be
    // loaded into more than one context.
    let _ = tracing::subscriber::set_global_default(StderrSubscriber { level });
}

/// Minimal `tracing::Subscriber` writing to stderr.
///
/// Spans are not tracked — nothing in the SDK creates any, and supporting
/// them properly is where the complexity lives. `tracing-subscriber` would
/// work too, but it is a heavy dependency for an addon whose whole selling
/// point is a small install.
struct StderrSubscriber {
    level: tracing::Level,
}

impl tracing::Subscriber for StderrSubscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() <= self.level
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        use std::io::Write as _;
        let meta = event.metadata();
        let mut visitor = FieldVisitor(String::new());
        event.record(&mut visitor);
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "[cloudpipe {}] {}", meta.level(), meta.target());
        let _ = writeln!(err, "  {}", visitor.0.trim());
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// Concatenates an event's message fields into one string.
struct FieldVisitor(String);

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        let _ = write!(self.0, "{} = {value:?} ", field.name());
    }
}

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

    /// Skip TLS verification when the origin is `https://` with a
    /// self-signed certificate — the normal case for a local dev server.
    /// Defaults to `false`.
    #[napi(js_name = "noTlsVerify")]
    pub no_tls_verify: Option<bool>,

    /// Send this `Host` header to the origin instead of the public
    /// hostname. Needed when the local service routes on its own host.
    #[napi(js_name = "originHostHeader")]
    pub origin_host_header: Option<String>,

    /// Reconnection budget used when `autoRestart` is enabled.
    /// Defaults to 5 attempts with a 500 ms doubling backoff.
    #[napi(js_name = "restartPolicy")]
    pub restart_policy: Option<RestartPolicyOptions>,
}

/// Reconnection budget for `autoRestart`.
#[napi(object)]
pub struct RestartPolicyOptions {
    /// Consecutive bootstrap failures tolerated before giving up.
    pub max_attempts: Option<u32>,
    /// Delay before the first retry, in milliseconds.
    #[napi(js_name = "baseDelayMs")]
    pub base_delay_ms: Option<u32>,
    /// Ceiling on any single retry delay, in milliseconds.
    #[napi(js_name = "maxDelayMs")]
    pub max_delay_ms: Option<u32>,
}

impl RestartPolicyOptions {
    /// Merges the JS-supplied subset over the defaults.
    fn resolve(&self) -> cloudpipe_sdk::RestartPolicy {
        let default = cloudpipe_sdk::RestartPolicy::default();
        cloudpipe_sdk::RestartPolicy {
            max_attempts: self.max_attempts.unwrap_or(default.max_attempts),
            base_delay: self
                .base_delay_ms
                .map(|ms| std::time::Duration::from_millis(ms.into()))
                .unwrap_or(default.base_delay),
            max_delay: self
                .max_delay_ms
                .map(|ms| std::time::Duration::from_millis(ms.into()))
                .unwrap_or(default.max_delay),
        }
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
        CloudpipeErrorCode::InvalidDomain,
        CloudpipeErrorCode::SubdomainInUse,
        CloudpipeErrorCode::AlreadyShutDown,
        CloudpipeErrorCode::Io,
        CloudpipeErrorCode::InvalidArgument,
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
