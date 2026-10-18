# cfp

> 🦀 Serverless localhost tunnels via Cloudflare Edge — single static binary, bring your own Cloudflare API token, no backend service.

## What it is

`cfp` exposes a local port to the public internet by creating a Cloudflare Tunnel and a DNS record on your own zone. Everything runs on your machine — there is **no shared service, no proxy server, no account on someone else's platform**. You authenticate with your own Cloudflare API token, the CLI talks to `api.cloudflare.com` directly, and `cloudflared` handles the QUIC connection to the Cloudflare edge.

- 🪶 **Single static binary** — Rust, `rustls` TLS (no system OpenSSL), ~4 MB
- 🪪 **Bring your own token** — your Cloudflare account, your zone, your rate-limit quota
- 🔒 **Local-only credentials** — token stored in `~/.cfp/config.json`, created `0600`
- 🌐 **Custom subdomains** — pick `-s myapp` or get a random one
- 🛣️ **Five protocols** — `http`, `https`, `tcp`, `udp`, `ssh` (Cloudflare edge always serves HTTPS publicly)
- 🔐 **Self-signed HTTPS works** — `--insecure` skips origin certificate validation for local dev servers
- 🧹 **Clean exit** — Ctrl+C stops `cloudflared` *first*, then deletes the tunnel and DNS record
- ♻️ **Auto-restart** — `cloudflared` crashes are recovered on the same URL with jittered backoff
- 🧯 **Self-healing** — every Cloudflare resource created during startup is rolled back if a later step fails
- 🪂 **Standalone** — works offline, air-gapped, behind NAT

## Install

### Download a release binary

Grab the latest release for your platform from the GitHub Releases page:

| Platform | File |
|---|---|
| Linux x86_64 (musl, glibc-free) | `cfp-linux-amd64` |
| Linux aarch64 (musl) | `cfp-linux-arm64` |
| macOS Intel | `cfp-darwin-amd64` |
| macOS Apple Silicon | `cfp-darwin-arm64` |
| Windows x86_64 | `cfp-windows-amd64.exe` |

Linux / macOS one-liner (auto-detects your platform):

```bash
curl -L -o cfp \
  "https://github.com/xinggaoya/cloudpipe/releases/latest/download/cfp-$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')-$(uname -s | tr A-Z a-z)"
chmod +x cfp
sudo mv cfp /usr/local/bin/
```

Or pick a specific asset manually:

```bash
# Linux x86_64 example
curl -L -o cfp https://github.com/xinggaoya/cloudpipe/releases/latest/download/cfp-linux-amd64
chmod +x cfp
sudo mv cfp /usr/local/bin/
```

### Install with cargo (from git)

Requires Rust 1.70+ and the `cloudflared` binary will be auto-downloaded on first use:

```bash
# Latest from main branch
cargo install --git https://github.com/xinggaoya/cloudpipe --bin cfp

# A pinned version
cargo install --git https://github.com/xinggaoya/cloudpipe --tag v0.1.0 --bin cfp
```

Then run `cfp --version` to confirm; the binary is placed at `~/.cargo/bin/cfp` (add to PATH if needed).

### Install from a mirror (mainland China / GFW)

If `github.com` is slow or unreachable from your network, prepend any of the
GitHub URLs above with a mirror prefix — `https://gh-proxy.org/` is a reliable
default and other public proxies (e.g. `https://ghfast.top/`,
`https://mirror.ghproxy.com/`) follow the same pattern:

```bash
# 1. Pre-built binary (auto-detect platform)
curl -L -o cfp \
  "https://gh-proxy.org/https://github.com/xinggaoya/cloudpipe/releases/latest/download/cfp-$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')-$(uname -s | tr A-Z a-z)"
chmod +x cfp
sudo mv cfp /usr/local/bin/

# 2. Specific Linux x86_64 binary
curl -L -o cfp https://gh-proxy.org/https://github.com/xinggaoya/cloudpipe/releases/latest/download/cfp-linux-amd64
chmod +x cfp
sudo mv cfp /usr/local/bin/

# 3. cargo install (from main branch, or pin a tag)
cargo install --git https://gh-proxy.org/https://github.com/xinggaoya/cloudpipe --bin cfp
cargo install --git https://gh-proxy.org/https://github.com/xinggaoya/cloudpipe --tag v0.1.0 --bin cfp

# 4. Clone source
git clone https://gh-proxy.org/https://github.com/xinggaoya/cloudpipe
```

The `cloudflared` helper binary that `cfp` downloads on first run also uses
this scheme automatically: it tries the configured mirror first and falls
back to `github.com` if the mirror is unreachable. Override the prefix with
the `CFP_GITHUB_PROXY` env var (e.g. `CFP_GITHUB_PROXY=https://ghfast.top/`);
set it to an empty string to disable mirroring entirely.

### How `cloudflared` is installed

`cfp` first looks for `cloudflared` on your `PATH`; if it is not there it
downloads a **pinned** version to `~/.cfp/bin/cloudflared`. Before the
download is made executable its SHA-256 is verified against a digest
compiled into the binary, and an asset with no pinned digest is refused
rather than installed unverified. Writes are staged and renamed atomically,
so two concurrent `cfp` runs cannot end up with a half-written executable.

The tunnel token is passed to `cloudflared` through a `0600` file rather than
the command line — argv is readable by every local user through
`/proc/<pid>/cmdline` and `ps`.

### Build from source

```bash
git clone https://github.com/xinggaoya/cloudpipe
cd cloudpipe
cargo build --release
./target/release/cfp --version
```

## One-time setup: Cloudflare API token

1. Open <https://dash.cloudflare.com/profile/api-tokens>
2. **Create Token** → **Custom Token**, with these permissions:
   - **Account → Cloudflare Tunnel → Edit**
   - **Zone → DNS → Edit**
   - **Zone → Zone → Read**
3. Under **Zone Resources**, pick the domain you want to expose tunnels under.
4. Copy the token (you won't see it again).

## Quick start

```bash
# Save the token. cfp verifies it with Cloudflare and auto-discovers
# the account and the first accessible zone.
cfp key <CF_API_TOKEN>

# Expose localhost:8080 with a random subdomain (defaults to http)
cfp 8080
#   => https://user-1234.example.com

# Use a specific subdomain
cfp 8080 -s myapp
#   => https://myapp.example.com

# Declare the local protocol explicitly (ngrok-style)
cfp https 8443
#   => https://user-1234.example.com   (local: https://localhost:8443)

# Expose an HTTPS dev server whose certificate is self-signed
cfp https 8443 -s myapp --insecure
#   => skips origin certificate validation, which would otherwise fail

# Move a live tunnel to a different local port (public URL unchanged)
# (also available as TunnelBuilder::update_port in the SDK)

# Expose a TCP service (e.g. SSH, Postgres, Redis — anything L4)
cfp tcp 22 -s ssh-tunnel
#   Reach it at ssh-tunnel.example.com (TCP)

# Expose a UDP service (e.g. DNS, WireGuard)
cfp udp 51820 -s wg
#   Reach it at wg.example.com (UDP)

# View current configuration
cfp key

# Switch the base domain (picks a different zone)
cfp domain another.example.com

# List all zones the token can access
cfp domains

# Preview what cleanup would remove, then do it
cfp cleanup --dry-run
cfp cleanup --json        # machine-readable report
cfp cleanup
```

The config file lives at `~/.cfp/config.json` (created `0600`). Writes are
atomic, so an interrupted save cannot corrupt it. You can also pass the
token via env var for one-off use (overrides the file):

```bash
CLOUDFLARE_API_TOKEN=... cfp 8080
```

### Environment variables

| Variable | Effect |
|---|---|
| `CLOUDFLARE_API_TOKEN` | Overrides the token in the config file. |
| `CFP_CONFIG_DIR` | Relocates the config directory (default `~/.cfp`). |
| `CFP_GITHUB_PROXY` | Mirror prefix for the `cloudflared` download. Empty disables it. |
| `CFP_LOG` | `trace`/`debug`/`info`/`warn`/`error`/`off`. Unset or `off` is silent. |
| `CFP_ASCII` | Force ASCII output for terminals without Unicode box drawing. |
| `NO_COLOR` | Disable colour (also auto-disabled when stdout is not a TTY). |

### Connecting to non-HTTP tunnels

For `http` / `https` (L7), just open the URL in your browser or `curl` it.

For `tcp` / `udp` / `ssh` (L4), the hostname works as the public endpoint. For example, after `cfp tcp 22 -s mybox`, you can:

```bash
# Generic TCP — connect directly to the hostname
ncat mybox.example.com 22
ssh user@mybox.example.com      # SSH transparently falls back to direct TCP connect

# Or via local-forward with socat
socat TCP-LISTEN:2222,fork TCP:mybox.example.com:22
```

For `ssh`, Cloudflare exposes a native `cloudflared access ssh` flow that gives a browser-based auth prompt on first connect — useful when you don't want to expose the port directly. Run `cloudflared access ssh --hostname mybox.example.com` on any host with `cloudflared` installed.

For `udp`, reach the service by pointing your client at `myhost.example.com:51820` (WireGuard, etc.); most work fine since Cloudflare passes the UDP datagram through.

## Commands

| Command | Description |
|---|---|
| `cfp [http\|https\|tcp\|udp\|ssh] <port> [-s <subdomain>]` | Start a tunnel. Protocol selects the local scheme (`http` is the default). |
| `cfp ... --insecure` | Skip origin TLS verification (self-signed HTTPS origins). |
| `cfp ... --origin-host-header <host>` | Send a specific `Host` header to the origin. |
| `cfp ... --no-restart` | Exit the first time `cloudflared` dies instead of restarting it. |
| `cfp ... -v / --verbose` | Print the SDK's internal trace to stderr. |
| `cfp key [TOKEN] [--clear]` | Save, show or clear the Cloudflare token. Saving auto-discovers account/zone. |
| `cfp domain <example.com>` | Switch the base domain to a different accessible zone. |
| `cfp domains` | List zones accessible with the saved token. |
| `cfp cleanup [--dry-run] [--json]` | Remove dead tunnels, their DNS records and orphaned CNAMEs. |
| `cfp -V / --version` | Show version. |

## As a Rust library

The same functionality is available as an async Rust SDK so you can embed
tunneling into your own program.

```toml
[dependencies]
cloudpipe-sdk = "0.1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```rust
use cloudpipe_sdk::{Protocol, TunnelBuilder};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut handle = TunnelBuilder::new()
        .token(std::env::var("CLOUDFLARE_API_TOKEN")?)
        .domain("example.com")            // account + zone are auto-discovered
        .protocol(Protocol::Http)
        .port(8080)
        .subdomain("myapp")
        .auto_restart(true)               // keep the tunnel alive across cloudflared crashes
        .on_event(|event| println!("{event:?}"))
        .start()
        .await?;

    println!("live at {}", handle.url());
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

See the SDK README in [`crates/cloudpipe-sdk/`](./crates/cloudpipe-sdk/) and the
[`quickstart`](./crates/cloudpipe-sdk/examples/quickstart.rs) /
[`with_events`](./crates/cloudpipe-sdk/examples/with_events.rs) examples.

## As a Node.js library

The same functionality is available as an npm-installable package for
Node.js — same `connect()` shape as `@ngrok/ngrok`, same `Listener`
handle, same lifecycle events.

```bash
npm install @xinggao/cloudpipe
```

```js
import { connect } from '@xinggao/cloudpipe';

const listener = await connect({
  token: process.env.CLOUDFLARE_API_TOKEN,
  domain: 'example.com',
  protocol: 'http',
  port: 8080,
  subdomain: 'myapp',
  autoRestart: true,             // keep the tunnel alive across cloudflared crashes
});

listener.on('dnsCreated', (json) => {
  const { fullName } = JSON.parse(json);
  console.log(`DNS created for ${fullName}`);
});

console.log(`live at ${listener.url}`);

// Move to another local port; the public URL is unchanged.
await listener.update(9090);

process.on('SIGINT', async () => {
  await listener.close();
  process.exit(0);
});
```

Prebuilt binaries are shipped for macOS (arm64 + x64), Linux (x64-gnu) and
Windows (x64-msvc). No separate `cloudflared` install required — the SDK
downloads a pinned, checksum-verified build on first use, with a GitHub
mirror default for users behind the GFW.

See [`crates/cloudpipe-node/README.md`](./crates/cloudpipe-node/) and the
[`quickstart`](./crates/cloudpipe-node/examples/quickstart.js) /
[`with-events`](./crates/cloudpipe-node/examples/with-events.js)
examples for the full API. An end-to-end **consumer demo** that runs
against the *published* npm tarball (not the local source) lives in
[`crates/cloudpipe-node/examples/consumer/`](./crates/cloudpipe-node/examples/consumer/)
and exits non-zero if anything regresses in the next publish.

## Repository layout

```
crates/
├── cloudpipe-sdk/           # pure async Rust SDK (no CLI / UI deps)
├── cloudpipe/               # `cfp` CLI binary — uses the SDK
├── cloudpipe-node-core/     # napi-free logic: event mapping, error codes
└── cloudpipe-node/          # napi-rs Node.js bindings — thin `#[napi]` shell
```

`cloudpipe-node` is a `cdylib`, so a `cargo test` harness for it cannot
link (the `napi_*` symbols only resolve inside a Node.js process).
Everything worth testing therefore lives in `cloudpipe-node-core`; the
addon crate holds only the annotated shell.

## How it works

```
cfp tcp 22 -s mybox
  ├─ Load ~/.cfp/config.json              (token + account/zone/domain)
  ├─ Ensure cloudflared                   (PATH → auto-download to ~/.cfp/bin/)
  ├─ Reclaim stale tunnels / orphaned DNS with the same name
  ├─ POST /accounts/{id}/tunnels          create named tunnel
  ├─ PUT  /accounts/{id}/cfd_tunnel/{id}/configurations
  │        ingress: [{hostname: mybox.example.com, service: tcp://localhost:22},
  │                  {service: http_status:404}]
  ├─ POST /zones/{id}/dns_records         CNAME mybox.example.com → {tunnelId}.cfargotunnel.com
  ├─ spawn cloudflared tunnel run --token ... --no-autoupdate
  └─ wait for Ctrl+C (or child crash if --no-restart)
        └─ child crash (default): kill cloudflared → delete DNS → cleanup
           connections → delete tunnel → bootstrap a fresh tunnel on the
           same subdomain
        └─ Ctrl+C: kill cloudflared → delete DNS → cleanup connections
           → delete tunnel
```

All five protocols (`http`/`https`/`tcp`/`udp`/`ssh`) are configured the same way — only the `service` field in ingress changes. Routing happens entirely on Cloudflare's edge using the remote ingress configuration; `cloudflared` does not need a local config file.

Conflict handling:
- **Same-name tunnel exists and is healthy** → refuse, suggest a different name.
- **A generated name collides** → transparently redraw it (an explicitly requested `-s name` is never silently renamed).
- **Same-name tunnel exists but is down/inactive/degraded** → reclaim it (delete DNS, kill connections, delete tunnel, then create fresh).
- **Orphaned DNS record** → remove before creating.
- **Cloudflare API errors** → matched on the real error variant and rendered with an actionable next step (10429 rate limit, 81053 DNS exists, 10001 auth, invalid subdomain, …).

Other guarantees worth knowing about:
- A DNS-creation or `cloudflared`-spawn failure **unwinds** the tunnel that was just created, instead of leaving it orphaned.
- `drop(handle)` detaches a best-effort Cloudflare purge; `cloudpipe_sdk::cleanup` reclaims anything a hard kill leaves.
- Generated subdomains draw from 32^7 values (the previous 10,000 made `SubdomainInUse` routine).

## Limitations

- You need a domain managed by Cloudflare (any plan, including free).
- Cloudflare's API rate limits apply per token. The SDK retries transient failures automatically; sustained heavy use may still need `cfp cleanup`.
- The `cloudflared` binary is downloaded on first use and stored at `~/.cfp/bin/cloudflared` (~40 MB). Its version is pinned, so upgrading `cfp` to pick up a newer `cloudflared` is deliberate.
- Auto-restart uses a bounded retry budget (5 attempts, 500 ms doubling to an 8 s cap). Beyond that the session ends and reports why, rather than reconnecting forever.

## License

MIT