//! Async Rust SDK for cloudpipe — serverless Cloudflare tunnels.
//!
//! See [`TunnelBuilder`] for the entry point and [`TunnelHandle`] for the
//! returned handle. Subscribe to [`Event`]s via [`TunnelBuilder::on_event`]
//! to render your own UI.
//!
//! # Quickstart
//!
//! ```no_run
//! use cloudpipe_sdk::{Protocol, TunnelBuilder};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let mut handle = TunnelBuilder::new()
//!     .token(std::env::var("CLOUDFLARE_API_TOKEN")?)
//!     .domain("example.com")
//!     .protocol(Protocol::Http)
//!     .port(8080)
//!     .subdomain("myapp")
//!     .start()
//!     .await?;
//!
//! println!("live at {}", handle.url());
//!
//! // `wait` blocks until the tunnel exits on its own (e.g. cloudflared
//! // crashed or the 4h age limit was reached). It does NOT signal
//! // shutdown — pair it with a control signal and call `stop` yourself.
//! tokio::select! {
//!     _ = tokio::signal::ctrl_c() => handle.stop().await?,
//!     _ = handle.wait() => {}
//! }
//! # Ok(()) }
//! ```
//!
//! Account and zone are looked up automatically from the Cloudflare API.

#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

mod binary;
mod builder;
mod cleanup;
mod client;
mod error;
mod event;
mod handle;
mod protocol;
mod session;

pub use builder::TunnelBuilder;
pub use cleanup::cleanup;
pub use client::{CloudflareApi, DnsRecord, IngressEntry, Tunnel, Zone};
pub use error::{ApiError, CloudflareErrorKind, Error, InstallError, Result};
pub use event::{Event, LogLevel, ShutdownReason};
pub use handle::TunnelHandle;
pub use protocol::Protocol;
