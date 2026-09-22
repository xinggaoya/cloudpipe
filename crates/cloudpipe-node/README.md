# cloudpipe (Node.js)

> Serverless Cloudflare tunnels for Node.js — no backend, no auth, no
> cloudflared install. Same shape as `@ngrok/ngrok`, backed by your own
> Cloudflare account.

## Install

```bash
npm install @xinggao/cloudpipe
```

Prebuilt binaries are shipped for macOS (arm64 + x64), Linux (x64-gnu),
and Windows (x64-msvc). Linux aarch64-gnu and Alpine/musl support are
scheduled for v0.2.0 — see the workflows' matrix entry for the upstream
zig-cross-compile setup we still need to land.

## Quick start

```js
import { connect } from '@xinggao/cloudpipe';

const listener = await connect({
  token: process.env.CLOUDFLARE_API_TOKEN,
  domain: 'example.com',
  port: 8080,
});

console.log(`live at ${listener.url}`);
// ... later ...
await listener.close();
```

No other binary needs to be installed — `cloudflared` is downloaded on
first use to `~/.cfp/bin/cloudflared` (with a default GitHub mirror for
users behind the GFW; override with `CFP_GITHUB_PROXY`).

## Lifecycle events

Subscribe to the same lifecycle events the CLI exposes:

```js
listener.on('creatingTunnel', (json) => {
 const { name } = JSON.parse(json);
 console.log(`creating tunnel ${name}`);
});

listener.on('edgeConnected', (json) => {
 const { connIndex, total } = JSON.parse(json);
 console.log(`edge connection ${connIndex}/${total}`);
});

listener.on('shuttingDown', (json) => {
 console.log('shutting down:', JSON.parse(json).reason);
});
```

The complete event-name catalog is documented in
[`index.d.ts`](./index.d.ts). Each callback receives the event payload
as a JSON string — `JSON.parse` it inside the handler to access
fields.

## API

### `connect(options) → Promise<Listener>`

| Field | Type | Default | Notes |
|---|---|---|---|
| `token` | `string` | — | **Required.** Cloudflare API token. |
| `domain` | `string` | auto | Base domain. Auto-discovered when the token has access to exactly one zone. |
| `protocol` | `'http'` \| `'https'` \| `'tcp'` \| `'udp'` \| `'ssh'` | `'http'` | Local service scheme. |
| `port` | `number` | `8080` | Local port to expose. |
| `subdomain` | `string` | random | Desired subdomain. |
| `autoRestart` | `boolean` | `false` | Respawn `cloudflared` on the same URL after a crash. |
| `cloudflaredPath` | `string` | — | Use a specific `cloudflared` binary instead of downloading. |
| `githubProxy` | `string` | `https://v4.gh-proxy.org/` | GitHub mirror prefix for the `cloudflared` download. |

### `Listener`

| Member | Type | Notes |
|---|---|---|
| `url` | `string` | Public HTTPS URL of the tunnel. |
| `fullName` | `string` | Public hostname (`myapp.example.com`). |
| `subdomain` | `string` | Just the subdomain part. |
| `on(event, cb)` | `(name, payload) → void` | Register an event listener. |
| `wait()` | `Promise<void>` | Block until the tunnel exits on its own. |
| `close()` | `Promise<void>` | Trigger clean shutdown + wait. |

### Errors

`cloudpipe` does not subclass the JS `Error` — the cloudpipe-specific
taxonomy is encoded as a `[CODE]` prefix on `error.message`
(optionally with a `:KIND` sub-class for `CLOUDFLARE_API`, e.g.
`[CLOUDFLARE_API:RATE_LIMITED]`). Use the exported
[`parseErrorCode`](./index.d.ts) helper to extract the code without
hand-rolling a regex:

```js
import { connect, parseErrorCode } from '@xinggao/cloudpipe';

try {
 await connect({ token });
} catch (err) {
 const code = parseErrorCode(err.message);
 // e.g. "CLOUDFLARE_API:RATE_LIMITED"
 switch (code) {
 case 'MISSING_CREDENTIAL': ...
 case 'CLOUDFLARE_API:RATE_LIMITED': ...
 }
}
```

The full top-level code list is in
[`index.d.ts`](./index.d.ts) (`CloudpipeErrorCode`); the
`CLOUDFLARE_API` sub-kind catalog is `CloudflareApiKind`.

## License

MIT — same as the parent project.