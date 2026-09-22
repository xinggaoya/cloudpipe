//! `Listener` — napi class wrapping `cloudpipe_sdk::TunnelHandle`.
//!
//! Exposes the synchronous getters (`url`, `subdomain`, `fullName`),
//! `wait()`, `close()`, and an EventEmitter-style `on()` / `once()`
//! surface that mirrors the SDK's lifecycle events.
//!
//! ## Concurrency model
//!
//! `TunnelHandle` is `!Send` (it carries a `JoinHandle<()>` and a
//! `std::sync::Mutex<Option<JoinHandle<()>>>` and one of its async
//! methods crosses an `.await` while holding a `std::sync::MutexGuard`).
//! napi-rs's `tokio_rt` backend forces every `pub async fn` to be
//! `Send + 'static` because the future is spawned onto a multi-thread
//! runtime owned by the addon. That means we cannot move the handle into
//! any future that crosses an `.await` on the napi thread pool.
//!
//! The session task runs on a **dedicated OS thread** with its own
//! `current_thread` tokio runtime; the only thing shared with the
//! outside world is an `mpsc::Sender<Command>`. Events from the SDK
//! arrive through `TunnelBuilder::on_event`, which fires the closure on
//! a non-napi thread. The closure forwards each event as a JSON
//! payload over a separate `mpsc` channel to an event-pump task that
//! lives on the multi-thread runtime and invokes
//! [`napi::ThreadsafeFunction`] callbacks.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use cloudpipe_sdk::{Event, TunnelBuilder, TunnelHandle};
use napi::bindgen_prelude::*;
use napi::threadsafe_function::ThreadsafeFunctionCallMode;
use napi_derive::napi;
use tokio::sync::{mpsc, oneshot, Mutex as TokioMutex};

use crate::error;
use crate::event::event_to_payload;
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

/// Commands the external `Listener` sends into the session task.
enum Command {
    /// Block until the tunnel exits on its own.
    Wait(oneshot::Sender<()>),
    /// Trigger a clean shutdown and wait for it to finish.
    Stop(oneshot::Sender<StopResult>),
}

/// A live Cloudflare tunnel. Constructed via [`crate::connect`].
#[napi]
pub struct Listener {
    url: String,
    full_name: String,
    subdomain: String,
    cmd_tx: mpsc::Sender<Command>,
    /// Per-event-name callback list, populated by `on()` calls.
    callbacks: Arc<TokioMutex<HashMap<String, Vec<EventCallback>>>>,
}

impl Listener {
    /// Boots the tunnel with the supplied options.
    pub async fn start(options: ConnectOptions) -> Result<Self> {
        let protocol = options.protocol_enum().map_err(error::invalid_arg)?;
        let port = port_u16(options.port).map_err(error::invalid_arg)?;

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

        let (event_tx, event_rx) = mpsc::channel::<EventEnvelope>(64);
        let callbacks: Arc<TokioMutex<HashMap<String, Vec<EventCallback>>>> =
            Arc::new(TokioMutex::new(HashMap::new()));
        let callbacks_for_pump = callbacks.clone();

        let builder = builder.on_event(move |event: Event| {
            let (name, payload) = event_to_payload(&event);
            // `blocking_send` is fine here: `on_event` runs inside the
            // SDK's session task, which is itself a non-async thread.
            let _ = event_tx.blocking_send((name.to_string(), payload));
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
            cmd_tx,
            callbacks,
        })
    }
}

/// Long-running task that owns the `TunnelHandle` and serializes every
/// external command. Exits when the sender side is dropped or a `Stop`
/// command completes.
async fn session_loop(mut handle: TunnelHandle, mut cmd_rx: mpsc::Receiver<Command>) {
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Command::Wait(tx) => {
                handle.wait().await;
                let _ = tx.send(());
            }
            Command::Stop(tx) => {
                let result: StopResult = handle
                    .stop()
                    .await
                    .map_err(|e| error::from_sdk_error(e).reason);
                let _ = tx.send(result);
                break;
            }
        }
    }
}

/// Pulls events off the SDK's `on_event` channel and dispatches each
/// one to the listeners registered via [`Listener::on`]. Holds no
/// TunnelHandle, so it can run on the multi-thread napi runtime.
async fn event_pump(
    mut event_rx: mpsc::Receiver<EventEnvelope>,
    callbacks: Arc<TokioMutex<HashMap<String, Vec<EventCallback>>>>,
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

/// Coerces the JS-side `Option<u32>` to a `u16` port.
///
/// Returns a plain `String` error so the function can be unit-tested
/// without linking the napi native symbols (which only resolve at
/// Node.js runtime). The call site wraps the message into `napi::Error`.
pub(crate) fn port_u16(value: Option<u32>) -> std::result::Result<u16, String> {
    match value {
        None => Ok(8080),
        Some(p) => u16::try_from(p).map_err(|_| format!("port {p} is out of range")),
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

    /// Registers a callback for a single named event.
    ///
    /// The callback receives the event payload as a JSON string —
    /// `JSON.parse` it inside the handler to access fields. The
    /// complete event-name catalog is documented on the package
    /// README.
    #[napi]
    pub fn on(
        &self,
        event: String,
        callback: napi::threadsafe_function::ThreadsafeFunction<String, ()>,
    ) -> Result<()> {
        let callback = Arc::new(callback);
        let listener = self.clone_callbacks();
        runtime().block_on(async move {
            let mut guard = listener.lock().await;
            guard.entry(event).or_default().push(callback);
        });
        Ok(())
    }

    /// Blocks until the tunnel exits on its own (cloudflared crashed,
    /// auto-restart exhausted, etc.).
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

    /// Triggers a clean shutdown and waits for it to complete. Idempotent:
    /// calling twice surfaces an `AlreadyShutDown` error.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel::<StopResult>();
        self.cmd_tx
            .send(Command::Stop(tx))
            .await
            .map_err(|_| error::session_gone())?;
        let result: StopResult = rx.await.map_err(|_| error::channel_dropped())?;
        result.map_err(error::other)
    }
}

impl Listener {
    fn clone_callbacks(&self) -> Arc<TokioMutex<HashMap<String, Vec<EventCallback>>>> {
        self.callbacks.clone()
    }
}
