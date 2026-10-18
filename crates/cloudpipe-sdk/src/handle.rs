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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{broadcast, Mutex, Notify};
use tokio::task::JoinHandle;

use crate::client::OriginRequest;
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

/// Ceiling on how long [`TunnelHandle::stop`] may spend tearing down.
///
/// Comfortably above the SDK's own 30 s HTTP deadline plus a SIGTERM grace
/// period, so a healthy shutdown never trips it.
pub const CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// Holds the stderr pump task for the current session generation. The
/// background task swaps this slot whenever it respawns `cloudflared`;
/// [`TunnelHandle::wait`] / [`TunnelHandle::stop`] / `Drop` always await
/// the most recent task so we don't leak a generation's pump.
pub(crate) type StderrSlot = Arc<StdMutex<Option<JoinHandle<()>>>>;

/// Extracts the pump task from `slot` and releases the lock before the
/// caller awaits it.
///
/// The slot is a `std::sync::Mutex` (not an async one) on purpose: it is
/// only ever held for a `take()`, never across an `.await`. Routing the
/// extraction through this helper keeps that invariant checkable by the
/// compiler instead of by convention.
pub(crate) fn take_pump(slot: &StderrSlot) -> Option<JoinHandle<()>> {
    let mut guard = slot.lock().expect("stderr slot poisoned");
    guard.take()
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
    /// Mirrors `SessionState::port` so the getter needs no lock.
    port: u16,
    /// Origin tuning to re-publish on every `update_port`.
    origin_request: Option<OriginRequest>,
    task: Option<JoinHandle<()>>,
    stderr_slot: Option<StderrSlot>,
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
        port: u16,
        origin_request: Option<OriginRequest>,
        task: JoinHandle<()>,
        stderr_slot: StderrSlot,
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
            port,
            origin_request,
            task: Some(task),
            stderr_slot: Some(stderr_slot),
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

    /// The local port currently forwarded by the tunnel.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether the background session task has finished.
    ///
    /// With auto-restart enabled this only becomes `true` after
    /// [`stop`](Self::stop) or after the retry budget is exhausted.
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_some_and(|t| t.is_finished())
    }

    /// Re-points the tunnel at a different local port.
    ///
    /// The public hostname — and therefore any URL already shared — is
    /// unchanged; only the local target moves. This rewrites the tunnel's
    /// remote ingress configuration, so it takes effect without a restart
    /// and without touching DNS.
    ///
    /// ```no_run
    /// # use cloudpipe_sdk::{Protocol, TunnelBuilder};
    /// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut handle = TunnelBuilder::new()
    ///     .token(std::env::var("CLOUDFLARE_API_TOKEN")?)
    ///     .domain("example.com")
    ///     .protocol(Protocol::Http)
    ///     .port(8080)
    ///     .start()
    ///     .await?;
    /// assert_eq!(handle.port(), 8080);
    /// handle.update_port(9090).await?;
    /// assert_eq!(handle.port(), 9090);
    /// # Ok(()) }
    /// ```
    pub async fn update_port(&mut self, port: u16) -> Result<()> {
        if self.stopped {
            return Err(Error::AlreadyShutDown);
        }
        if port == 0 {
            return Err(Error::InvalidPort(port));
        }
        // Short lock: read the coordinates, then do the API write with no
        // guard held.
        let snapshot = { self.state.lock().await.clone() };
        snapshot
            .api
            .set_tunnel_ingress(
                &snapshot.account_id,
                &snapshot.tunnel_id,
                &snapshot.ingress_for_port(port, self.origin_request.as_ref()),
            )
            .await?;
        // Commit only after Cloudflare accepted it, so a failed update
        // leaves `port()` reporting what is actually live.
        self.state.lock().await.port = port;
        self.port = port;
        Ok(())
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
            if let Some(task) = take_pump(&slot) {
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
    ///
    /// # Cancellation
    ///
    /// Every internal await is bounded by [`CLEANUP_TIMEOUT`]. The SDK's
    /// own HTTP calls carry a request deadline, but a caller that wrapped
    /// this in a `select!` against another branch would otherwise have no
    /// upper bound at all — a shutdown that never returns is worse than one
    /// that gives up, because it leaves the process running with a
    /// half-released tunnel.
    pub async fn stop(&mut self) -> Result<()> {
        self.stop_within(CLEANUP_TIMEOUT).await
    }

    /// [`stop`](Self::stop) with an explicit bound on how long teardown may
    /// take.
    pub async fn stop_within(&mut self, budget: std::time::Duration) -> Result<()> {
        if self.stopped {
            return Err(Error::AlreadyShutDown);
        }
        self.stopped = true;
        self.shutdown.trigger();

        let reap = async {
            if let Some(task) = self.task.take() {
                let _ = task.await;
            }
        };
        if tokio::time::timeout(budget, reap).await.is_err() {
            tracing::warn!("stop(): supervisor task did not finish within {budget:?}");
            return Ok(());
        }
        if let Some(slot) = self.stderr_slot.take() {
            if let Some(task) = take_pump(&slot) {
                let _ = task.await;
            }
        }

        // Belt-and-suspenders: if the background task somehow didn't clean up
        // (it should have), do it here. Snapshot the Cloudflare-side
        // coordinates under a short lock and release it before the network
        // round-trips — `SessionState::purge` is internally idempotent, so
        // racing the supervisor's own purge is harmless.
        let snapshot = { self.state.lock().await.clone() };
        if tokio::time::timeout(budget, snapshot.purge())
            .await
            .is_err()
        {
            tracing::warn!("stop(): Cloudflare cleanup exceeded {budget:?}; run `cfp cleanup`");
        }
        Ok(())
    }
}

impl Drop for TunnelHandle {
    fn drop(&mut self) {
        // We cannot `await` in `Drop`, so the Cloudflare-side teardown is
        // detached onto the runtime instead. The previous implementation
        // only called `task.abort()`, which killed the supervisor *before*
        // it could run cleanup — so `drop(handle)` reliably orphaned a
        // tunnel and a DNS record on the user's account.
        if !self.stopped {
            self.spawn_detached_cleanup();
        }

        self.shutdown.trigger();
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Some(slot) = self.stderr_slot.take() {
            if let Some(task) = take_pump(&slot) {
                task.abort();
            }
        }
        self.dispatch.abort();
    }
}

impl TunnelHandle {
    /// Releases the Cloudflare resources in the background.
    ///
    /// Best-effort by nature: the process may be exiting and the runtime
    /// may be shutting down, in which case the task is dropped rather than
    /// run. That is strictly better than never attempting it at all, and
    /// `cfp cleanup` remains the backstop for anything left behind.
    fn spawn_detached_cleanup(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // No runtime (e.g. dropped outside async context). Nothing can
            // be awaited; `cfp cleanup` is the recovery path.
            return;
        };
        let snapshot = match self.state.try_lock() {
            Ok(guard) => guard.clone(),
            Err(_) => return,
        };
        handle.spawn(async move {
            snapshot.purge().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handle with no session behind it, for exercising the parts that do
    /// not need a live tunnel.
    fn detached() -> TunnelHandle {
        let state = SessionState {
            api: crate::CloudflareApi::with_base("t", "http://127.0.0.1:1").unwrap(),
            account_id: "a".into(),
            zone_id: "z".into(),
            full_name: "app.example.com".into(),
            tunnel_id: "t".into(),
            protocol: crate::Protocol::Http,
            port: 8080,
            cleaned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let (tx, _rx) = broadcast::channel(4);
        TunnelHandle::new(
            Arc::new(Mutex::new(state)),
            Shutdown::new(),
            tx.subscribe(),
            "app.example.com".into(),
            "app".into(),
            8080,
            None,
            tokio::runtime::Runtime::new()
                .map(|rt| rt.spawn(async {}))
                .unwrap(),
            Arc::new(StdMutex::new(None)),
            crate::session::DispatchSlot::default(),
        )
    }

    #[test]
    fn getters_describe_the_tunnel() {
        let handle = detached();
        assert_eq!(handle.url(), "https://app.example.com");
        assert_eq!(handle.full_name(), "app.example.com");
        assert_eq!(handle.subdomain(), "app");
        assert_eq!(handle.port(), 8080);
        // The stub task completes immediately, so `is_finished` is the
        // observable here; `shutdown_signal_is_latched` covers the
        // still-running case via the flag instead.
        assert!(handle.is_finished());
    }

    #[test]
    fn a_fresh_shutdown_is_not_triggered() {
        assert!(!Shutdown::new().is_triggered());
        assert!(!Shutdown::default().is_triggered());
    }

    #[test]
    fn debug_does_not_leak_the_token() {
        let rendered = format!("{:?}", detached());
        assert!(rendered.contains("app.example.com"), "{rendered}");
        assert!(!rendered.contains("test-token"), "{rendered}");
    }

    #[test]
    fn shutdown_signal_is_latched() {
        let shutdown = Shutdown::new();
        assert!(!shutdown.is_triggered());

        // Registering before triggering must not miss the notification —
        // that race would hang `wait_for_exit` forever.
        let waiter = shutdown.clone();
        let trigger = shutdown.clone();
        runtime().block_on(async move {
            let notified = tokio::spawn(async move { waiter.notified().await });
            // Let the waiter actually reach `notified()` before triggering.
            tokio::task::yield_now().await;
            trigger.trigger();
            notified
                .await
                .expect("trigger() must wake a pending waiter");
        });
        assert!(shutdown.is_triggered());
    }

    #[test]
    fn shutdown_notified_returns_immediately_once_triggered() {
        let shutdown = Shutdown::new();
        shutdown.trigger();
        runtime().block_on(async { shutdown.notified().await });
    }

    /// A shared current-thread runtime for the async bits of these tests.
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    }

    #[test]
    fn take_pump_releases_the_slot() {
        let slot: StderrSlot = Arc::new(StdMutex::new(None));
        assert!(take_pump(&slot).is_none());

        let slot: StderrSlot = Arc::new(StdMutex::new(Some(
            tokio::runtime::Runtime::new()
                .unwrap()
                .spawn(async { std::future::pending::<()>().await }),
        )));
        assert!(take_pump(&slot).is_some(), "the task must come out");
        assert!(
            take_pump(&slot).is_none(),
            "a slot yields its task exactly once"
        );
    }

    #[test]
    fn cleanup_timeout_is_generous() {
        // It must comfortably exceed the SDK's own 30 s HTTP deadline plus
        // the supervisor's SIGTERM grace period, or a healthy shutdown
        // would trip it.
        assert!(
            CLEANUP_TIMEOUT > std::time::Duration::from_secs(30),
            "{CLEANUP_TIMEOUT:?}"
        );
    }
}
