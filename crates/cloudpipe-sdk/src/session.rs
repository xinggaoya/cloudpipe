//! Tunnel lifecycle orchestration — UI-agnostic core used by the builder.
//!
//! Owns the Cloudflare session state (tunnel id, DNS, child process) and the
//! shutdown signal. Exposes [`start`] which runs the setup phase and returns
//! a [`TunnelHandle`] ready for the caller to await / stop.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// Tunnels are cleaned up automatically after this much time.
pub const TUNNEL_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);

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
}

/// Mutable Cloudflare-side state of a running session.
#[derive(Debug)]
pub(crate) struct SessionState {
    pub api: CloudflareApi,
    pub account_id: String,
    pub zone_id: String,
    pub full_name: String,
    pub tunnel_id: String,
    pub cleaned: bool,
}

/// Boots a tunnel end-to-end: resolve conflicts → create tunnel → ingress →
/// DNS → spawn cloudflared → install background stderr pump + shutdown
/// watcher → return a [`TunnelHandle`].
pub(crate) async fn start(cfg: SessionConfig) -> Result<TunnelHandle> {
    validate_subdomain(&cfg.subdomain)
        .map_err(|e| Error::InvalidSubdomain(format!("\"{}\": {}", e.value, e.reason)))?;
    let full_name = format!("{}.{}", cfg.subdomain, cfg.domain);

    emit(&cfg.event_tx, Event::Banner);
    emit(&cfg.event_tx, Event::ResolvingConflicts);

    resolve_conflicts(
        &cfg.api,
        &cfg.account_id,
        &cfg.zone_id,
        &cfg.subdomain,
        &full_name,
    )
    .await?;

    emit(
        &cfg.event_tx,
        Event::CreatingTunnel {
            name: cfg.subdomain.clone(),
        },
    );

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
            hostname: Some(full_name.clone()),
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
    emit(
        &cfg.event_tx,
        Event::IngressConfigured {
            protocol: cfg.protocol,
            port: cfg.port,
        },
    );

    let cname_target = format!("{}.cfargotunnel.com", tunnel.id);
    if let Err(err) = cfg
        .api
        .create_dns_record(&cfg.zone_id, &full_name, &cname_target)
        .await
    {
        let _ = cfg.api.delete_tunnel(&cfg.account_id, &tunnel.id).await;
        return Err(err);
    }
    emit(
        &cfg.event_tx,
        Event::DnsCreated {
            full_name: full_name.clone(),
        },
    );

    let mut child = binary::spawn(&cfg.binary_path, &tunnel_token).await?;
    emit(&cfg.event_tx, Event::CloudflaredStarted);

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

    let state = Arc::new(Mutex::new(SessionState {
        api: cfg.api.clone(),
        account_id: cfg.account_id.clone(),
        zone_id: cfg.zone_id.clone(),
        full_name: full_name.clone(),
        tunnel_id: tunnel.id.clone(),
        cleaned: false,
    }));

    let state_for_task = Arc::clone(&state);
    let shutdown_for_task = cfg.shutdown.clone();
    let event_tx_for_task = cfg.event_tx.clone();
    let started = Instant::now();
    let task: JoinHandle<()> = tokio::spawn(async move {
        let reason = run_until_exit(&mut child, &shutdown_for_task, started).await;
        let _ = event_tx_for_task.send(Event::ShuttingDown {
            reason: reason.clone(),
        });

        let mut guard = state_for_task.lock().await;
        cleanup(&mut guard).await;

        let _ = event_tx_for_task.send(Event::Cleaned);
    });

    let events_rx = cfg.event_tx.subscribe();
    Ok(TunnelHandle::new(
        state,
        cfg.shutdown,
        events_rx,
        full_name,
        cfg.subdomain,
        task,
        stderr_task,
        connections,
        cfg.dispatch,
    ))
}

/// Polls the child and shutdown signal until one fires. Returns the reason.
async fn run_until_exit(
    child: &mut tokio::process::Child,
    shutdown: &Shutdown,
    started: Instant,
) -> ShutdownReason {
    let deadline = started + TUNNEL_TIMEOUT;
    loop {
        if shutdown.is_triggered() {
            return ShutdownReason::UserRequested;
        }
        if Instant::now() >= deadline {
            warn!("4h age limit reached — cleaning up");
            return ShutdownReason::Timeout;
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
        // Sleep until either shutdown is signaled or ~250ms pass.
        let triggered = shutdown.notified();
        tokio::select! {
            _ = triggered => return ShutdownReason::UserRequested,
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }
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
