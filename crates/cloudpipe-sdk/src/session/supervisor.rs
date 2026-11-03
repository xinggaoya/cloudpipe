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
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, Mutex};
use tracing::{debug, warn};

use crate::binary::{self, LineKind};
use crate::error::Result;
use crate::event::{Event, ShutdownReason};
use crate::handle::{Outcome, OutcomeSlot, Shutdown, StderrSlot};
use crate::session::bootstrap;
use crate::session::crashloop::{self, CrashDecision, CrashGuard};
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
    /// When the current generation was spawned. The crash guard needs to
    /// know whether the child had time to actually work.
    generation_started: Instant,
    /// Counts consecutive starts that failed.
    guard: CrashGuard,
    /// Where the terminal outcome is published.
    outcome_slot: OutcomeSlot,
}

impl Supervisor {
    /// Assembles a supervisor around the first `cloudflared` generation.
    pub(crate) fn new(
        child: tokio::process::Child,
        state: Arc<Mutex<SessionState>>,
        stderr_slot: StderrSlot,
        outcome_slot: OutcomeSlot,
        cfg: SessionConfig,
    ) -> Self {
        let full_name = format!("{}.{}", cfg.subdomain, cfg.domain);
        let auto_restart = cfg.auto_restart;
        let guard = crashloop::guard_for(&cfg.restart_policy);
        Self {
            child,
            state,
            stderr_slot,
            shutdown: cfg.shutdown.clone(),
            event_tx: cfg.event_tx.clone(),
            cfg,
            full_name,
            auto_restart,
            generation_started: Instant::now(),
            guard,
            outcome_slot,
        }
    }

    /// Publishes how the session ended.
    ///
    /// Written once, at the very end of the session, so a caller that
    /// waits for the session to finish can read the outcome without
    /// racing the event stream.
    fn finish(&self, outcome: Outcome) {
        let mut guard = self.outcome_slot.lock().expect("outcome slot poisoned");
        if guard.is_none() {
            *guard = Some(outcome);
        }
    }

    /// Drives the tunnel until the user shuts it down, or auto-restart is
    /// exhausted.
    pub(crate) async fn run(mut self) {
        let mut attempt: u32 = 1;

        loop {
            // ---- Phase 1: a child is running; wait for it to end -------
            let (reason, status) = self.wait_for_exit().await;
            debug!(?reason, "supervisor: cloudflared generation ended");

            // Stop the process *before* touching Cloudflare. The old order
            // deleted the DNS record and the tunnel while `cloudflared` was
            // still serving, so every in-flight request failed and "clean
            // exit" was not clean.
            self.terminate_child().await;
            let stderr = self.drain_stderr_pump().await;
            let code = status.and_then(|status| status.code());

            // The reason the child died, published *before* the cleanup
            // noise so a consumer can show it first.
            let _ = self.event_tx.send(Event::CloudflaredExited {
                code,
                stderr: stderr.clone(),
            });

            let snapshot = self.state.lock().await.clone();
            snapshot.purge().await;

            if matches!(reason, ShutdownReason::UserRequested) {
                self.finish(Outcome::Stopped);
                let _ = self.event_tx.send(Event::ShuttingDown { reason });
                break;
            }

            let _ = self.event_tx.send(Event::ShuttingDown {
                reason: reason.clone(),
            });
            if !self.auto_restart {
                self.finish(Outcome::Crashed);
                break;
            }

            // ---- Crash-loop guard ------------------------------------
            // A child that dies the moment it starts will die again the
            // moment it is restarted. The budget bounds that, so a
            // misconfiguration ends the session with an explanation
            // instead of looping forever.
            let uptime = self.generation_started.elapsed();
            match self.guard.record(uptime) {
                CrashDecision::GiveUp { attempts } => {
                    let last_error = describe_exit(code, &stderr);
                    warn!(
                        attempts,
                        "supervisor: giving up after {attempts} failed starts: {last_error}"
                    );
                    self.finish(Outcome::GaveUp);
                    let _ = self.event_tx.send(Event::RestartGivingUp {
                        attempts,
                        last_error,
                    });
                    break;
                }
                CrashDecision::Retry { .. } => {}
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
                    self.finish(Outcome::GaveUp);
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
                        *guard = Some(next.stderr_pump);
                    }
                    // The new generation starts its own uptime clock: the
                    // crash guard judges each generation on its own.
                    self.generation_started = Instant::now();
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
    ///
    /// The exit status is returned alongside the reason: it is the first
    /// half of "why did it die", the stderr tail being the second.
    async fn wait_for_exit(&mut self) -> (ShutdownReason, Option<std::process::ExitStatus>) {
        if self.shutdown.is_triggered() {
            return (ShutdownReason::UserRequested, None);
        }
        tokio::select! {
            biased;
            _ = self.shutdown.notified() => (ShutdownReason::UserRequested, None),
            status = self.child.wait() => match status {
                Ok(status) => {
                    debug!("cloudflared exited with {status:?}");
                    (ShutdownReason::ChildExited, Some(status))
                }
                Err(err) => {
                    warn!("wait on child failed: {err}");
                    (ShutdownReason::Error(err.to_string()), None)
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

    /// Waits for the stderr pump to reach EOF and returns the tail of
    /// what the child printed.
    ///
    /// Awaiting the pump (rather than aborting it) is what makes the
    /// reason a crash happened available: the pump ends when the child's
    /// stderr closes, so its last line — the fatal message — is in the
    /// tail by the time this returns.
    async fn drain_stderr_pump(&mut self) -> String {
        let Some(pump) = crate::handle::take_pump(&self.stderr_slot) else {
            return String::new();
        };
        let tail = pump.tail.clone();
        let mut task = pump.task;
        if tokio::time::timeout(STDERR_DRAIN_TIMEOUT, &mut task)
            .await
            .is_err()
        {
            // The pipe is still open (a grandchild inherited it). Whatever
            // we have is the best we will get.
            debug!("supervisor: stderr pump did not finish within the drain window");
            task.abort();
        }
        // A poisoned tail is a reporting problem, not a reason to take
        // the whole tunnel down.
        let guard = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.snapshot()
    }
}

/// How long to wait for the stderr pump to reach EOF after the child
/// exited.
///
/// The pump ends when the child's stderr closes, which happens as the
/// child exits — so the last line it printed is in the tail by the time
/// the pump finishes. The timeout only guards against a grandchild that
/// inherited the pipe and keeps it open.
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// Builds a one-line explanation of why `cloudflared` ended.
///
/// Prefers the most recent *error* line of the tail — for a fatal
/// startup failure that is the line that names the cause — and falls
/// back to the last line of all.
fn describe_exit(code: Option<i32>, stderr: &str) -> String {
    let head = match code {
        Some(code) => format!("cloudflared exited with code {code}"),
        None => "cloudflared was terminated by a signal".to_string(),
    };
    let reason = stderr
        .lines()
        .rev()
        .find(|line| matches!(binary::classify_line(line), LineKind::Error))
        .or_else(|| stderr.lines().last());
    match reason {
        Some(line) => format!("{head}: {line}"),
        None => head,
    }
}

/// Grace period between SIGTERM and SIGKILL on shutdown.
const TERMINATE_GRACE: Duration = Duration::from_secs(2);

/// Sends `SIGTERM` to `pid`.
///
/// The workspace denies `unsafe_code`, so this is the one place in the
/// tree that opts back in — `tokio::process::Child::kill` only offers
/// SIGKILL, and skipping the graceful step is what made shutdown drop
/// in-flight requests before this existed.
#[cfg(unix)]
#[allow(unsafe_code)]
fn signal_term(pid: i32) -> std::io::Result<()> {
    // SAFETY:
    // - `kill(2)` is async-signal-safe and has no memory-safety
    //   requirements; the only failure mode is a stale pid.
    // - `pid` comes from `tokio::process::Child::id()`, so it is a process
    //   this crate spawned and is the direct parent of — we never signal
    //   anything we did not create, and the `Child` is still alive here so
    //   the pid has not been recycled yet.
    // - `SIGTERM` (15) is the portable default termination request and
    //   asks `cloudflared` to close its QUIC sessions rather than dropping
    //   them.
    let rc = unsafe { libc_kill(pid, SIGNAL_TERM) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// `SIGTERM` on every Unix platform this builds for.
#[cfg(unix)]
const SIGNAL_TERM: i32 = 15;

#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}
