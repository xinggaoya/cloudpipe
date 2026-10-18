//! The data a session carries: its configuration, the live Cloudflare
//! coordinates, and the task slots the handle reads.
//!
//! [`SessionState::purge`] lives here rather than in the supervisor so the
//! supervisor and `TunnelHandle::stop` share exactly one cleanup
//! implementation.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::client::{CloudflareApi, IngressEntry, OriginRequest};
use crate::event::Event;
use crate::handle::Shutdown;
use crate::protocol::Protocol;
use crate::session::policy::RestartPolicy;

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
///
/// `Clone` because the supervisor owns a copy for its reconnection attempts.
#[derive(Debug, Clone)]
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
    /// Local protocol the ingress rule points at.
    pub protocol: Protocol,
    /// Local port the ingress rule points at.
    pub port: u16,
    pub(crate) cleaned: Arc<AtomicBool>,
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
            protocol: self.protocol,
            port: self.port,
            cleaned: Arc::clone(&self.cleaned),
        }
    }
}

impl SessionState {
    /// A fresh, not-yet-cleaned state for a newly created generation.
    pub(crate) fn new(
        api: CloudflareApi,
        account_id: &str,
        zone_id: &str,
        full_name: &str,
        tunnel_id: &str,
        protocol: Protocol,
        port: u16,
    ) -> Self {
        Self {
            api,
            account_id: account_id.to_string(),
            zone_id: zone_id.to_string(),
            full_name: full_name.to_string(),
            tunnel_id: tunnel_id.to_string(),
            protocol,
            port,
            cleaned: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Builds the ingress rules this session publishes: the hostname rule
    /// plus the mandatory catch-all.
    pub(crate) fn ingress(&self, origin_request: Option<&OriginRequest>) -> Vec<IngressEntry> {
        self.ingress_for_port(self.port, origin_request)
    }

    /// The same rules, but targeting `port` instead of the current one.
    ///
    /// Used by [`TunnelHandle::update_port`](crate::TunnelHandle::update_port)
    /// to publish a new origin without mutating this state first.
    pub(crate) fn ingress_for_port(
        &self,
        port: u16,
        origin_request: Option<&OriginRequest>,
    ) -> Vec<IngressEntry> {
        vec![
            IngressEntry {
                hostname: Some(self.full_name.clone()),
                service: self.protocol.local_service(port),
                origin_request: origin_request.cloned(),
            },
            IngressEntry {
                hostname: None,
                service: "http_status:404".to_string(),
                origin_request: None,
            },
        ]
    }

    /// Re-targets this state at a freshly created generation.
    ///
    /// Used on respawn: the previous generation's coordinates are dropped
    /// and the cleanup flag is reset, so a later `stop()` cannot be fooled
    /// into thinking the new tunnel is already released.
    pub(crate) fn retarget(&mut self, next: SessionState) {
        *self = SessionState {
            cleaned: Arc::new(AtomicBool::new(false)),
            ..next
        };
    }

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
            let _ = self.api.delete_dns_record(&self.zone_id, &record.id).await;
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
