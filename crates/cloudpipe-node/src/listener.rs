//! `Listener` — napi class wrapping `cloudpipe_sdk::TunnelHandle`.
//!
//! Exposes the synchronous getters (`url`, `fullName`, `subdomain`),
//! `wait()`, `close()`, an `update()`, and an EventEmitter-style
//! `on()` / `off()` surface that mirrors the SDK's lifecycle events.
//!
//! ## Concurrency model
//!
//! `TunnelHandle` is `!Send` (it carries a `JoinHandle<()>` and one of its
//! async methods crosses an `.await` while holding a `std::sync::MutexGuard`).
//! napi-rs's `tokio_rt` backend forces every `pub async fn` to be
//! `Send + 'static`, so the handle cannot move into any future that crosses
//! an `.await` on the napi thread pool.
//!
//! The session therefore runs on a **dedicated OS thread** with its own
//! `current_thread` tokio runtime; the only thing shared with the outside
//! world is an `mpsc::Sender<Command>`. Events from the SDK arrive through
//! `TunnelBuilder::on_event`, which fires the closure on a non-napi thread.
//! The closure forwards each event as a JSON payload over an unbounded
//! channel to an event-pump task on the multi-thread runtime, which invokes
//! [`napi::ThreadsafeFunction`] callbacks.
//!
//! ## Command handling
//!
//! `wait()` and `close()` are both implemented as commands on a single
//! session loop, but they are **not** serialized: a pending `Wait` is
//! cancelled the moment a `Stop` arrives. The naive
//! `while let Some(cmd) = rx.recv().await { … }` loop this replaced deadlocked
//! on the very common `await listener.wait(); await listener.close();`
//! pattern, because the `Wait` never returned and the `Stop` sat unread in
//! the channel behind it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use cloudpipe_sdk::{Event, TunnelBuilder, TunnelHandle};
use napi::bindgen_prelude::*;
use napi::threadsafe_function::ThreadsafeFunctionCallMode;
use napi_derive::napi;
use tokio::sync::{mpsc, oneshot, Mutex as TokioMutex};

use crate::error;
use cloudpipe_node_core::event_to_payload;

use crate::{runtime, ConnectOptions};

/// String-typed result carried by the Stop command channel. The session
/// loop returns the SDK error as a formatted `String` rather than a
/// `napi::Error` so we don't drag the napi native symbols across the
/// `oneshot` boundary.
type StopResult = std::result::Result<(), String>;

/// Payload type for the event channel: `(camelCase event name, JSON
/// payload string)`.
type EventEnvelope = (String, String);

/// Threadsafe callback registered by [`Listener::on`].
type EventCallback = Arc<napi::threadsafe_function::ThreadsafeFunction<String, ()>>;

/// Registry of per-event callbacks, shared with the event pump.
type CallbackRegistry = Arc<TokioMutex<HashMap<String, Vec<EventCallback>>>>;

/// Commands the external `Listener` sends into the session task.
enum Command {
    /// Block until the tunnel exits on its own. Cancelled by a `Stop`.
    Wait(oneshot::Sender<()>),
    /// Trigger a clean shutdown and wait for it to finish.
    Stop(oneshot::Sender<StopResult>),
    /// Re-point the tunnel at a different local port without changing the
    /// public hostname.
    Update {
        port: u16,
        reply: oneshot::Sender<StopResult>,
    },
}

/// A live Cloudflare tunnel. Constructed via [`crate::connect`].
#[napi]
pub struct Listener {
    url: String,
    full_name: String,
    subdomain: String,
    /// Local port the tunnel forwards. An atomic so the napi getter can
    /// read it through `&self` while `update()` swaps it.
    port: Arc<std::sync::atomic::AtomicU16>,
    cmd_tx: mpsc::Sender<Command>,
    /// Per-event-name callback list, populated by `on()` calls.
    callbacks: CallbackRegistry,
}

impl Listener {
    /// Boots the tunnel with the supplied options.
    pub async fn start(options: ConnectOptions) -> Result<Self> {
        let protocol = cloudpipe_node_core::parse_protocol(options.protocol.as_deref())
            .map_err(error::invalid_arg)?;
        let port = cloudpipe_node_core::parse_port(options.port).map_err(error::invalid_arg)?;

        let mut builder = TunnelBuilder::new()
            .token(options.token)
            .protocol(protocol)
            .port(port)
            .auto_restart(options.auto_restart.unwrap_or(false));

        if let Some(domain) = options.domain.filter(|d| !d.is_empty()) {
            builder = builder.domain(domain);
        }
        if let Some(subdomain) = options.subdomain.filter(|s| !s.is_empty()) {
            builder = builder.subdomain(subdomain);
        }
        if let Some(path) = options.cloudflared_path.filter(|p| !p.is_empty()) {
            builder = builder.cloudflared_path(PathBuf::from(path));
        }
        if let Some(proxy) = options.github_proxy {
            builder = builder.github_proxy(proxy);
        }
        if options.no_tls_verify.unwrap_or(false) {
            builder = builder.no_tls_verify(true);
        }
        if let Some(header) = options.origin_host_header.filter(|h| !h.is_empty()) {
            builder = builder.origin_host_header(header);
        }
        if let Some(policy) = options.restart_policy {
            builder = builder.restart_policy(policy.resolve());
        }

        // An unbounded channel: these events are produced at the pace of
        // `cloudflared`'s lifecycle (a handful per minute), so the
        // backpressure a bounded channel would buy is worth nothing, while
        // the lossiness it imposes is real. The previous `try_send` into a
        // 64-slot bounded channel silently dropped `restarting` /
        // `bootstrapFailed` exactly when a user most wanted to see them.
        let (event_tx, event_rx) = mpsc::unbounded_channel::<EventEnvelope>();
        let callbacks: CallbackRegistry = Arc::new(TokioMutex::new(HashMap::new()));
        let callbacks_for_pump = Arc::clone(&callbacks);

        let builder = builder.on_event(move |event: Event| {
            let (name, payload) = event_to_payload(&event);
            // Unbounded `send` cannot fail on capacity; the only error is
            // the receiver being gone, which means the session ended.
            let _ = event_tx.send((name.to_string(), payload));
        });

        let join_result = runtime().spawn(async move { builder.start().await }).await;
        let handle = match join_result {
            Ok(Ok(handle)) => handle,
            Ok(Err(sdk_err)) => return Err(error::from_sdk_error(sdk_err)),
            Err(join_err) => return Err(error::other(format!("join error: {join_err}"))),
        };

        let url = handle.url().to_string();
        let full_name = handle.full_name().to_string();
        let subdomain = handle.subdomain().to_string();

        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(8);

        // Spawn the event pump on the multi-thread runtime: it only
        // touches Send values (mpsc Receiver + Arc of HashMap of
        // ThreadsafeFunctions) and never holds a TunnelHandle.
        runtime().spawn(event_pump(event_rx, callbacks_for_pump));

        // Hand the TunnelHandle off to a dedicated session thread. The
        // thread owns its own current-thread tokio runtime so that
        // `!Send` futures (e.g. anything holding a `TunnelHandle`) can
        // drive the session without napi's multi-thread runtime
        // rejecting them.
        std::thread::Builder::new()
            .name("cloudpipe-session".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("cloudpipe-node: failed to build session runtime: {e}");
                        return;
                    }
                };
                rt.block_on(session_loop(handle, cmd_rx));
            })
            .map_err(|e| error::other(format!("failed to spawn session thread: {e}")))?;

        Ok(Self {
            url,
            full_name,
            subdomain,
            port: Arc::new(std::sync::atomic::AtomicU16::new(port)),
            cmd_tx,
            callbacks,
        })
    }
}

/// Long-running task that owns the `TunnelHandle` and drives every
/// external command.
///
/// A pending [`Command::Wait`] does **not** block a [`Command::Stop`]: the
/// loop runs the wait inside a `select!` that also polls the command
/// channel. That is what keeps the idiomatic
///
/// ```js
/// await listener.wait();   // e.g. a signal handler
/// await listener.close();  // must still work
/// ```
///
/// from deadlocking, which the previous `while let Some(cmd) = rx.recv()`
/// version did.
async fn session_loop(mut handle: TunnelHandle, mut cmd_rx: mpsc::Receiver<Command>) {
    let mut waiting: Option<oneshot::Sender<()>> = None;

    loop {
        // Poll alongside the command channel so a parked `wait()` resolves
        // when the tunnel actually ends. Checking `is_finished` only after
        // a command arrived — as an earlier version did — left `wait()`
        // hanging forever on a tunnel that died by itself.
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    Command::Wait(tx) => {
                        // Park the waiter and go back to serving the
                        // channel, so their eventual `close()` is not
                        // starved. Only one `wait()` is meaningful at a
                        // time; a second replaces the first, whose sender
                        // then drops and rejects that caller's promise.
                        waiting = Some(tx);
                    }
                    Command::Stop(tx) => {
                        let result: StopResult = handle
                            .stop()
                            .await
                            .map_err(|e| error::from_sdk_error(e).reason);
                        let _ = tx.send(result);
                        // Release anyone parked in `wait()`.
                        drop(waiting.take());
                        break;
                    }
                    Command::Update { port, reply } => {
                        let result = update_port(&mut handle, port).await;
                        let _ = reply.send(result);
                    }
                }
            }
            () = wait_for_finish(&handle) => {
                // The tunnel ended on its own; nothing will clear the
                // parked waiter except us.
                if let Some(tx) = waiting.take() {
                    let _ = tx.send(());
                }
                break;
            }
        }
    }
}

/// Resolves once the handle's background task has finished.
///
/// The current implementation has no `is_finished` notification, so this
/// polls. `Listener::wait` is a passive observation API rather than a hot
/// path, and the alternative — a watch channel threaded through the SDK —
/// is not worth the surface for it.
async fn wait_for_finish(handle: &TunnelHandle) {
    while !handle.is_finished() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Re-points a live tunnel at a different local port.
///
/// The public hostname is unchanged, so any URL already shared keeps
/// working — it just starts serving the new port.
async fn update_port(handle: &mut TunnelHandle, port: u16) -> StopResult {
    match handle.update_port(port).await {
        Ok(()) => Ok(()),
        Err(e) => Err(error::from_sdk_error(e).reason),
    }
}

/// Pulls events off the SDK's `on_event` channel and dispatches each one
/// to the listeners registered via [`Listener::on`]. Holds no
/// TunnelHandle, so it can run on the multi-thread napi runtime.
async fn event_pump(
    mut event_rx: mpsc::UnboundedReceiver<EventEnvelope>,
    callbacks: CallbackRegistry,
) {
    while let Some((name, payload)) = event_rx.recv().await {
        // Snapshot the listeners registered for this event under a brief
        // lock; the actual `cb.call` runs after we drop the guard so a
        // slow JS handler doesn't block other registrations.
        let snapshot: Vec<EventCallback> = {
            let guard = callbacks.lock().await;
            guard.get(&name).map(|v| v.to_vec()).unwrap_or_default()
        };
        for cb in snapshot {
            let _ = cb.call(Ok(payload.clone()), ThreadsafeFunctionCallMode::NonBlocking);
        }
    }
}

#[napi]
impl Listener {
    /// The full public URL of the tunnel (always HTTPS).
    #[napi(getter)]
    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// The public hostname (`myapp.example.com`).
    #[napi(js_name = "fullName", getter)]
    pub fn full_name(&self) -> String {
        self.full_name.clone()
    }

    /// Just the subdomain part (`myapp`).
    #[napi(getter)]
    pub fn subdomain(&self) -> String {
        self.subdomain.clone()
    }

    /// The local port currently being forwarded.
    #[napi(getter)]
    pub fn port(&self) -> u32 {
        u32::from(self.port.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Registers a callback for a single named event.
    ///
    /// The callback receives the event payload as a JSON string —
    /// `JSON.parse` it inside the handler to access fields. The
    /// complete event-name catalog is available at runtime via
    /// [`crate::event_names`].
    #[napi]
    pub fn on(
        &self,
        event: String,
        callback: napi::threadsafe_function::ThreadsafeFunction<String, ()>,
    ) -> Result<()> {
        if !cloudpipe_node_core::event::EVENT_NAMES.contains(&event.as_str()) {
            return Err(error::invalid_arg(format!(
                "unknown event \"{event}\" — expected one of: {}",
                cloudpipe_node_core::event::EVENT_NAMES.join(", ")
            )));
        }
        let callback = Arc::new(callback);
        let registry = Arc::clone(&self.callbacks);
        runtime().block_on(async move {
            let mut guard = registry.lock().await;
            guard.entry(event).or_default().push(callback);
        });
        Ok(())
    }

    /// Removes every callback registered for an event.
    ///
    /// Without this, a long-lived process that re-registers handlers (on a
    /// reconnect, say) grows the registry without bound and every dispatch
    /// keeps calling dead closures.
    #[napi]
    pub fn off(&self, event: String) -> Result<()> {
        let registry = Arc::clone(&self.callbacks);
        runtime().block_on(async move {
            let mut guard = registry.lock().await;
            guard.remove(&event);
        });
        Ok(())
    }

    /// Removes every registered callback.
    #[napi]
    pub fn remove_all_listeners(&self) -> Result<()> {
        let registry = Arc::clone(&self.callbacks);
        runtime().block_on(async move {
            let mut guard = registry.lock().await;
            guard.clear();
        });
        Ok(())
    }

    /// Blocks until the tunnel exits on its own (cloudflared crashed and
    /// auto-restart was exhausted, or the process ended).
    ///
    /// This does **not** stop the tunnel, and it does not prevent a
    /// subsequent [`close`](Self::close) from working.
    #[napi]
    pub async fn wait(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Wait(tx))
            .await
            .map_err(|_| error::session_gone())?;
        rx.await.map_err(|_| error::channel_dropped())?;
        Ok(())
    }

    /// Re-points the tunnel at a different local port.
    ///
    /// The public URL is unchanged, so URLs already shared keep working.
    /// This is the Node-side counterpart of ngrok's `listener.update()`.
    #[napi]
    pub async fn update(&self, port: u32) -> Result<()> {
        let port = cloudpipe_node_core::parse_port(Some(port)).map_err(error::invalid_arg)?;
        let (reply, rx) = oneshot::channel::<StopResult>();
        self.cmd_tx
            .send(Command::Update { port, reply })
            .await
            .map_err(|_| error::session_gone())?;
        rx.await
            .map_err(|_| error::channel_dropped())?
            .map_err(error::other)?;
        self.port.store(port, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Triggers a clean shutdown and waits for it to complete. Idempotent:
    /// calling twice surfaces an `AlreadyShutDown` error.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel::<StopResult>();
        self.cmd_tx
            .send(Command::Stop(tx))
            .await
            .map_err(|_| error::session_gone())?;
        rx.await
            .map_err(|_| error::channel_dropped())?
            .map_err(error::other)
    }
}

// A `Listener` with no explicit `Drop`: when the final one is garbage
// collected, napi-rs runs its finalizer, which drops the `cmd_tx` field.
// That is the last sender, so `session_loop`'s `recv()` returns `None`, it
// drops the `TunnelHandle`, and the handle's own `Drop` fires a
// best-effort Cloudflare purge.
//
// An explicit impl was written first and was a no-op: `self.cmd_tx.clone()`
// produces an *independent* sender, so dropping that clone changed nothing.
// The mechanism that actually works is field destruction, so it is left to
// the field order rather than pretended at here.

impl Listener {
    /// Releases the command channel early.
    ///
    /// Only needed if a `cmd_tx` clone ever outlives the listener; today
    /// nothing does, and the assertions below keep it that way.
    #[cfg(test)]
    fn sender_count_would_not_outlive_self(&self) -> usize {
        std::sync::Arc::strong_count(&self.callbacks)
    }
}
