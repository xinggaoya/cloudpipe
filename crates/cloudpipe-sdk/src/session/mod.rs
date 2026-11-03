//! Tunnel lifecycle orchestration — the UI-agnostic core behind the builder.
//!
//! Owns the Cloudflare session state (tunnel id, DNS, child process) and
//! the shutdown signal. [`start`] runs the setup phase and hands back a
//! [`TunnelHandle`] for the caller to await or stop.
//!
//! The module is split by concern rather than by size:
//!
//! | module | responsibility |
//! |---|---|
//! | [`policy`] | reconnection budget, random subdomain generation |
//! | [`state`] | [`SessionConfig`], live Cloudflare coordinates, task slots |
//! | [`conflict`] | reclaiming a subdomain that is already in use |
//! | [`bootstrap`] | creating tunnel + ingress + DNS and spawning `cloudflared` |
//! | [`supervisor`] | the keep-alive loop |
//!
//! ## Auto-restart
//!
//! When [`SessionConfig::auto_restart`] is set, the supervisor keeps the
//! tunnel alive across `cloudflared` crashes: each respawn creates a fresh
//! Cloudflare tunnel object and DNS record under the **same subdomain**, so
//! the public hostname is stable. A user-initiated
//! [`TunnelHandle::stop`](crate::TunnelHandle::stop) always ends the
//! session regardless of the setting.

mod bootstrap;
mod conflict;
mod crashloop;
mod policy;
pub(crate) mod pump;
mod state;
mod supervisor;

use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::error::Result;
use crate::event::Event;
use crate::handle::TunnelHandle;
use crate::protocol::validate_subdomain;

pub use policy::{random_subdomain, RestartPolicy};
pub(crate) use state::SessionConfig;
pub(crate) use state::{DispatchSlot, SessionState};

use supervisor::Supervisor;

/// Boots a tunnel end-to-end, then keeps it running until the user calls
/// [`TunnelHandle::stop`].
///
/// The subdomain is validated before any network call, so a typo never
/// costs a zone lookup or a `cloudflared` download.
pub(crate) async fn start(cfg: SessionConfig) -> Result<TunnelHandle> {
    validate_subdomain(&cfg.subdomain).map_err(|e| {
        crate::error::Error::InvalidSubdomain(format!("\"{}\": {}", e.value, e.reason))
    })?;

    let full_name = format!("{}.{}", cfg.subdomain, cfg.domain);

    // The very first bootstrap emits the public-facing setup events so the
    // UI can render them. Respawns use a quieter path that emits only
    // Restarting / Restarted / BootstrapFailed.
    let initial = bootstrap::session(&cfg, &full_name, true).await?;

    // The shared session state. Respawns rewrite `state.tunnel_id` so
    // belt-and-suspenders cleanup in `TunnelHandle::stop` targets the live
    // tunnel, not a stale one from a previous generation.
    let (initial_state, initial_child, stderr_slot) = initial.into_parts();
    let port = initial_state.port;
    let state = Arc::new(Mutex::new(initial_state));

    // The stderr pump is replaced on every respawn. The handle keeps the
    // `Arc<Mutex<…>>` slot so it can drain whichever generation is current.
    let state_for_task = Arc::clone(&state);
    let stderr_for_task = stderr_slot.clone();
    let event_tx_for_task = cfg.event_tx.clone();
    let cfg_for_task = cfg.clone();

    // The supervisor publishes how the session ended here, so a caller
    // that waits for the session can tell a clean stop from a failure
    // without racing the event stream.
    let outcome_slot: crate::handle::OutcomeSlot = Arc::new(std::sync::Mutex::new(None));
    let outcome_slot_for_task = Arc::clone(&outcome_slot);

    let task: JoinHandle<()> = tokio::spawn(async move {
        let supervisor = Supervisor::new(
            initial_child,
            state_for_task,
            stderr_for_task,
            outcome_slot_for_task,
            cfg_for_task,
        );
        supervisor.run().await;
        let _ = event_tx_for_task.send(Event::Cleaned);
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
        port,
        cfg.origin_request.clone(),
        task,
        stderr_slot,
        cfg.dispatch.clone(),
        outcome_slot,
    ))
}

/// Sends an event, ignoring the error from having no subscribers.
fn emit(tx: &tokio::sync::broadcast::Sender<Event>, event: Event) {
    let _ = tx.send(event);
}
