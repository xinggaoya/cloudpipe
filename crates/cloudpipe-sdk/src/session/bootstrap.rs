//! Bringing a tunnel up: conflict resolution, tunnel + ingress + DNS
//! creation, `cloudflared` spawn, and the stderr pump.
//!
//! Every Cloudflare resource is registered with [`Created`] the moment it
//! exists, so a failure at any later step unwinds the earlier ones instead
//! of orphaning a tunnel or a DNS record on the account.

use std::sync::Arc;

use tracing::warn;

use crate::binary;
use crate::client::CloudflareApi;
use crate::error::{Error, Result};
use crate::event::Event;
use crate::handle::StderrSlot;
use crate::session::conflict::resolve_conflicts;
use crate::session::pump::{self, StderrPump};
use crate::session::state::{SessionConfig, SessionState};

use super::emit;

/// Tracks the Cloudflare resources a bootstrap attempt has created so a
/// failure at any later step can unwind them.
///
/// Without this, a `cloudflared` spawn failure left both a tunnel and a
/// DNS record orphaned on the account, only reclaimable by a later
/// `cfp cleanup` run.
#[derive(Debug, Default)]
pub(crate) struct Created {
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
pub(crate) struct Bootstrap {
    pub(crate) state: SessionState,
    pub(crate) child: tokio::process::Child,
    pub(crate) stderr_pump: StderrPump,
}

impl Bootstrap {
    /// Splits the generation into the pieces the caller installs.
    ///
    /// The pump cannot be cloned, so it is *moved* into the shared slot
    /// exactly once — the supervisor reads that slot to abort the previous
    /// generation, and the handle drains whichever is current.
    pub(crate) fn into_parts(self) -> (SessionState, tokio::process::Child, StderrSlot) {
        let slot: StderrSlot = Arc::new(std::sync::Mutex::new(Some(self.stderr_pump)));
        (self.state, self.child, slot)
    }
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
pub(crate) async fn session(
    cfg: &SessionConfig,
    full_name: &str,
    is_initial: bool,
) -> Result<Bootstrap> {
    let mut created = Created::default();
    let result = inner(cfg, full_name, is_initial, &mut created).await;
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
async fn inner(
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

    let state = SessionState::new(
        cfg.api.clone(),
        &cfg.account_id,
        &cfg.zone_id,
        full_name,
        &tunnel.id,
        cfg.protocol,
        cfg.port,
    );
    cfg.api
        .set_tunnel_ingress(
            &cfg.account_id,
            &tunnel.id,
            &state.ingress(cfg.origin_request.as_ref()),
        )
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

    let stderr_pump = pump::spawn(stderr, cfg.event_tx.clone());

    Ok(Bootstrap {
        state,
        child,
        stderr_pump,
    })
}
