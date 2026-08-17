//! Builder for [`TunnelHandle`] — the main entry point for the SDK.

use std::path::PathBuf;

use tokio::sync::broadcast;

use crate::binary;
use crate::client::CloudflareApi;
use crate::error::{Error, Result};
use crate::event::Event;
use crate::handle::Shutdown;
use crate::protocol::Protocol;
use crate::session::{self, DispatchSlot, SessionConfig};

/// Default local port when none is supplied. Matches the CLI default.
pub const DEFAULT_PORT: u16 = 8080;

/// Default broadcast channel buffer size for events.
const EVENT_BUFFER: usize = 64;

/// Configures and starts a tunnel. Construct with [`TunnelBuilder::new`],
/// chain configuration methods, then call [`TunnelBuilder::start`].
///
/// Only the **token** is strictly required. `account`, `zone` and `domain`
/// are discovered from the Cloudflare API on `start()`:
/// - if `domain` is set, the matching zone is looked up;
/// - if nothing is set, the first accessible zone is used (single-zone
///   tokens) or an error is returned asking you to disambiguate.
///
/// # Example
///
/// ```no_run
/// use cloudpipe_sdk::{Protocol, TunnelBuilder};
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let handle = TunnelBuilder::new()
///     .token(std::env::var("CLOUDFLARE_API_TOKEN")?)
///     .domain("example.com")
///     .protocol(Protocol::Http)
///     .port(8080)
///     .subdomain("myapp")
///     .start()
///     .await?;
/// println!("{}", handle.url());
/// handle.wait().await;
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
pub struct TunnelBuilder {
    token: Option<String>,
    account_id: Option<String>,
    zone_id: Option<String>,
    domain: Option<String>,
    protocol: Protocol,
    port: u16,
    subdomain: Option<String>,
    binary_path: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    github_proxy: Option<String>,
}

impl Default for TunnelBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TunnelBuilder {
    /// Creates a builder with sensible defaults: HTTP protocol on port 8080
    /// and a randomly generated subdomain.
    pub fn new() -> Self {
        Self {
            token: None,
            account_id: None,
            zone_id: None,
            domain: None,
            protocol: Protocol::Http,
            port: DEFAULT_PORT,
            subdomain: None,
            binary_path: None,
            config_dir: None,
            github_proxy: None,
        }
    }

    /// Sets the Cloudflare API token.
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Sets the base domain (e.g. `example.com`).
    ///
    /// The SDK uses this to look up the matching zone at `start()` time.
    /// If the token has access to exactly one zone, you can skip this and
    /// the SDK will auto-pick it.
    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Sets the Cloudflare account ID.
    ///
    /// **Optional.** Override the value the SDK auto-discovers from the
    /// token — useful when you've cached it (e.g. read from your own
    /// config store) and want to skip the `list_zones` round-trip.
    pub fn account(mut self, account_id: impl Into<String>) -> Self {
        self.account_id = Some(account_id.into());
        self
    }

    /// Sets the Cloudflare zone ID for the base domain.
    ///
    /// **Optional.** See [`account`](Self::account) for why you'd supply
    /// this manually.
    pub fn zone(mut self, zone_id: impl Into<String>) -> Self {
        self.zone_id = Some(zone_id.into());
        self
    }

    /// Sets the local service protocol.
    pub fn protocol(mut self, protocol: Protocol) -> Self {
        self.protocol = protocol;
        self
    }

    /// Sets the local port to expose.
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Sets a specific subdomain. If unset, a random one is generated.
    pub fn subdomain(mut self, subdomain: impl Into<String>) -> Self {
        self.subdomain = Some(subdomain.into());
        self
    }

    /// Use a pre-installed `cloudflared` binary at this exact path.
    pub fn cloudflared_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.binary_path = Some(path.into());
        self
    }

    /// Root directory for SDK-managed files (where `cloudflared` is
    /// downloaded to when not found on `PATH`). Defaults to `~/.cfp`.
    pub fn config_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.config_dir = Some(dir.into());
        self
    }

    /// Override the GitHub mirror prefix used when downloading `cloudflared`.
    /// Pass an empty string to disable mirroring.
    pub fn github_proxy(mut self, proxy: impl Into<String>) -> Self {
        self.github_proxy = Some(proxy.into());
        self
    }

    /// Validates that all required credentials are present. Mainly useful
    /// for dry-run preflight checks.
    ///
    /// Only `token` is strictly required — `account` / `zone` / `domain`
    /// can be discovered automatically by `start()`.
    pub fn validate(&self) -> Result<()> {
        if self.token.is_none() {
            return Err(Error::MissingCredential("Cloudflare API token"));
        }
        Ok(())
    }

    /// Starts the tunnel with an event-handling closure.
    pub fn on_event<F>(self, on_event: F) -> StartedBuilder<F>
    where
        F: Fn(Event) + Send + 'static,
    {
        StartedBuilder {
            inner: self,
            handler: Some(on_event),
            dispatch: DispatchSlot::default(),
        }
    }

    /// Starts the tunnel with no event handler. Shortcut for `on_event(|_| {})`.
    pub async fn start(self) -> Result<crate::TunnelHandle> {
        self.on_event(|_| {}).start().await
    }
}

/// Intermediate builder returned by [`TunnelBuilder::on_event`]. Holds the
/// user-supplied closure plus a dispatch slot that the resulting
/// [`crate::TunnelHandle`] will abort on `Drop`.
#[must_use = "call .start() to launch the tunnel"]
pub struct StartedBuilder<F> {
    inner: TunnelBuilder,
    handler: Option<F>,
    dispatch: DispatchSlot,
}

impl<F> StartedBuilder<F>
where
    F: Fn(Event) + Send + 'static,
{
    /// Launches the tunnel.
    pub async fn start(mut self) -> Result<crate::TunnelHandle> {
        let token = self
            .inner
            .token
            .clone()
            .ok_or(Error::MissingCredential("Cloudflare API token"))?;
        let api = CloudflareApi::new(token)?;

        // Resolve account/zone/domain. When all three are provided, skip
        // discovery. Otherwise ask Cloudflare to fill the gaps:
        //
        // - domain set, account/zone missing → find_zone(domain).
        // - domain missing → list_zones(); if exactly one, use it; if
        //   multiple, surface a clear error so the caller can pick.
        let (account_id, zone_id, domain) = resolve_zone(&api, self.inner.account_id, self.inner.zone_id, self.inner.domain).await?;

        let subdomain = match self.inner.subdomain.take() {
            Some(s) if !s.is_empty() => s.to_ascii_lowercase(),
            _ => crate::session::random_subdomain(),
        };

        let binary_path = match self.inner.binary_path.take() {
            Some(p) => p,
            None => {
                binary::ensure_installed(
                    self.inner.config_dir.as_deref(),
                    self.inner.github_proxy.as_deref(),
                )
                .await?
            }
        };

        // Spawn the user-supplied event handler on its own task. The
        // session will own the dispatch slot and abort the task on drop.
        let (event_tx, _event_rx_keepalive) = broadcast::channel::<Event>(EVENT_BUFFER);
        if let Some(handler) = self.handler.take() {
            let mut rx = event_tx.subscribe();
            let task = tokio::spawn(async move {
                while let Ok(event) = rx.recv().await {
                    handler(event);
                }
            });
            self.dispatch.install(task);
        }

        let cfg = SessionConfig {
            api,
            account_id,
            zone_id,
            domain,
            protocol: self.inner.protocol,
            port: self.inner.port,
            subdomain,
            binary_path,
            event_tx,
            shutdown: Shutdown::new(),
            dispatch: self.dispatch,
        };

        session::start(cfg).await
    }
}

/// Fills in any missing zone/account/domain by asking the Cloudflare API.
///
/// - If all three are provided, returns them verbatim.
/// - If only `domain` is provided, looks it up via `find_zone`.
/// - If neither is provided, lists the token's accessible zones and picks
///   the only one if there is exactly one; otherwise returns a descriptive
///   error so the caller can disambiguate.
async fn resolve_zone(
    api: &CloudflareApi,
    account_id: Option<String>,
    zone_id: Option<String>,
    domain: Option<String>,
) -> Result<(String, String, String)> {
    match (account_id, zone_id, domain) {
        (Some(a), Some(z), Some(d)) => Ok((a, z, d)),
        (Some(_), Some(_), None) => Err(Error::Other(anyhow::anyhow!(
            "account and zone provided but domain is missing — pass .domain(...)"
        ))),
        (None, None, Some(d)) => {
            let zone = api
                .find_zone(&d)
                .await?
                .ok_or_else(|| Error::Other(anyhow::anyhow!(
                    "domain \"{d}\" is not accessible with this token"
                )))?;
            Ok((zone.account.id, zone.id, zone.name))
        }
        (None, None, None) => {
            let zones = api.list_zones().await?;
            match zones.len() {
                0 => Err(Error::Other(anyhow::anyhow!(
                    "token is valid but has access to no zones"
                ))),
                1 => {
                    let z = zones.into_iter().next().unwrap();
                    Ok((z.account.id, z.id, z.name))
                }
                n => Err(Error::Other(anyhow::anyhow!(
                    "token has access to {n} zones — pass .domain(\"...\"), \
                     .account(...), and .zone(...) to disambiguate"
                ))),
            }
        }
        // Partial combinations — surface a friendly hint.
        _ => Err(Error::Other(anyhow::anyhow!(
            "account, zone and domain must be supplied together; \
             pass .domain(\"...\") alone to auto-discover the rest"
        ))),
    }
}

impl<F: Fn(Event) + Send + 'static> std::fmt::Debug for StartedBuilder<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartedBuilder")
            .field("inner", &self.inner)
            .field("handler", &"<closure>")
            .finish_non_exhaustive()
    }
}
