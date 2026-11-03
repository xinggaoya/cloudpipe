//! Crash-loop detection: when is a restart worth trying again?
//!
//! # The problem
//!
//! `cloudflared` can die for two very different reasons:
//!
//! - **A transient crash** — the edge connection dropped, the machine
//!   slept, the process was OOM-killed. Restarting is exactly the right
//!   response, and the tunnel comes straight back.
//! - **A fatal startup failure** — the token is invalid, the credentials
//!   cannot be read, the binary is not runnable. The next start will fail
//!   in exactly the same way, because nothing about the environment
//!   changed.
//!
//! The supervisor used to treat both alike and restart forever. A user
//! whose `cloudflared` could not start at all watched the CLI loop
//! ("restarting (attempt #2)", "attempt #3}", …) until they killed it,
//! with no reason on screen. The README had claimed a bounded retry
//! budget all along — the budget simply was not applied to child exits.
//!
//! # The rule
//!
//! A generation that stayed up for less than [`UNSTABLE_WINDOW`] counts
//! as a failed attempt. Once [`RestartPolicy::max_attempts`] consecutive
//! failures have accumulated, the session stops and reports why.
//!
//! A generation that outlived the window resets the counter: the tunnel
//! was working, so the next crash is a fresh incident rather than part
//! of a loop. That is what keeps a long-lived tunnel that crashes once a
//! month from being given up on.

use std::time::Duration;

use crate::session::policy::RestartPolicy;

/// How long a generation must survive to count as "it was working".
///
/// Ten seconds is comfortably longer than `cloudflared`'s own startup
/// (process launch → TLS handshake → first edge registration) and short
/// enough that a genuinely broken configuration is caught in seconds
/// rather than minutes.
pub const UNSTABLE_WINDOW: Duration = Duration::from_secs(10);

/// What the supervisor should do after a generation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrashDecision {
    /// Try again. `attempt` is the 1-based number of this failure.
    Retry {
        /// The failure count, 1-based.
        attempt: u32,
    },
    /// Stop restarting. `attempts` is how many failures accumulated.
    GiveUp {
        /// How many consecutive failures were observed.
        attempts: u32,
    },
}

/// Tracks consecutive failures of a `cloudflared` generation.
#[derive(Debug, Clone)]
pub(crate) struct CrashGuard {
    /// How long a generation must survive to count as healthy.
    window: Duration,
    /// How many consecutive failures are tolerated.
    budget: u32,
    /// Failures accumulated so far.
    failures: u32,
}

impl CrashGuard {
    /// A guard with the default window and the given budget.
    pub(crate) fn new(budget: u32) -> Self {
        Self::with_window(budget, UNSTABLE_WINDOW)
    }

    /// A guard with an explicit window — the seam the tests use to avoid
    /// sleeping for ten seconds.
    pub(crate) fn with_window(budget: u32, window: Duration) -> Self {
        Self {
            window,
            budget,
            failures: 0,
        }
    }

    /// Records that a generation ended after having been up for
    /// `uptime`, and says whether to try again.
    pub(crate) fn record(&mut self, uptime: Duration) -> CrashDecision {
        if uptime >= self.window {
            // The generation was healthy, so this exit opens a new
            // incident rather than continuing the previous one.
            self.failures = 1;
        } else {
            self.failures = self.failures.saturating_add(1);
        }

        if self.failures >= self.budget {
            CrashDecision::GiveUp {
                attempts: self.failures.max(1),
            }
        } else {
            CrashDecision::Retry {
                attempt: self.failures,
            }
        }
    }

    /// The failures accumulated so far.
    #[cfg(test)]
    pub(crate) fn failures(&self) -> u32 {
        self.failures
    }
}

/// Builds a guard from the session's restart policy.
pub(crate) fn guard_for(policy: &RestartPolicy) -> CrashGuard {
    CrashGuard::new(policy.max_attempts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How long a generation has to stay up to count as healthy.
    const WINDOW: Duration = Duration::from_millis(50);
    /// A generation that died the moment it started.
    const INSTANT: Duration = Duration::from_millis(1);
    /// A generation that ran for a long time.
    const SLOW: Duration = Duration::from_secs(600);

    #[test]
    fn a_generation_that_dies_immediately_is_retried_until_the_budget_is_spent() {
        let mut guard = CrashGuard::with_window(3, WINDOW);
        assert_eq!(guard.record(INSTANT), CrashDecision::Retry { attempt: 1 });
        assert_eq!(guard.record(INSTANT), CrashDecision::Retry { attempt: 2 });
        assert_eq!(guard.record(INSTANT), CrashDecision::GiveUp { attempts: 3 });
    }

    #[test]
    fn a_generation_that_stayed_up_resets_the_counter() {
        let mut guard = CrashGuard::with_window(3, WINDOW);
        assert_eq!(guard.record(INSTANT), CrashDecision::Retry { attempt: 1 });
        assert_eq!(guard.record(INSTANT), CrashDecision::Retry { attempt: 2 });
        // This one was healthy: the next crash starts a new incident.
        assert_eq!(guard.record(SLOW), CrashDecision::Retry { attempt: 1 });
        assert_eq!(guard.failures(), 1);
    }

    #[test]
    fn a_zero_budget_never_retries() {
        let mut guard = CrashGuard::with_window(0, WINDOW);
        assert_eq!(
            guard.record(INSTANT),
            CrashDecision::GiveUp { attempts: 1 },
            "a policy with no retries must stop at the first crash"
        );
    }

    #[test]
    fn the_default_window_is_long_enough_for_cloudflared_to_start() {
        // If the window were shorter than `cloudflared`'s own startup,
        // every healthy start would count as a failure.
        assert!(
            UNSTABLE_WINDOW >= Duration::from_secs(5),
            "{UNSTABLE_WINDOW:?}"
        );
    }

    #[test]
    fn the_guard_is_built_from_the_restart_policy() {
        let policy = RestartPolicy {
            max_attempts: 2,
            ..RestartPolicy::default()
        };
        let mut guard = guard_for(&policy);
        assert_eq!(guard.record(INSTANT), CrashDecision::Retry { attempt: 1 });
        assert_eq!(guard.record(INSTANT), CrashDecision::GiveUp { attempts: 2 });
    }
}
