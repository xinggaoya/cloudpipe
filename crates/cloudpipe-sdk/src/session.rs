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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::binary::{self, LineKind};
use crate::client::{CloudflareApi, IngressEntry};
use crate::error::{Error, Result};
use crate::event::{Event, LogLevel, ShutdownReason};
use crate::handle::{Shutdown, TunnelHandle};
use crate::protocol::{validate_subdomain, Protocol};

/// Maximum number of consecutive bootstrap failures before the auto-restart
/// loop gives up and tears the session down. A bootstrap failure typically
/// means Cloudflare is rate-limiting the token, or `cloudflared` is missing
/// or non-executable on disk; retrying indefinitely would just burn API
/// quota, so we stop after a handful of attempts and surface the error.
pub const MAX_RESTART_ATTEMPTS: u32 = 5;

/// Delay between auto-restart attempts. Kept short — the SDK only fails on
/// transient API or network blips, and long backoffs here just punish the
/// user. We rely on [`MAX_RESTART_ATTEMPTS`] to bound total damage.
pub const RESTART_BACKOFF: Duration = Duration::from_millis(500);

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
    pub event_tx: broadcast::Sender<Event>,
    pub shutdown: Shutdown,
    pub dispatch: DispatchSlot,
    pub auto_restart: bool,
}

/// Mutable Cloudflare-side state of a running session. The `tunnel_id` is
/// rewritten in-place whenever the auto-restart loop creates a fresh
/// tunnel, so a [`crate::TunnelHandle::stop`] call always cleans up the
/// tunnel that's actually live.
#[derive(Debug, Clone)]
pub(crate) struct SessionState {
    pub api: CloudflareApi,
    pub account_id: String,
    pub zone_id: String,
    pub full_name: String,
    pub tunnel_id: String,
    pub cleaned: bool,
}

/// Output of a single bootstrap attempt: a running `cloudflared` plus the
/// state and tasks the supervisor task needs to drive it.
struct Bootstrap {
    state: SessionState,
    child: tokio::process::Child,
    stderr_task: JoinHandle<()>,
    connections: Arc<AtomicUsize>,
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
    let connections = initial.connections;

    let state_for_task = Arc::clone(&state);
    let shutdown_for_task = cfg.shutdown.clone();
    let event_tx_for_task = cfg.event_tx.clone();
    let api_for_task = cfg.api.clone();
    let account_id_for_task = cfg.account_id.clone();
    let zone_id_for_task = cfg.zone_id.clone();
    let protocol_for_task = cfg.protocol;
    let port_for_task = cfg.port;
    let subdomain_for_task = cfg.subdomain.clone();
    let binary_path_for_task = cfg.binary_path.clone();
    let auto_restart = cfg.auto_restart;
    let dispatch_for_task = cfg.dispatch.clone();
    let full_name_for_task = full_name.clone();

    let mut current_child = initial.child;
    let stderr_slot_for_task = Arc::clone(&stderr_slot);
    let mut attempt: u32 = 1;
    let mut consecutive_failures: u32 = 0;
    let mut last_bootstrap_error: Option<String> = None;

    let task: JoinHandle<()> = tokio::spawn(async move {
        loop {
            log_debug("supervisor: enter loop");
            let reason = run_until_exit(&mut current_child, &shutdown_for_task).await;
            log_debug(&format!("supervisor: exit reason = {reason:?}"));

            // Drain the stderr pump for this generation before we drop
            // the child. We abort it (the process is already gone) rather
            // than waiting for the read to return EOF, which can hang if
            // the OS hasn't reaped the process yet.
            if let Some(task) = stderr_slot_for_task
                .lock()
                .expect("stderr slot poisoned")
                .take()
            {
                task.abort();
            }

            {
                let mut guard = state_for_task.lock().await;
                cleanup(&mut guard).await;
            }

            match reason {
                ShutdownReason::UserRequested => {
                    let _ = event_tx_for_task.send(Event::ShuttingDown { reason });
                    break;
                }
                other => {
                    let _ = event_tx_for_task.send(Event::ShuttingDown {
                        reason: other.clone(),
                    });

                    if !auto_restart {
                        break;
                    }

                    attempt += 1;
                    consecutive_failures += 1;
                    if consecutive_failures > MAX_RESTART_ATTEMPTS {
                        let _ = event_tx_for_task.send(Event::RestartGivingUp {
                            attempts: consecutive_failures,
                            last_error: last_bootstrap_error
                                .clone()
                                .unwrap_or_else(|| format!("{other:?}")),
                        });
                        break;
                    }

                    let _ = event_tx_for_task.send(Event::Restarting {
                        reason: other,
                        attempt,
                    });

                    tokio::time::sleep(RESTART_BACKOFF).await;

                    // If the user pressed Ctrl+C while we were tearing
                    // down, don't bother rebuilding a tunnel that's about
                    // to be cleaned up again.
                    if shutdown_for_task.is_triggered() {
                        break;
                    }

                    let respawn_cfg = SessionConfig {
                        api: api_for_task.clone(),
                        account_id: account_id_for_task.clone(),
                        zone_id: zone_id_for_task.clone(),
                        domain: full_name_for_task
                            .rsplit_once('.')
                            .map(|(_, d)| d.to_string())
                            .unwrap_or_default(),
                        protocol: protocol_for_task,
                        port: port_for_task,
                        subdomain: subdomain_for_task.clone(),
                        binary_path: binary_path_for_task.clone(),
                        event_tx: event_tx_for_task.clone(),
                        shutdown: shutdown_for_task.clone(),
                        dispatch: dispatch_for_task.clone(),
                        auto_restart,
                    };
                    match bootstrap_session(&respawn_cfg, &full_name_for_task, false).await {
                        Ok(next) => {
                            {
                                let mut guard = state_for_task.lock().await;
                                *guard = SessionState {
                                    cleaned: false,
                                    ..next.state.clone()
                                };
                            }
                            current_child = next.child;
                            {
                                let mut guard = stderr_slot_for_task
                                    .lock()
                                    .expect("stderr slot poisoned");
                                *guard = Some(next.stderr_task);
                            }
                            consecutive_failures = 0;
                            last_bootstrap_error = None;
                            let _ = event_tx_for_task
                                .send(Event::Restarted { attempt });
                        }
                        Err(err) => {
                            warn!("auto-restart bootstrap failed: {err:#}");
                            last_bootstrap_error = Some(format!("{err:#}"));
                            // Loop back: run_until_exit will see the
                            // previous (now-dead) child exit immediately
                            // and we'll re-enter the cleanup + retry
                            // branch with an incremented failure count.
                        }
                    }
                }
            }
        }

        let _ = event_tx_for_task.send(Event::Cleaned);
        log_debug("supervisor: task done");
    });
    log_debug("supervisor: spawned");

    let events_rx = cfg.event_tx.subscribe();
    Ok(TunnelHandle::new(
        state,
        cfg.shutdown,
        events_rx,
        full_name,
        cfg.subdomain,
        task,
        stderr_slot,
        connections,
        cfg.dispatch,
    ))
}

/// Runs a single tunnel session: blocks until either the user signals
/// shutdown or the `cloudflared` child exits on its own.
///
/// There is **no time limit** — the session lives as long as `cloudflared`
/// does. The auto-restart loop in [`start`] decides what to do with the
/// returned [`ShutdownReason`].
async fn run_until_exit(
    child: &mut tokio::process::Child,
    shutdown: &Shutdown,
) -> ShutdownReason {
    loop {
        if shutdown.is_triggered() {
            return ShutdownReason::UserRequested;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                debug!("cloudflared exited with {status:?}");
                return ShutdownReason::ChildExited;
            }
            Ok(None) => {}
            Err(err) => {
                warn!("try_wait failed: {err}");
                return ShutdownReason::Error(err.to_string());
            }
        }
        let triggered = shutdown.notified();
        tokio::select! {
            _ = triggered => return ShutdownReason::UserRequested,
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }
}

/// Runs one full bootstrap pass — resolve conflicts, create the tunnel,
/// write ingress, create the DNS record, spawn `cloudflared`, install the
/// stderr pump — and hands the live components back to the caller.
///
/// `is_initial` controls whether the public-facing setup events
/// ([`Event::ResolvingConflicts`], [`Event::CreatingTunnel`], etc.) are
/// emitted. Respawns emit only [`Event::Restarted`] instead, so the UI
/// doesn't repeat the same banner every 4 hours.
async fn bootstrap_session(
    cfg: &SessionConfig,
    full_name: &str,
    is_initial: bool,
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
    let tunnel_token = tunnel.token.clone().ok_or_else(|| {
        Error::Other(anyhow::anyhow!(
            "Cloudflare returned a tunnel without a token"
        ))
    })?;

    let ingress = vec![
        IngressEntry {
            hostname: Some(full_name.to_string()),
            service: cfg.protocol.local_service(cfg.port),
        },
        IngressEntry {
            hostname: None,
            service: "http_status:404".to_string(),
        },
    ];
    if let Err(err) = cfg
        .api
        .set_tunnel_ingress(&cfg.account_id, &tunnel.id, &ingress)
        .await
    {
        let _ = cfg.api.delete_tunnel(&cfg.account_id, &tunnel.id).await;
        return Err(err);
    }
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
    if let Err(err) = cfg
        .api
        .create_dns_record(&cfg.zone_id, full_name, &cname_target)
        .await
    {
        let _ = cfg.api.delete_tunnel(&cfg.account_id, &tunnel.id).await;
        return Err(err);
    }
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
            cleaned: false,
        },
        child,
        stderr_task,
        connections,
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

/// One-shot Cloudflare-side cleanup. Idempotent.
async fn cleanup(state: &mut SessionState) {
    if state.cleaned {
        return;
    }
    state.cleaned = true;
    if let Ok(Some(record)) = state
        .api
        .find_dns_record(&state.zone_id, &state.full_name)
        .await
    {
        let _ = state
            .api
            .delete_dns_record(&state.zone_id, &record.id)
            .await;
    }
    let _ = state
        .api
        .cleanup_connections(&state.account_id, &state.tunnel_id)
        .await;
    let _ = state
        .api
        .delete_tunnel(&state.account_id, &state.tunnel_id)
        .await;
}

/// Helper to ignore send errors when no subscribers are listening.
fn emit(tx: &broadcast::Sender<Event>, event: Event) {
    let _ = tx.send(event);
}

/// Append-only debug trace. Active when the `CFP_DEBUG_LOG` env var is
/// set (the file it points at is opened in append mode). Off by default
/// in release builds so we don't pay for fsync on every supervisor tick.
pub(crate) fn log_debug(msg: &str) {
    if let Ok(path) = std::env::var("CFP_DEBUG_LOG") {
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(
                f,
                "[{:.3}] {}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0),
                msg
            );
        }
    }
}
