//! Handle to a running tunnel returned by [`crate::TunnelBuilder::start`].
//!
//! The handle exposes the public URL, lets you subscribe to more events, and
//! provides two ways to interact with the tunnel's lifetime:
//!
//! - [`TunnelHandle::wait`] — passively block until the user-initiated
//!   shutdown completes. With auto-restart enabled this only fires after
//!   [`TunnelHandle::stop`]; `cloudflared` crashes are absorbed by the
//!   respawn loop in the background task. Does **not** signal shutdown on
//!   its own; pair it with a control signal (e.g. `tokio::signal::ctrl_c`)
//!   and call [`TunnelHandle::stop`] from there.
//! - [`TunnelHandle::stop`] — signal shutdown, await the background task
//!   and run Cloudflare-side cleanup.
//!
//! [`Drop`] performs a best-effort `shutdown.trigger()` so the background
//! task has one last chance to run cleanup if the runtime hasn't already
//! been torn down — but you should not rely on it. Always call
//! [`TunnelHandle::stop`] (or the corresponding `Drop`) on every code path
//! you control so the Cloudflare tunnel and DNS record are released.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

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

/// Holds the stderr pump task for the current session generation. The
/// background task swaps this slot whenever it respawns `cloudflared`;
/// [`TunnelHandle::wait`] / [`TunnelHandle::stop`] / `Drop` always await
/// the most recent task so we don't leak a generation's pump.
pub(crate) type StderrSlot = Arc<StdMutex<Option<JoinHandle<()>>>>;

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
    stderr_slot: Option<StderrSlot>,
    _connections: Arc<AtomicUsize>,
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
        stderr_slot: StderrSlot,
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
            stderr_slot: Some(stderr_slot),
            _connections,
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
    pub fn subscribe(&mut self) -> broadcast::Receiver<Event> {
        self.events.resubscribe()
    }

    /// Blocks until the user-initiated shutdown completes.
    ///
    /// With auto-restart enabled this only returns after the caller has
    /// triggered shutdown (via [`TunnelHandle::stop`] or another path);
    /// intermediate `cloudflared` crashes are absorbed by the respawn
    /// loop and produce an [`Event::Restarted`] instead.
    ///
    /// This call is a passive wait: it does **not** signal shutdown on
    /// its own. To stop the tunnel cleanly, call [`stop`](Self::stop) (or
    /// trigger shutdown through another path).
    ///
    /// Note: `wait()` does **not** consume the background task — it
    /// just observes its completion. A subsequent [`stop`](Self::stop)
    /// can still drive cleanup. This matters when the caller races
    /// `wait()` against another signal via `tokio::select!` and one
    /// branch gets cancelled: the underlying task remains attached.
    pub async fn wait(&mut self) {
        // Await the background task by reference so we don't consume it
        // — a subsequent `stop()` still needs to drive cleanup.
        if let Some(task) = self.task.as_mut() {
            let _ = task.await;
        }
        // Drain the most recent stderr pump. By the time `wait()` is
        // returning the background task has already finished, so the
        // slot will not be rewritten underneath us.
        if let Some(slot) = self.stderr_slot.take() {
            if let Some(task) = slot.lock().expect("stderr slot poisoned").take() {
                let _ = task.await;
            }
        }
    }

    /// Stops the tunnel and releases all Cloudflare-side resources.
    ///
    /// Idempotent: calling it twice returns [`Error::AlreadyShutDown`]. If
    /// the tunnel is already gone (e.g. `cloudflared` crashed and the
    /// background task finished on its own), `stop` still runs the
    /// Cloudflare-side cleanup best-effort.
    pub async fn stop(&mut self) -> Result<()> {
        if self.stopped {
            return Err(Error::AlreadyShutDown);
        }
        self.stopped = true;
        self.shutdown.trigger();

        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        if let Some(slot) = self.stderr_slot.take() {
            if let Some(task) = slot.lock().expect("stderr slot poisoned").take() {
                let _ = task.await;
            }
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
        if let Some(slot) = self.stderr_slot.take() {
            if let Some(task) = slot.lock().expect("stderr slot poisoned").take() {
                task.abort();
            }
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
