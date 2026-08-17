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

/// Why a tunnel began shutting down.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ShutdownReason {
    /// User called [`crate::TunnelHandle::stop`] (or the program is exiting).
    UserRequested,
    /// The 4-hour age limit was reached.
    Timeout,
    /// `cloudflared` exited unexpectedly on its own.
    ChildExited,
    /// An internal task reported an unrecoverable error.
    Error(String),
}
