# cloudpipe-sdk

Async Rust SDK for [cloudpipe](https://github.com/xinggaoya/cloudpipe) —
expose a local port to the public internet via a Cloudflare Tunnel from
your own program, with **no backend service** involved.

```rust
use cloudpipe_sdk::{TunnelBuilder, Protocol};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut handle = TunnelBuilder::new()
        .token(std::env::var("CLOUDFLARE_API_TOKEN")?)
        .domain("example.com")
        .protocol(Protocol::Http)
        .port(8080)
        .subdomain("myapp")
        .auto_restart(true) // keep the tunnel alive across cloudflared crashes
        .on_event(|event| println!("{event:?}"))
        .start()
        .await?;

    println!("Live at {}", handle.url());

    // `wait` blocks until the user signals shutdown. With auto-restart
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
- **Bring your own credentials** — same `~/.cfp/config.json` layout as the CLI,
  or pass them in code.
- **Event stream** — `on_event` closure receives lifecycle events so you
  render your own UI.
- **Safe by default** — `Drop` on `TunnelHandle` performs best-effort cleanup
  if `stop()` wasn't called.
- **No `colored` / `clap` / `ctrlc`** — the SDK stays UI-agnostic so you can
  embed it anywhere.

## License

MIT