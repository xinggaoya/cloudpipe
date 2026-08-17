//! Handle to a running tunnel returned by [`crate::TunnelBuilder::start`].
//!
//! The handle exposes the public URL, lets you subscribe to more events, and
//! provides both async ([`TunnelHandle::wait`]) and explicit
//! ([`TunnelHandle::stop`]) ways to shut down. [`Drop`] performs best-effort
//! cleanup if `stop` was never called.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{broadcast, Mutex, Notify};
use tokio::task::{JoinError, JoinHandle};

use crate::error::{Error, Result};
use crate::event::Event;
use crate::session::SessionState;

/// Internal shutdown signal shared between the session task and the handle.
#[derive(Debug, Clone)]
pub(crate) struct Shutdown {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    pub(crate) fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }

    pub(crate) fn is_triggered(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub(crate) fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub(crate) async fn notified(&self) {
        // Register first to avoid missing the notification race.
        let notified = self.notify.notified();
        if self.flag.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }
}

/// A live tunnel. Created by [`crate::TunnelBuilder::start`].
///
/// `TunnelHandle` is `Send + Sync` so you can move it between tasks or share
/// behind an `Arc` if you need to.
pub struct TunnelHandle {
    state: Arc<Mutex<SessionState>>,
    shutdown: Shutdown,
    url: String,
    full_name: String,
    subdomain: String,
    events: broadcast::Receiver<Event>,
    task: Option<JoinHandle<()>>,
    stderr_task: Option<JoinHandle<()>>,
    dispatch: crate::session::DispatchSlot,
    stopped: bool,
}

impl std::fmt::Debug for TunnelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelHandle")
            .field("url", &self.url)
            .field("subdomain", &self.subdomain)
            .field("full_name", &self.full_name)
            .field("stopped", &self.stopped)
            .finish_non_exhaustive()
    }
}

impl TunnelHandle {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        state: Arc<Mutex<SessionState>>,
        shutdown: Shutdown,
        events: broadcast::Receiver<Event>,
        full_name: String,
        subdomain: String,
        task: JoinHandle<()>,
        stderr_task: JoinHandle<()>,
        _connections: Arc<AtomicUsize>,
        dispatch: crate::session::DispatchSlot,
    ) -> Self {
        let url = format!("https://{full_name}");
        Self {
            state,
            shutdown,
            url,
            full_name,
            subdomain,
            events,
            task: Some(task),
            stderr_task: Some(stderr_task),
            dispatch,
            stopped: false,
        }
    }

    /// The full public URL of the tunnel (always HTTPS).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The public hostname (`myapp.example.com`).
    pub fn full_name(&self) -> &str {
        &self.full_name
    }

    /// Just the subdomain part (`myapp`).
    pub fn subdomain(&self) -> &str {
        &self.subdomain
    }

    /// Subscribes to lifecycle events. The returned receiver is independent
    /// of any `on_event` closure registered on the builder.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        // Note: there's no public Sender to subscribe from, so we hand out a
        // receiver cloned from the one stored inside the handle. It receives
        // all events emitted from now on.
        // (The current implementation reuses the stored receiver — in a more
        // elaborate SDK we'd split the broadcast channel between handle and
        // builder subscribers.)
        self.events.resubscribe()
    }

    /// Blocks until the tunnel exits for any reason (timeout, child crash,
    /// or an explicit [`stop`](Self::stop)).
    pub async fn wait(mut self) {
        // Trigger shutdown so the background task can wind down cleanly.
        self.shutdown.trigger();

        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        if let Some(task) = self.stderr_task.take() {
            let _ = task.await;
        }
    }

    /// Stops the tunnel and releases all Cloudflare-side resources. Consumes
    /// the handle so it cannot be reused.
    pub async fn stop(mut self) -> Result<()> {
        if self.stopped {
            return Err(Error::AlreadyShutDown);
        }
        self.stopped = true;
        self.shutdown.trigger();

        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        if let Some(task) = self.stderr_task.take() {
            let _ = task.await;
        }

        // Belt-and-suspenders: if the background task somehow didn't clean up
        // (it should have), do it here.
        let mut guard = self.state.lock().await;
        if !guard.cleaned {
            // Mirror `session::cleanup` directly.
            guard.cleaned = true;
            if let Ok(Some(record)) = guard
                .api
                .find_dns_record(&guard.zone_id, &guard.full_name)
                .await
            {
                let _ = guard
                    .api
                    .delete_dns_record(&guard.zone_id, &record.id)
                    .await;
            }
            let _ = guard
                .api
                .cleanup_connections(&guard.account_id, &guard.tunnel_id)
                .await;
            let _ = guard
                .api
                .delete_tunnel(&guard.account_id, &guard.tunnel_id)
                .await;
        }
        Ok(())
    }

    fn _parts(&mut self) {
        // intentionally unused; kept for future field-extraction needs.
    }
}

impl Drop for TunnelHandle {
    fn drop(&mut self) {
        // Best-effort: trigger shutdown so the background task wakes up and
        // cleans up. We can't `await` in Drop, so the actual cleanup is
        // owned by the session task. If the runtime is being torn down too,
        // leaked tasks will be aborted — but `cloudflared` was spawned with
        // `kill_on_drop(true)` so the local half dies with us.
        self.shutdown.trigger();
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
        self.dispatch.abort();
    }
}

// Silence "unused" warnings for fields only consumed inside `wait`.
#[allow(dead_code)]
fn _assert_send_sync<T: Send + Sync>() {}

#[allow(dead_code)]
fn _assert_handle_send_sync() {
    _assert_send_sync::<TunnelHandle>();
    _assert_send_sync::<JoinError>();
}
