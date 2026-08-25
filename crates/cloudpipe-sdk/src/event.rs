//! Lifecycle events emitted by a running tunnel.
//!
//! Subscribers receive [`Event`] values via the closure passed to
//! [`crate::TunnelBuilder::on_event`], or by calling
//! [`crate::TunnelHandle::subscribe`] to obtain a `broadcast::Receiver`.

use crate::protocol::Protocol;

/// A lifecycle event.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Event {
    /// SDK banner — the very first event on a fresh `start()`.
    Banner,

    /// The SDK is querying Cloudflare for a pre-existing tunnel/DNS with the
    /// chosen name.
    ResolvingConflicts,

    /// A new tunnel is being created on Cloudflare.
    CreatingTunnel {
        /// Tunnel name (== subdomain).
        name: String,
    },

    /// The tunnel's remote ingress configuration has been written.
    IngressConfigured {
        /// Local protocol scheme.
        protocol: Protocol,
        /// Local port.
        port: u16,
    },

    /// The DNS CNAME record has been created.
    DnsCreated {
        /// Full hostname (e.g. `myapp.example.com`).
        full_name: String,
    },

    /// `cloudflared` was spawned successfully.
    CloudflaredStarted,

    /// The previous tunnel session ended and a new one is being created on
    /// the same URL. Only emitted when auto-restart is enabled.
    Restarting {
        /// Why the previous session ended.
        reason: ShutdownReason,
        /// 1-based restart attempt counter — the *next* attempt will be
        /// `attempt` (so the very first restart after the initial run is
        /// `attempt = 2`).
        attempt: u32,
    },

    /// A new tunnel session has finished bootstrapping and `cloudflared`
    /// is running again on the same URL. Only emitted when auto-restart
    /// is enabled.
    Restarted {
        /// 1-based restart counter — matches the `attempt` value of the
        /// most recent [`Event::Restarting`].
        attempt: u32,
    },

    /// The auto-restart loop has exhausted its retry budget (consecutive
    /// bootstrap failures) and is giving up. The session is about to
    /// enter normal shutdown. Only emitted when auto-restart is enabled.
    RestartGivingUp {
        /// How many consecutive bootstrap failures occurred.
        attempts: u32,
        /// The last error string returned by the Cloudflare API or
        /// `cloudflared` spawn.
        last_error: String,
    },

    /// A new edge connection was registered. `cloudflared` establishes up to
    /// four QUIC connections to the edge.
    EdgeConnected {
        /// 0-based connection index.
        conn_index: u8,
        /// Total connections established so far.
        total: u8,
    },

    /// A log line emitted by `cloudflared` after filtering.
    CloudflaredLog {
        /// Log severity, classified from the stderr line.
        level: LogLevel,
        /// Raw line, already trimmed.
        line: String,
    },

    /// The tunnel is starting to shut down. Resources are still being
    /// released; see [`Event::Cleaned`] for the terminal state.
    ShuttingDown {
        /// Why the shutdown started.
        reason: ShutdownReason,
    },

    /// All Cloudflare-side resources have been cleaned up. The last event
    /// of a tunnel's lifetime.
    Cleaned,
}

/// Severity of a [`Event::CloudflaredLog`] line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Informational noise.
    Info,
    /// Worth showing but not fatal.
    Warn,
    /// A real error from `cloudflared`.
    Error,
}

/// Why a tunnel session ended (and is potentially being restarted).
///
/// Note: with auto-restart enabled, a `ChildExited` or `Error` reason does
/// **not** mark the tunnel as dead — the SDK will respawn `cloudflared` on
/// the same URL and the next event will be [`Event::Restarting`] followed
/// by [`Event::Restarted`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ShutdownReason {
    /// User called [`crate::TunnelHandle::stop`] (or the program is exiting).
    /// The session is final — no auto-restart will be attempted.
    UserRequested,
    /// `cloudflared` exited unexpectedly on its own.
    ChildExited,
    /// An internal task reported an unrecoverable error (typically a
    /// failed `try_wait` on the child process).
    Error(String),
}
