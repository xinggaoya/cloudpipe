//! Napi-free logic shared by the Node.js binding layer.
//!
//! Everything in this crate is a plain function or a plain enum with **no
//! dependency on `napi`**. That separation exists for a concrete reason:
//! `cloudpipe-node` is a `cdylib`, so a `cargo test --lib` harness for it
//! fails to link (`undefined symbol: napi_call_threadsafe_function`) —
//! the napi symbols only resolve inside a Node.js process.
//!
//! Moving the mapping/parsing/formatting logic here means it is covered by
//! real unit tests on `cargo test -p cloudpipe-node-core`, and it leaves
//! `cloudpipe-node` holding nothing but `#[napi]`-annotated shells that
//! delegate here.

#![deny(missing_docs)]

pub mod error;
pub mod event;
pub mod options;
pub mod parse;

pub use error::{CloudflareApiKind, CloudpipeErrorCode};
pub use event::event_to_payload;
pub use options::{parse_port, parse_protocol};
pub use parse::extract_code;
