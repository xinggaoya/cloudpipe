# cloudpipe-sdk

Async Rust SDK for [cloudpipe](https://github.com/xinggaoya/cloudpipe) —
expose a local port to the public internet via a Cloudflare Tunnel from
your own program, with **no backend service** involved.

```rust
use std::time::Duration;

use cloudpipe_sdk::{Protocol, RestartPolicy, TunnelBuilder};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut handle = TunnelBuilder::new()
        .token(std::env::var("CLOUDFLARE_API_TOKEN")?)
        .domain("example.com")
        .protocol(Protocol::Http)
        .port(8080)
        .subdomain("myapp")
        // Keep the tunnel alive across cloudflared crashes. The budget is
        // tunable; these are the defaults.
        .auto_restart(true)
        .restart_policy(RestartPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        })
        .on_event(|event| println!("{event:?}"))
        .start()
        .await?;

    println!("Live at {}", handle.url());

    // `wait` blocks until the tunnel ends on its own. With auto-restart
    // enabled, cloudflared crashes are absorbed by the SDK and don't
    // return from `wait`; Ctrl+C triggers a clean stop.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => handle.stop().await?,
        _ = handle.wait() => {}
    }
    Ok(())
}
```

## Highlights

- **Pure async (tokio)** — no blocking calls leak into your runtime.
- **Bring your own credentials** — pass them in code, or reuse the CLI's
  `~/.cfp/config.json` layout.
- **Event stream** — `on_event` receives lifecycle events so you render
  your own UI.
- **Safe by default** — every Cloudflare resource created during startup is
  rolled back if a later step fails, so a DNS or `cloudflared` failure never
  orphans a tunnel. `Drop` on `TunnelHandle` performs a best-effort purge
  too, and [`cleanup`] reclaims anything a hard kill leaves behind.
- **No `colored` / `clap` / `ctrlc` / `napi`** — the SDK stays UI-agnostic
  so you can embed it anywhere (CI asserts this).
- **Verified dependency on `cloudflared`** — the version is pinned and its
  SHA-256 checked before the binary is made executable.

## Retargeting a live tunnel

`update_port` rewrites the tunnel's remote ingress, so the public URL is
unchanged and any URL you already shared keeps working — it just starts
serving the new local port.

```rust,no_run
# use cloudpipe_sdk::TunnelBuilder;
# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let mut handle = TunnelBuilder::new()
    .token(std::env::var("CLOUDFLARE_API_TOKEN")?)
    .domain("example.com")
    .port(8080)
    .start()
    .await?;

handle.update_port(9090).await?;
assert_eq!(handle.port(), 9090);
# Ok(()) }
```

## Exposing a self-signed HTTPS origin

`cloudflared` validates the origin certificate, so a local dev server with
a self-signed cert fails to connect. `no_tls_verify` turns that off.

```rust,no_run
# use cloudpipe_sdk::{Protocol, TunnelBuilder};
# async fn run() -> Result<(), Box<dyn std::error::Error>> {
TunnelBuilder::new()
    .token(std::env::var("CLOUDFLARE_API_TOKEN")?)
    .domain("example.com")
    .protocol(Protocol::Https)
    .port(8443)
    .no_tls_verify(true)
    .start()
    .await?;
# Ok(()) }
```

`OriginRequest` also exposes `http_host_header`, `connect_timeout`,
`tcp_keep_alive`, `http2_origin` and `no_happy_eyeballs`, individually or
as a whole via `origin_request(...)`.

## Reclaiming orphaned resources

```rust,no_run
# use cloudpipe_sdk::{CloudflareApi, cleanup_with_report};
# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let api = CloudflareApi::new(std::env::var("CLOUDFLARE_API_TOKEN")?)?;

// Preview first: nothing is deleted, everything is listed.
let report = cloudpipe_sdk::preview_cleanup(&api, "acct", "zone", "example.com").await?;
for action in &report.would_remove {
    println!("would remove {action}");
}

let report = cleanup_with_report(&api, "acct", "zone", "example.com").await?;
println!("removed {}", report.removed.len());
# Ok(()) }
```

## Errors

Every fallible call returns [`cloudpipe_sdk::Error`], which callers can
`match` on rather than string-parsing. The important distinctions:

| Variant | Meaning |
|---|---|
| `SubdomainInUse` | A **healthy** tunnel already serves that name. |
| `InvalidSubdomain` | Violates the DNS label rules. Caught before any network call. |
| `Cloudflare(Api { kind: RateLimited })` | Retried automatically with backoff. |
| `Cloudflare(Api { kind: AuthFailed })` | Token rejected — needs new permissions. |
| `Cloudflare(Timeout)` | Request deadline hit. |
| `InstallError::ChecksumMismatch` | The `cloudflared` download did not match its pinned digest. |

## License

MIT
