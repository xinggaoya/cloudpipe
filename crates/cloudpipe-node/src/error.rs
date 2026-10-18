//! Thin napi-facing wrappers around `cloudpipe-node-core`.
//!
//! napi-rs's async-fn machinery hard-codes `Into<napi::Error>`, which
//! defaults to `Into<Error<Status>>`. There is no way for an `async fn`
//! exported via `#[napi]` to surface a custom status type to JavaScript.
//! So all errors leave the Rust side as `napi::Error<Status>`, with the
//! cloudpipe taxonomy encoded as a `[CODE]` prefix on the message.
//!
//! The classification itself lives in `cloudpipe_node_core::error` where
//! it is unit-tested; this module only adds the `napi::Error` skin.

use napi::Error as NapiError;
use napi::Status;

/// Alias that pins the napi error's `status` slot to the `napi::Status`
/// enum. `napi::Error<S>` is generic over `S: AsRef<str>`, and the bare
/// `napi::Error` in async-fn return position forces the compiler to fix
/// the slot. Pinning it once at the module boundary removes the churn.
pub(crate) type FixedError = NapiError<Status>;

/// Translates any [`cloudpipe_sdk::Error`] into a napi `Error` whose
/// `message` carries the cloudpipe code as a `[CODE]` prefix.
pub(crate) fn from_sdk_error(err: cloudpipe_sdk::Error) -> FixedError {
    let prefix = cloudpipe_node_core::error::error_prefix(&err);
    NapiError::new(Status::GenericFailure, format!("{prefix}{err}"))
}

/// Generic fall-through for things that have no SDK equivalent (e.g.
/// a tokio join error or a thread-spawn failure).
pub(crate) fn other(msg: impl Into<String>) -> FixedError {
    let msg = msg.into();
    NapiError::new(
        Status::GenericFailure,
        format!("{}{msg}", cloudpipe_node_core::error::other_prefix()),
    )
}

/// Maps a [`Status::InvalidArg`] when the JS side supplies a value
/// outside the Rust SDK's accepted range (e.g. port > 65535 or an
/// unknown protocol string).
pub(crate) fn invalid_arg(msg: impl Into<String>) -> FixedError {
    NapiError::new(Status::InvalidArg, msg.into())
}

/// Channel dropped its reply before the session task could answer —
/// surfaces as `OTHER`.
pub(crate) fn channel_dropped() -> FixedError {
    other("session task dropped the reply channel")
}

/// The session task's sender side is gone (handle was dropped, thread
/// panicked and shut down, etc.).
pub(crate) fn session_gone() -> FixedError {
    other("session task is gone")
}
