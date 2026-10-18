//! Builder for [`TunnelHandle`] — the main entry point for the SDK.

use std::path::PathBuf;

use tokio::sync::broadcast;

use crate::binary;
use crate::cleanup::CleanupReport;
use crate::client::{CloudflareApi, OriginRequest};
use crate::error::{Error, Result};
use crate::event::Event;
use crate::handle::Shutdown;
use crate::protocol::{validate_domain, validate_subdomain, Protocol};
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
/// let mut handle = TunnelBuilder::new()
///     .token(std::env::var("CLOUDFLARE_API_TOKEN")?)
///     .domain("example.com")
///     .protocol(Protocol::Http)
///     .port(8080)
///     .subdomain("myapp")
///     .start()
///     .await?;
/// println!("{}", handle.url());
/// tokio::select! {
///     _ = tokio::signal::ctrl_c() => handle.stop().await?,
///     _ = handle.wait() => {}
/// }
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
pub struct TunnelBuilder {
    token: Option<String>,
    api: Option<CloudflareApi>,
    account_id: Option<String>,
    zone_id: Option<String>,
    domain: Option<String>,
    protocol: Protocol,
    port: u16,
    subdomain: Option<String>,
    binary_path: Option<PathBuf>,
    origin_request: Option<OriginRequest>,
    config_dir: Option<PathBuf>,
    github_proxy: Option<String>,
    auto_restart: bool,
    restart_policy: crate::session::RestartPolicy,
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
            api: None,
            account_id: None,
            zone_id: None,
            domain: None,
            protocol: Protocol::Http,
            port: DEFAULT_PORT,
            subdomain: None,
            binary_path: None,
            origin_request: None,
            config_dir: None,
            github_proxy: None,
            auto_restart: false,
            restart_policy: crate::session::RestartPolicy::default(),
        }
    }

    /// Sets the Cloudflare API token.
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Injects a pre-built [`CloudflareApi`] client.
    ///
    /// Most callers should use [`token`](Self::token) — the SDK will build
    /// a `CloudflareApi` from it. This entry point is for advanced cases
    /// (custom base URL, shared client, integration tests against a mock
    /// API). When set, [`token`](Self::token) is ignored.
    pub fn api(mut self, api: CloudflareApi) -> Self {
        self.api = Some(api);
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
    /// **Optional, and combinable with any other subset.** Supply it
    /// alone and the SDK picks the account's first accessible zone; supply
    /// it with a `domain` and the lookup is verified against it. Useful when
    /// you've cached the id (e.g. from your own config store) and want to
    /// skip a round-trip.
    ///
    /// See [`resolve_zone`](Self::resolve_zone) semantics in the type docs.
    pub fn account(mut self, account_id: impl Into<String>) -> Self {
        self.account_id = Some(account_id.into());
        self
    }

    /// Sets the Cloudflare zone ID for the base domain.
    ///
    /// **Optional, and combinable with any other subset.** Supply it alone
    /// and the SDK reads the domain and account back from Cloudflare. See
    /// [`account`](Self::account).
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

    /// Skips TLS verification when the origin is `https://` with a
    /// self-signed certificate.
    ///
    /// This is what makes exposing a local dev server over HTTPS possible
    /// at all: `cloudflared` validates the origin certificate, and a
    /// `localhost` dev server almost never has a CA-signed one.
    pub fn no_tls_verify(mut self, enabled: bool) -> Self {
        let origin = self
            .origin_request
            .get_or_insert_with(OriginRequest::default);
        origin.no_tls_verify = Some(enabled);
        self
    }

    /// Sends `Host: <value>` to the origin instead of the public hostname.
    ///
    /// Needed whenever the local service routes on its own hostname — a
    /// virtual-host setup such as `*.test` or a framework that validates
    /// the `Host` header.
    pub fn origin_host_header(mut self, value: impl Into<String>) -> Self {
        self.origin_request
            .get_or_insert_with(OriginRequest::default)
            .http_host_header = Some(value.into());
        self
    }

    /// Overrides the origin TCP connect timeout, in seconds.
    pub fn origin_connect_timeout(mut self, seconds: u64) -> Self {
        self.origin_request
            .get_or_insert_with(OriginRequest::default)
            .connect_timeout = Some(seconds);
        self
    }

    /// Disables HTTP/2 to the origin.
    ///
    /// `cloudflared` negotiates h2 by default; a few dev servers (older
    /// Python/Node handlers, some gRPC-over-plaintext setups) only speak
    /// HTTP/1.1 and fail the connection outright rather than negotiating
    /// down.
    pub fn http2_origin(mut self, enabled: bool) -> Self {
        self.origin_request
            .get_or_insert_with(OriginRequest::default)
            .http2_origin = Some(enabled);
        self
    }

    /// Sets the complete origin configuration at once, overriding any
    /// individual [`no_tls_verify`](Self::no_tls_verify) /
    /// [`origin_host_header`](Self::origin_host_header) / … calls.
    pub fn origin_request(mut self, request: OriginRequest) -> Self {
        self.origin_request = Some(request);
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

    /// When `true`, the SDK automatically respawns `cloudflared` on the
    /// same public URL if it exits on its own (crash, network drop, etc.).
    ///
    /// Each respawn creates a fresh Cloudflare tunnel object and DNS
    /// record under the same subdomain, so the public hostname is stable
    /// from the user's point of view. A user-initiated
    /// [`crate::TunnelHandle::stop`] always terminates the session
    /// regardless of this setting.
    ///
    /// Defaults to `false` at the SDK level so embedding programs get
    /// predictable lifetime semantics; the `cfp` CLI enables it by
    /// default.
    pub fn auto_restart(mut self, enabled: bool) -> Self {
        self.auto_restart = enabled;
        self
    }

    /// Resolves the zone this token can reach and hands it to
    /// [`cleanup_zone`](Self::cleanup_zone).
    ///
    /// The convenience counterpart to the free functions
    /// [`cleanup`]/[`preview_cleanup`], which need the account, zone and
    /// domain spelled out.
    pub async fn cleanup_zone(&self) -> Result<CleanupReport> {
        let api = match &self.api {
            Some(api) => api.clone(),
            None => CloudflareApi::new(
                self.token
                    .clone()
                    .ok_or(Error::MissingCredential("Cloudflare API token"))?,
            )?,
        };
        let (account, zone, domain) = resolve_zone(
            &api,
            self.account_id.clone(),
            self.zone_id.clone(),
            self.domain.clone(),
        )
        .await?;
        self.cleanup_zone_with(&api, &account, &zone, &domain).await
    }

    /// Reclaims dead tunnels and orphaned DNS in one zone, without deleting
    /// anything.
    pub async fn preview_zone(&self) -> Result<CleanupReport> {
        let api = match &self.api {
            Some(api) => api.clone(),
            None => CloudflareApi::new(
                self.token
                    .clone()
                    .ok_or(Error::MissingCredential("Cloudflare API token"))?,
            )?,
        };
        let (account, zone, domain) = resolve_zone(
            &api,
            self.account_id.clone(),
            self.zone_id.clone(),
            self.domain.clone(),
        )
        .await?;
        crate::cleanup::preview_cleanup(&api, &account, &zone, &domain).await
    }

    /// Reclaims dead tunnels and orphaned DNS in one zone, given coordinates
    /// the caller already has.
    pub async fn cleanup_zone_with(
        &self,
        api: &CloudflareApi,
        account: &str,
        zone: &str,
        domain: &str,
    ) -> Result<CleanupReport> {
        crate::cleanup::cleanup_with_report(api, account, zone, domain).await
    }

    /// Overrides how aggressively the supervisor reconnects a crashed
    /// tunnel. Only consulted when [`auto_restart`](Self::auto_restart)
    /// is enabled.
    ///
    /// The defaults (5 attempts, 500 ms doubling to a 8 s cap) suit a
    /// developer exposing a dev server. Widen `max_delay` for a
    /// long-lived tunnel that should ride out a Cloudflare incident.
    pub fn restart_policy(mut self, policy: crate::session::RestartPolicy) -> Self {
        self.restart_policy = policy;
        self
    }

    /// Validates that all required credentials are present. Mainly useful
    /// for dry-run preflight checks.
    ///
    /// Either [`token`](Self::token) or [`api`](Self::api) must be set.
    /// `account` / `zone` / `domain` can be discovered automatically by
    /// `start()`.
    pub fn validate(&self) -> Result<()> {
        if self.token.is_none() && self.api.is_none() {
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
            completed: false,
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
///
/// If [`StartedBuilder::start`] fails, the `Drop` impl aborts the event
/// dispatch task that `start` spawned — otherwise the task would outlive
/// the builder, hold the broadcast sender alive and never terminate.
#[must_use = "call .start() to launch the tunnel"]
pub struct StartedBuilder<F> {
    inner: TunnelBuilder,
    handler: Option<F>,
    dispatch: DispatchSlot,
    completed: bool,
}

impl<F> Drop for StartedBuilder<F> {
    fn drop(&mut self) {
        if !self.completed {
            self.dispatch.abort();
        }
    }
}

impl<F> StartedBuilder<F>
where
    F: Fn(Event) + Send + 'static,
{
    /// Launches the tunnel.
    pub async fn start(mut self) -> Result<crate::TunnelHandle> {
        // Resolve and validate the subdomain first. It is the cheapest
        // possible failure (pure local check) and validating it after
        // zone discovery / `cloudflared` download would mean a typo costs
        // the user an API round-trip and a ~40 MB download before the
        // error surfaces.
        let requested = self.inner.subdomain.take().filter(|s| !s.is_empty());
        let subdomain = match &requested {
            Some(s) => s.to_ascii_lowercase(),
            None => crate::session::random_subdomain(),
        };
        validate_subdomain(&subdomain)
            .map_err(|e| Error::InvalidSubdomain(format!("\"{}\": {}", e.value, e.reason)))?;

        // The base domain lands in the public hostname, so it gets the
        // same up-front treatment — before any API call or download.
        if let Some(domain) = &self.inner.domain {
            let trimmed = domain.trim().trim_end_matches('.').to_ascii_lowercase();
            validate_domain(&trimmed)?;
            self.inner.domain = Some(trimmed);
        }

        let api = match self.inner.api.take() {
            Some(api) => api,
            None => {
                let token = self
                    .inner
                    .token
                    .clone()
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .ok_or(Error::MissingCredential("Cloudflare API token"))?;
                CloudflareApi::new(token)?
            }
        };

        // Resolve account/zone/domain. When all three are provided, skip
        // discovery. Otherwise ask Cloudflare to fill the gaps:
        //
        // - domain set, account/zone missing → find_zone(domain).
        // - domain missing → list_zones(); if exactly one, use it; if
        //   multiple, surface a clear error so the caller can pick.
        let (account_id, zone_id, domain) = resolve_zone(
            &api,
            self.inner.account_id.take(),
            self.inner.zone_id.take(),
            self.inner.domain.take(),
        )
        .await?;

        // A *generated* name that happens to be taken just gets another
        // draw — the user never chose it, so failing the whole start would
        // be gratuitous. An explicitly requested name is never renamed:
        // silently serving a different URL than the one asked for would be
        // worse than an error.
        let subdomain = match requested {
            Some(_) => subdomain,
            None => pick_available_subdomain(&api, &account_id, &zone_id, &domain).await?,
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
            origin_request: self.inner.origin_request.take(),
            event_tx,
            shutdown: Shutdown::new(),
            dispatch: self.dispatch.clone(),
            auto_restart: self.inner.auto_restart,
            restart_policy: self.inner.restart_policy,
        };

        let handle = session::start(cfg).await?;
        // Ownership of the dispatch task transfers to `TunnelHandle`;
        // suppress the `Drop` abort so the handler keeps running.
        self.completed = true;
        Ok(handle)
    }
}

/// Draws random subdomains until one is free.
///
/// Bounded by [`MAX_SUBDOMAIN_ATTEMPTS`]; if every draw collides (a zone
/// already carrying thousands of tunnels) the last name is returned so the
/// caller reports the normal `SubdomainInUse` error rather than looping.
async fn pick_available_subdomain(
    api: &CloudflareApi,
    account_id: &str,
    zone_id: &str,
    domain: &str,
) -> Result<String> {
    let mut candidate = crate::session::random_subdomain();
    for _ in 1..MAX_SUBDOMAIN_ATTEMPTS {
        if api
            .subdomain_available(account_id, zone_id, &format!("{candidate}.{domain}"))
            .await?
        {
            return Ok(candidate);
        }
        candidate = crate::session::random_subdomain();
    }
    Ok(candidate)
}

/// How many names a generated subdomain may try before giving up.
const MAX_SUBDOMAIN_ATTEMPTS: u32 = 5;

/// Fills in whatever the caller left out, by asking the Cloudflare API.
///
/// Every partial combination is handled — the docs on
/// [`TunnelBuilder::account`] and [`TunnelBuilder::zone`] describe both as
/// optional overrides, which the previous all-or-nothing `match` did not
/// deliver: supplying two of the three values returned an error telling
/// you to supply all three.
///
/// | supplied | how the rest is found |
/// |---|---|
/// | all three | used verbatim, no API call |
/// | `zone` only | `GET /zones/{id}` gives the domain and account |
/// | `domain` only | [`CloudflareApi::find_zone`] gives zone + account |
/// | `account` only | the first zone that account owns |
/// | `account` + `domain` | the zone is looked up, and its account checked |
/// | `zone` + `account` | the zone is looked up for the domain |
/// | none | the only zone, or an error listing the choices |
/// | `domain` + nothing else, not accessible | error naming the domain |
async fn resolve_zone(
    api: &CloudflareApi,
    account_id: Option<String>,
    zone_id: Option<String>,
    domain: Option<String>,
) -> Result<(String, String, String)> {
    // Fast path: nothing to discover.
    if let (Some(account), Some(zone), Some(domain)) = (&account_id, &zone_id, &domain) {
        return Ok((account.clone(), zone.clone(), domain.clone()));
    }

    // A domain is the strongest key: it identifies exactly one zone.
    if let Some(domain) = domain {
        let zone = api.find_zone(&domain).await?.ok_or_else(|| {
            Error::Other(anyhow::anyhow!(
                "domain \"{domain}\" is not accessible with this token — \
                     run `cfp domains` to list what you can reach"
            ))
        })?;
        let zone_account = zone.require_account_id()?.to_string();
        if let Some(expected) = &account_id {
            if *expected != zone_account {
                return Err(Error::Other(anyhow::anyhow!(
                    "domain \"{domain}\" belongs to account {zone_account}, \
                     not {expected}"
                )));
            }
        }
        return Ok((zone_account, zone.id, zone.name));
    }

    // A zone id is the next strongest key.
    if let Some(zone_id) = zone_id {
        let zone = api.find_zone_by_id(&zone_id).await?.ok_or_else(|| {
            Error::Other(anyhow::anyhow!(
                "zone {zone_id} is not accessible with this token"
            ))
        })?;
        let zone_account = zone.require_account_id()?.to_string();
        if let Some(expected) = &account_id {
            if *expected != zone_account {
                return Err(Error::Other(anyhow::anyhow!(
                    "zone {zone_id} belongs to account {zone_account}, not {expected}"
                )));
            }
        }
        return Ok((zone_account, zone.id, zone.name));
    }

    // An account alone: the first zone it owns.
    if let Some(account) = account_id {
        let zones = api.list_zones_in_account(&account).await?;
        return match zones.len() {
            0 => Err(Error::Other(anyhow::anyhow!(
                "account {account} owns no zones accessible to this token"
            ))),
            1 => {
                let z = &zones[0];
                let z_account = z.require_account_id()?.to_string();
                Ok((z_account, z.id.clone(), z.name.clone()))
            }
            n => Err(Error::Other(anyhow::anyhow!(
                "account {account} owns {n} accessible zones — pass \
                 .domain(\"...\") to choose one"
            ))),
        };
    }

    // Nothing supplied: the single-zone case is the common one.
    let zones = api.list_zones().await?;
    match zones.len() {
        0 => Err(Error::Other(anyhow::anyhow!(
            "token is valid but has access to no zones"
        ))),
        1 => {
            let z = &zones[0];
            let account = z.require_account_id()?.to_string();
            Ok((account, z.id.clone(), z.name.clone()))
        }
        n => Err(Error::Other(anyhow::anyhow!(
            "token has access to {n} zones — pass .domain(\"...\") to choose one, \
             e.g. .domain(\"example.com\")"
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
