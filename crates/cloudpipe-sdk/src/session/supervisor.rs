//! The supervision loop: keep `cloudflared` alive across crashes.
//!
//! Split into two explicit phases so a bootstrap failure can never be
//! confused with a child exit:
//!
//! 1. **wait-for-exit** — a child is running; block until it ends, then
//!    stop the process and purge Cloudflare.
//! 2. **retry-bootstrap** — no child exists; retry creation with backoff
//!    until it succeeds, the user stops, or the budget is spent.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, Mutex};
use tracing::{debug, warn};

use crate::error::Result;
use crate::event::{Event, ShutdownReason};
use crate::handle::{Shutdown, StderrSlot};
use crate::session::bootstrap;
use crate::session::state::{SessionConfig, SessionState};

/// Everything the supervisor task needs, bundled so the loop body stays
/// readable. Built once at the start of the task and never reallocated.
pub(crate) struct Supervisor {
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
    /// Assembles a supervisor around the first `cloudflared` generation.
    pub(crate) fn new(
        child: tokio::process::Child,
        state: Arc<Mutex<SessionState>>,
        stderr_slot: StderrSlot,
        cfg: SessionConfig,
    ) -> Self {
        let full_name = format!("{}.{}", cfg.subdomain, cfg.domain);
        let auto_restart = cfg.auto_restart;
        Self {
            child,
            state,
            stderr_slot,
            shutdown: cfg.shutdown.clone(),
            event_tx: cfg.event_tx.clone(),
            cfg,
            full_name,
            auto_restart,
        }
    }

    /// Drives the tunnel until the user shuts it down, or auto-restart is
    /// exhausted.
    pub(crate) async fn run(mut self) {
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
            let _ = self.event_tx.send(Event::Restarting { reason, attempt });

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

            match bootstrap::session(&self.cfg, &self.full_name, false).await {
                Ok(next) => {
                    {
                        let mut guard = self.state.lock().await;
                        guard.retarget(next.state.clone());
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
