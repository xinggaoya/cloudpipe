//! Logging setup for the CLI.
//!
//! The subscriber itself lives in the SDK (`cloudpipe_sdk::logging`) so
//! that the CLI and the Node addon cannot drift apart, and so that an
//! SDK embedder gets a supported way to see the SDK's own trace.

pub use cloudpipe_sdk::logging::{init, LogSetting};
