# cfp

> 🦀 Serverless localhost tunnels via Cloudflare Edge — single static binary, bring your own Cloudflare API token, no backend service.

## What it is

`cfp` exposes a local port to the public internet by creating a Cloudflare Tunnel and a DNS record on your own zone. Everything runs on your machine — there is **no shared service, no proxy server, no account on someone else's platform**. You authenticate with your own Cloudflare API token, the CLI talks to `api.cloudflare.com` directly, and `cloudflared` handles the QUIC connection to the Cloudflare edge.

- 🪶 **Single static binary** — Rust, `rustls` TLS (no system OpenSSL), ~4 MB
- 🪪 **Bring your own token** — your Cloudflare account, your zone, your rate-limit quota
- 🔒 **Local-only credentials** — token stored in `~/.cfp/config.json` with mode `0600`
- 🌐 **Custom subdomains** — pick `-s myapp` or get a random one
- 🛣️ **Five protocols** — `http`, `https`, `tcp`, `udp`, `ssh` (Cloudflare edge always serves HTTPS publicly)
- 🧹 **Clean exit** — Ctrl+C deletes the tunnel and DNS record automatically; no orphans
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

# Clean up dead tunnels and orphaned DNS records
cfp cleanup
```

The config file lives at `~/.cfp/config.json` (mode `0600`). You can also pass the token via env var for one-off use (overrides the file):

```bash
CLOUDFLARE_API_TOKEN=... cfp 8080
```

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
| `cfp key [TOKEN] [--clear]` | Save, show or clear the Cloudflare token. Saving auto-discovers account/zone. |
| `cfp domain <example.com>` | Switch the base domain to a different accessible zone. |
| `cfp domains` | List zones accessible with the saved token. |
| `cfp cleanup` | Remove dead tunnels, their DNS records and orphaned CNAMEs. |
| `cfp -v / --version` | Show version. |

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
  └─ wait for Ctrl+C / 4h timeout / child exit
        └─ kill cloudflared → delete DNS → cleanup connections → delete tunnel
```

All five protocols (`http`/`https`/`tcp`/`udp`/`ssh`) are configured the same way — only the `service` field in ingress changes. Routing happens entirely on Cloudflare's edge using the remote ingress configuration; `cloudflared` does not need a local config file.

Conflict handling:
- **Same-name tunnel exists and is healthy** → refuse, suggest a different name.
- **Same-name tunnel exists but is down/inactive/degraded** → reclaim it (delete DNS, kill connections, delete tunnel, then create fresh).
- **Orphaned DNS record** → remove before creating.
- **Cloudflare API errors** → translated to actionable text (10429 rate limit, 81053 DNS exists, 10001 auth).

## Limitations

- You need a domain managed by Cloudflare (any plan, including free).
- Cloudflare's API rate limits apply per token — heavy usage may need `cfp cleanup` if a previous run was interrupted.
- The `cloudflared` binary is downloaded on first use and stored at `~/.cfp/bin/cloudflared` (~30 MB).

## License

MIT