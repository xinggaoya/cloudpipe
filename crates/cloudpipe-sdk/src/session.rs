//! Tunnel lifecycle orchestration — UI-agnostic core used by the builder.
//!
//! Owns the Cloudflare session state (tunnel id, DNS, child process) and the
//! shutdown signal. Exposes [`start`] which runs the setup phase and returns
//! a [`TunnelHandle`] ready for the caller to await / stop.
//!
//! ## Auto-restart
//!
//! When [`SessionConfig::auto_restart`] is set, the background task keeps
//! the tunnel alive across `cloudflared` crashes: each respawn creates a
//! fresh Cloudflare tunnel object and DNS record under the **same
//! subdomain**, so the public hostname is stable from the caller's point
//! of view. A user-initiated [`crate::TunnelHandle::stop`] always
//! terminates the session regardless of the auto-restart setting.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::protocol::validate_subdomain;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::binary::{self, LineKind};
use crate::client::{CloudflareApi, IngressEntry, OriginRequest};
use crate::error::{Error, Result};
use crate::event::{Event, LogLevel, ShutdownReason};
use crate::handle::{Shutdown, TunnelHandle};
use crate::protocol::Protocol;

/// How hard the supervisor tries to bring a crashed tunnel back.
///
/// Exposed as a value rather than hard-coded constants so (a) integration
/// tests can run the whole retry ladder in milliseconds instead of tens of
/// seconds, and (b) an embedder with different tolerance — a long-lived
/// production tunnel, say — can widen the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Consecutive bootstrap failures tolerated before giving up.
    pub max_attempts: u32,
    /// Delay before the first retry; doubles each failure, capped at
    /// [`max_delay`](Self::max_delay).
    pub base_delay: Duration,
    /// Upper bound on any single retry delay.
    pub max_delay: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            // A bootstrap failure usually means Cloudflare is rate-limiting
            // the token or `cloudflared` is unusable; retrying forever
            // would just burn API quota.
            max_attempts: 5,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        }
    }
}

impl RestartPolicy {
    /// Delay before retry number `attempt` (1-based), with jitter so a
    /// fleet of clients does not reconnect in lockstep against one quota.
    pub fn delay(&self, attempt: u32) -> Duration {
        let base = self.base_delay.as_millis() as u64;
        let cap = self.max_delay.as_millis() as u64;
        let ceiling = base
            .saturating_mul(1u64 << attempt.saturating_sub(1).min(16))
            .min(cap);
        // 50% base + up to 50% jitter.
        let jitter = (ceiling / 2)
            * ((attempt as u64).wrapping_mul(2_654_435_761) % 1_000)
            / 1_000;
        Duration::from_millis(ceiling / 2 + jitter)
    }
}

/// Generates a random subdomain (`user-1234`).
pub fn random_subdomain() -> String {
    let seed = RandomState::new().build_hasher().finish() as u32;
    format!("user-{:04}", seed % 10000)
}

/// Holds the user-supplied event dispatch task. The handle aborts it on Drop.
#[derive(Clone, Default)]
pub(crate) struct DispatchSlot(std::sync::Arc<std::sync::Mutex<Option<JoinHandle<()>>>>);

impl std::fmt::Debug for DispatchSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("DispatchSlot").finish()
    }
}

impl DispatchSlot {
    pub(crate) fn install(&self, handle: JoinHandle<()>) {
        let mut guard = self.0.lock().expect("dispatch slot poisoned");
        if let Some(prev) = guard.take() {
            prev.abort();
        }
        *guard = Some(handle);
    }

    pub(crate) fn abort(&self) {
        let mut guard = self.0.lock().expect("dispatch slot poisoned");
        if let Some(task) = guard.take() {
            task.abort();
        }
    }
}

/// Pre-built configuration for [`start`]. Constructed by the builder.
#[derive(Debug)]
pub(crate) struct SessionConfig {
    pub api: CloudflareApi,
    pub account_id: String,
    pub zone_id: String,
    pub domain: String,
    pub protocol: Protocol,
    pub port: u16,
    pub subdomain: String,
    pub binary_path: PathBuf,
    /// Origin-side tuning applied to the hostname ingress rule.
    pub origin_request: Option<OriginRequest>,
    pub event_tx: broadcast::Sender<Event>,
    pub shutdown: Shutdown,
    pub dispatch: DispatchSlot,
    pub auto_restart: bool,
    pub restart_policy: RestartPolicy,
}

/// Cloudflare-side coordinates of a running session. The `tunnel_id` is
/// rewritten in-place whenever the auto-restart loop creates a fresh
/// tunnel, so a [`crate::TunnelHandle::stop`] call always cleans up the
/// tunnel that's actually live.
///
/// `cleaned` is a shared [`AtomicBool`] rather than a plain `bool` so that
/// [`SessionState::purge`] can claim the cleanup exactly once through an
/// interior-mutability swap. That lets both the supervisor task and
/// `TunnelHandle::stop`'s belt-and-suspenders pass run their purge
/// concurrently without either of them holding a lock across `.await`,
/// and without duplicating the API round-trips.
#[derive(Debug)]
pub(crate) struct SessionState {
    pub api: CloudflareApi,
    pub account_id: String,
    pub zone_id: String,
    pub full_name: String,
    pub tunnel_id: String,
    cleaned: Arc<AtomicBool>,
}

/// `AtomicBool` is not `Clone`, so a snapshot is taken by reading the flag
/// rather than sharing it. A snapshot is only used to reach
/// [`SessionState::purge`], which is idempotent, so a slightly stale
/// `cleaned` reading is harmless — worst case two callers both perform the
/// round-trips.
impl Clone for SessionState {
    fn clone(&self) -> Self {
        // The flag is shared, not copied: a snapshot taken by
        // `TunnelHandle::stop` must observe (and be observed by) the
        // supervisor's own purge, otherwise both run the same three
        // round-trips against the user's rate-limit quota.
        Self {
            api: self.api.clone(),
            account_id: self.account_id.clone(),
            zone_id: self.zone_id.clone(),
            full_name: self.full_name.clone(),
            tunnel_id: self.tunnel_id.clone(),
            cleaned: Arc::clone(&self.cleaned),
        }
    }
}

impl SessionState {
    /// Releases every Cloudflare-side resource this session created:
    /// the CNAME record, the tunnel's connections, then the tunnel
    /// itself.
    ///
    /// Idempotent — the first caller to flip `cleaned` wins and performs
    /// the round-trips; every later call returns immediately. Takes
    /// `&self` so it never needs to be called under a guard held across
    /// an `.await` point.
    pub(crate) async fn purge(&self) {
        if self.cleaned.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(Some(record)) = self
            .api
            .find_dns_record(&self.zone_id, &self.full_name)
            .await
        {
            let _ = self
                .api
                .delete_dns_record(&self.zone_id, &record.id)
                .await;
        }
        let _ = self
            .api
            .cleanup_connections(&self.account_id, &self.tunnel_id)
            .await;
        let _ = self
            .api
            .delete_tunnel(&self.account_id, &self.tunnel_id)
            .await;
    }
}

/// Tracks the Cloudflare resources a bootstrap attempt has created so a
/// failure at any later step can unwind them.
///
/// Without this, a `cloudflared` spawn failure left both a tunnel and a
/// DNS record orphaned on the account, only reclaimable by a later
/// `cfp cleanup` run.
#[derive(Debug, Default)]
struct Created {
    tunnel: Option<String>,
    dns: Option<String>,
}

impl Created {
    /// Undoes every recorded creation, newest first. Failures are
    /// swallowed — cleanup is best-effort by design.
    async fn rollback(self, api: &CloudflareApi, account_id: &str, zone_id: &str) {
        if let Some(record_id) = self.dns {
            if let Err(err) = api.delete_dns_record(zone_id, &record_id).await {
                warn!("rollback: delete DNS {record_id} failed: {err:#}");
            }
        }
        if let Some(tunnel_id) = self.tunnel {
            let _ = api.cleanup_connections(account_id, &tunnel_id).await;
            if let Err(err) = api.delete_tunnel(account_id, &tunnel_id).await {
                warn!("rollback: delete tunnel {tunnel_id} failed: {err:#}");
            }
        }
    }
}

/// Output of a single bootstrap attempt: a running `cloudflared` plus the
/// state and tasks the supervisor task needs to drive it.
struct Bootstrap {
    state: SessionState,
    child: tokio::process::Child,
    stderr_task: JoinHandle<()>,
}

/// Boots a tunnel end-to-end on first call, then loops forever (until the
/// user calls [`TunnelHandle::stop`]) respawning on `cloudflared` crashes
/// when `auto_restart` is enabled.
pub(crate) async fn start(cfg: SessionConfig) -> Result<TunnelHandle> {
    validate_subdomain(&cfg.subdomain)
        .map_err(|e| Error::InvalidSubdomain(format!("\"{}\": {}", e.value, e.reason)))?;
    let full_name = format!("{}.{}", cfg.subdomain, cfg.domain);

    // The very first bootstrap emits the public-facing setup events so
    // the UI can render them. Respawns below use a quieter path that only
    // emits Restarting/Restarted.
    let initial = bootstrap_session(&cfg, &full_name, true).await?;

    // The shared session state. Respawns rewrite `state.tunnel_id` so
    // belt-and-suspenders cleanup in TunnelHandle::stop targets the live
    // tunnel, not a stale one from a previous generation.
    let state = Arc::new(Mutex::new(initial.state));

    // Stderr tasks and connection counters are replaced on every respawn.
    // The handle keeps an Arc<Mutex<...>> slot it can read to discover the
    // current generation's stderr pump task.
    let stderr_slot: crate::handle::StderrSlot = Arc::new(std::sync::Mutex::new(Some(
        initial.stderr_task,
    )));

    let state_for_task = Arc::clone(&state);
    let shutdown_for_task = cfg.shutdown.clone();
    let event_tx_for_task = cfg.event_tx.clone();
    let auto_restart = cfg.auto_restart;
    let full_name_for_task = full_name.clone();
    let event_tx_for_send = event_tx_for_task.clone();
    let stderr_slot_for_task = Arc::clone(&stderr_slot);
    let initial_child = initial.child;

    // The respawn path reuses the caller's `SessionConfig` verbatim,
    // including `domain`. Deriving the domain from `full_name` by
    // splitting on the last dot silently yields the TLD ("com") instead of
    // "example.com", so the value is carried across as-is.
    let respawn_cfg_template = SessionConfig {
        api: cfg.api.clone(),
        account_id: cfg.account_id.clone(),
        zone_id: cfg.zone_id.clone(),
        domain: cfg.domain.clone(),
        protocol: cfg.protocol,
        port: cfg.port,
        subdomain: cfg.subdomain.clone(),
        binary_path: cfg.binary_path.clone(),
        origin_request: cfg.origin_request.clone(),
        event_tx: cfg.event_tx.clone(),
        shutdown: cfg.shutdown.clone(),
        dispatch: cfg.dispatch.clone(),
        auto_restart: cfg.auto_restart,
        restart_policy: cfg.restart_policy,
    };

    let task: JoinHandle<()> = tokio::spawn(async move {
        Supervisor {
            child: initial_child,
            state: state_for_task,
            stderr_slot: stderr_slot_for_task,
            shutdown: shutdown_for_task,
            event_tx: event_tx_for_task.clone(),
            cfg: respawn_cfg_template,
            full_name: full_name_for_task,
            auto_restart,
        }
        .run()
        .await;

        let _ = event_tx_for_send.send(Event::Cleaned);
        debug!("supervisor: task done");
    });
    debug!("supervisor: spawned");

    let events_rx = cfg.event_tx.subscribe();
    Ok(TunnelHandle::new(
        state,
        cfg.shutdown,
        events_rx,
        full_name,
        cfg.subdomain,
        task,
        stderr_slot,
        cfg.dispatch.clone(),
    ))
}

/// Everything the supervisor task needs, bundled so the loop body stays
/// readable. Built once at the start of the task and never reallocated.
struct Supervisor {
    child: tokio::process::Child,
    state: Arc<Mutex<SessionState>>,
    stderr_slot: crate::handle::StderrSlot,
    shutdown: Shutdown,
    event_tx: broadcast::Sender<Event>,
    cfg: SessionConfig,
    full_name: String,
    auto_restart: bool,
}

impl Supervisor {
    async fn run(mut self) {
        let mut attempt: u32 = 1;

        loop {
            // ---- Phase 1: a child is running; wait for it to end -------
            let reason = self.wait_for_exit().await;
            debug!(?reason, "supervisor: cloudflared generation ended");

            // Stop the process *before* touching Cloudflare. The old order
            // deleted the DNS record and the tunnel while `cloudflared` was
            // still serving, so every in-flight request failed and "clean
            // exit" was not clean.
            self.terminate_child().await;
            self.drain_stderr_pump();
            let snapshot = self.state.lock().await.clone();
            snapshot.purge().await;

            if matches!(reason, ShutdownReason::UserRequested) {
                let _ = self.event_tx.send(Event::ShuttingDown { reason });
                break;
            }

            let _ = self.event_tx.send(Event::ShuttingDown {
                reason: reason.clone(),
            });
            if !self.auto_restart {
                break;
            }

            attempt += 1;
            let _ = self
                .event_tx
                .send(Event::Restarting { reason, attempt });

            // ---- Phase 2: no child; retry the bootstrap ---------------
            match self.retry_bootstrap(attempt).await {
                Ok(()) => {
                    let _ = self.event_tx.send(Event::Restarted { attempt });
                }
                Err(err) => {
                    warn!(attempts = self.cfg.restart_policy.max_attempts, "{err:#}");
                    let _ = self.event_tx.send(Event::RestartGivingUp {
                        attempts: self.cfg.restart_policy.max_attempts,
                        last_error: format!("{err:#}"),
                    });
                    break;
                }
            }
        }
    }

    /// Retries `bootstrap_session` until it succeeds, the user shuts down,
    /// or the retry budget is spent.
    ///
    /// Owning the whole ladder here — rather than looping back through
    /// `wait_for_exit` with an already-dead child, as the previous version
    /// did — means a bootstrap failure can never emit a second spurious
    /// `ShuttingDown`, and there is exactly one place where backoff is
    /// applied.
    async fn retry_bootstrap(&mut self, attempt: u32) -> Result<()> {
        let policy = self.cfg.restart_policy;
        let mut failure = 0u32;

        loop {
            if self.shutdown.is_triggered() {
                return Ok(());
            }

            match bootstrap_session(&self.cfg, &self.full_name, false).await {
                Ok(next) => {
                    {
                        let mut guard = self.state.lock().await;
                        *guard = SessionState {
                            cleaned: Arc::new(AtomicBool::new(false)),
                            ..next.state.clone()
                        };
                    }
                    self.child = next.child;
                    {
                        let mut guard = self.stderr_slot.lock().expect("stderr slot poisoned");
                        *guard = Some(next.stderr_task);
                    }
                    return Ok(());
                }
                Err(err) => {
                    failure += 1;
                    if failure >= policy.max_attempts || policy.max_attempts == 0 {
                        return Err(err);
                    }
                    let message = format!("{err:#}");
                    warn!("reconnect attempt {failure} failed: {message}");
                    let _ = self.event_tx.send(Event::BootstrapFailed {
                        attempt,
                        error: message,
                    });

                    let delay = policy.delay(failure);
                    tokio::select! {
                        _ = self.shutdown.notified() => return Ok(()),
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
    }

    /// Waits until the current `cloudflared` exits or the user asks to stop.
    ///
    /// Driven by `child.wait()` raced against the shutdown notification —
    /// no polling interval, so an exit is observed the moment it happens.
    async fn wait_for_exit(&mut self) -> ShutdownReason {
        if self.shutdown.is_triggered() {
            return ShutdownReason::UserRequested;
        }
        tokio::select! {
            biased;
            _ = self.shutdown.notified() => ShutdownReason::UserRequested,
            status = self.child.wait() => match status {
                Ok(status) => {
                    debug!("cloudflared exited with {status:?}");
                    ShutdownReason::ChildExited
                }
                Err(err) => {
                    warn!("wait on child failed: {err}");
                    ShutdownReason::Error(err.to_string())
                }
            },
        }
    }

    /// Sends SIGTERM, then SIGKILL after a grace period, then reaps.
    ///
    /// `cloudflared` is spawned with `kill_on_drop(true)`, so without this
    /// explicit sequence it would only die before the DNS record was
    /// deleted.
    async fn terminate_child(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return; // already exited
        }
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            if let Err(err) = signal_term(pid as i32) {
                debug!("SIGTERM to cloudflared failed: {err}");
            }
        }
        match tokio::time::timeout(TERMINATE_GRACE, self.child.wait()).await {
            Ok(Ok(_)) => return,
            Ok(Err(err)) => warn!("reaping cloudflared failed: {err}"),
            Err(_) => debug!("cloudflared outlived the grace period; killing"),
        }
        let _ = self.child.start_kill();
    }

    fn drain_stderr_pump(&self) {
        if let Some(task) = crate::handle::take_pump(&self.stderr_slot) {
            task.abort();
        }
    }
}

/// Grace period between SIGTERM and SIGKILL on shutdown.
const TERMINATE_GRACE: Duration = Duration::from_secs(2);

#[cfg(unix)]
fn signal_term(pid: i32) -> std::io::Result<()> {
    // SAFETY: `kill` with SIGTERM is async-signal-safe and `pid` comes
    // from a `Child` this process spawned, so it is always a live,
    // child-owned pid while we hold the `Child`.
    unsafe {
        if libc_kill(pid, 15) == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

/// Runs one full bootstrap pass — resolve conflicts, create the tunnel,
/// write ingress, create the DNS record, spawn `cloudflared`, install the
/// stderr pump — and hands the live components back to the caller.
///
/// `is_initial` controls whether the public-facing setup events
/// ([`Event::ResolvingConflicts`], [`Event::CreatingTunnel`], etc.) are
/// emitted. Respawns emit only [`Event::Restarted`] instead, so the UI
/// doesn't repeat the setup banner on every respawn.
///
/// Every Cloudflare resource created here is registered with [`Created`]
/// as soon as it exists, so a failure in any later step (including the
/// `cloudflared` spawn) unwinds the earlier ones instead of orphaning a
/// tunnel or a DNS record on the account.
async fn bootstrap_session(
    cfg: &SessionConfig,
    full_name: &str,
    is_initial: bool,
) -> Result<Bootstrap> {
    let mut created = Created::default();
    let result = bootstrap_inner(cfg, full_name, is_initial, &mut created).await;
    if result.is_err() {
        // Every Cloudflare resource registered in `created` is unwound here.
        // The earlier implementation only guarded the spawn step, so a
        // DNS-creation failure still left a tunnel behind.
        created
            .rollback(&cfg.api, &cfg.account_id, &cfg.zone_id)
            .await;
    }
    result
}

/// The actual creation sequence. `created` is threaded through so any
/// `?` short-circuit leaves a record the caller can undo.
async fn bootstrap_inner(
    cfg: &SessionConfig,
    full_name: &str,
    is_initial: bool,
    created: &mut Created,
) -> Result<Bootstrap> {
    if is_initial {
        emit(&cfg.event_tx, Event::Banner);
        emit(&cfg.event_tx, Event::ResolvingConflicts);
    }

    resolve_conflicts(
        &cfg.api,
        &cfg.account_id,
        &cfg.zone_id,
        &cfg.subdomain,
        full_name,
    )
    .await?;

    if is_initial {
        emit(
            &cfg.event_tx,
            Event::CreatingTunnel {
                name: cfg.subdomain.clone(),
            },
        );
    }

    let tunnel = cfg
        .api
        .create_tunnel(&cfg.account_id, &cfg.subdomain)
        .await?;
    created.tunnel = Some(tunnel.id.clone());
    let tunnel_token = tunnel.token.clone().ok_or_else(|| {
        Error::Other(anyhow::anyhow!(
            "Cloudflare returned a tunnel without a token"
        ))
    })?;

    let ingress = vec![
        IngressEntry {
            hostname: Some(full_name.to_string()),
            service: cfg.protocol.local_service(cfg.port),
            origin_request: cfg.origin_request.clone(),
        },
        IngressEntry {
            hostname: None,
            service: "http_status:404".to_string(),
            origin_request: None,
        },
    ];
    cfg.api
        .set_tunnel_ingress(&cfg.account_id, &tunnel.id, &ingress)
        .await?;
    if is_initial {
        emit(
            &cfg.event_tx,
            Event::IngressConfigured {
                protocol: cfg.protocol,
                port: cfg.port,
            },
        );
    }

    let cname_target = format!("{}.cfargotunnel.com", tunnel.id);
    let record = cfg
        .api
        .create_dns_record(&cfg.zone_id, full_name, &cname_target)
        .await?;
    created.dns = Some(record.id.clone());
    if is_initial {
        emit(
            &cfg.event_tx,
            Event::DnsCreated {
                full_name: full_name.to_string(),
            },
        );
    }

    let mut child = binary::spawn(&cfg.binary_path, &tunnel_token).await?;
    if is_initial {
        emit(&cfg.event_tx, Event::CloudflaredStarted);
    }

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("cloudflared stderr not piped")))?;

    let connections = Arc::new(AtomicUsize::new(0));
    let connections_for_pump = Arc::clone(&connections);
    let event_tx_for_pump = cfg.event_tx.clone();
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let kind = binary::classify_line(trimmed);
                    match kind {
                        LineKind::Connection => {
                            let total = connections_for_pump.fetch_add(1, Ordering::SeqCst) + 1;
                            let _ = event_tx_for_pump.send(Event::EdgeConnected {
                                conn_index: total.saturating_sub(1) as u8,
                                total: total as u8,
                            });
                        }
                        LineKind::Error => {
                            let _ = event_tx_for_pump.send(Event::CloudflaredLog {
                                level: LogLevel::Error,
                                line: trimmed.to_string(),
                            });
                        }
                        LineKind::Ignore => {}
                    }
                }
            }
        }
    });

    Ok(Bootstrap {
        state: SessionState {
            api: cfg.api.clone(),
            account_id: cfg.account_id.clone(),
            zone_id: cfg.zone_id.clone(),
            full_name: full_name.to_string(),
            tunnel_id: tunnel.id.clone(),
            cleaned: Arc::new(AtomicBool::new(false)),
        },
        child,
        stderr_task,
    })
}

/// Resolves subdomain conflicts before creating anything. Stale tunnels
/// (`down`/`inactive`/`degraded`) and orphaned DNS records are removed.
async fn resolve_conflicts(
    api: &CloudflareApi,
    account_id: &str,
    zone_id: &str,
    subdomain: &str,
    full_name: &str,
) -> Result<()> {
    if let Ok(existing) = api.find_tunnel_by_name(account_id, subdomain).await {
        if let Some(t) = existing.into_iter().next() {
            if t.status == "healthy" {
                return Err(Error::SubdomainInUse(subdomain.to_string()));
            }
            warn!("reclaiming stale tunnel \"{subdomain}\" ({})", t.status);
            if let Ok(Some(record)) = api.find_dns_record(zone_id, full_name).await {
                let _ = api.delete_dns_record(zone_id, &record.id).await;
            }
            api.cleanup_connections(account_id, &t.id).await?;
            api.delete_tunnel(account_id, &t.id).await?;
        }
    }

    if let Ok(Some(record)) = api.find_dns_record(zone_id, full_name).await {
        warn!("removing orphaned DNS record for {full_name}");
        api.delete_dns_record(zone_id, &record.id).await?;
    }
    Ok(())
}

/// Helper to ignore send errors when no subscribers are listening.
fn emit(tx: &broadcast::Sender<Event>, event: Event) {
    let _ = tx.send(event);
}
