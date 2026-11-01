# cloudpipe (Node.js)

> Serverless Cloudflare tunnels for Node.js — no backend, no auth, no
> cloudflared install. Same shape as `@ngrok/ngrok`, backed by your own
> Cloudflare account.

## Install

```bash
npm install @xinggao/cloudpipe
```

Prebuilt binaries are shipped for macOS (arm64 + x64), Linux (x64-gnu)
and Windows (x64-msvc). Linux aarch64-gnu and Alpine/musl need a zig
cross-compiler that the CI matrix does not set up yet.

No other binary needs to be installed: `cloudflared` is downloaded on
first use to `~/.cfp/bin/cloudflared`, at a **pinned version** whose
SHA-256 is verified before the binary is made executable.

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

## Changing a live tunnel

```js
await listener.update(9090);   // same public URL, new local port
console.log(listener.port);    // 9090
```

This rewrites the tunnel's remote ingress, so no URL you already shared
breaks. It is the counterpart of ngrok's `listener.update()`.

## Self-signed HTTPS origins

`cloudflared` validates the origin certificate, so a local dev server with
a self-signed certificate needs an explicit opt-out:

```js
const listener = await connect({
  token,
  domain: 'example.com',
  protocol: 'https',
  port: 8443,
  noTlsVerify: true,
});
```

`originHostHeader` is there for services that route on their own hostname.

## Diagnostics

The SDK emits `debug`/`warn` events for reconnect retries, backoff and
cleanup failures. They are collected by a subscriber you install:

```bash
CFP_LOG=debug node app.js
```

```js
const { connect, initLogging } = require('@xinggao/cloudpipe');
initLogging();   // reads CFP_LOG
```

Without it the events are dropped, so a tunnel that keeps flapping looks
like a tunnel that keeps flapping for no visible reason.

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

Events that fire before a handler is registered are delivered as soon as
one is. The startup events (`dnsCreated`, `cloudflaredStarted`, the first
`edgeConnected`) happen while the tunnel is being created — before
`connect()` even resolves — so without this they would be unreachable. A
callback registered *after* the event has already been delivered to an
earlier listener does not receive it.

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
| `noTlsVerify` | `boolean` | `false` | Skip origin certificate validation. |
| `originHostHeader` | `string` | — | `Host` header sent to the origin. |
| `restartPolicy` | `{ maxAttempts?, baseDelayMs?, maxDelayMs? }` | 5 / 500 / 8000 | Reconnection budget when `autoRestart` is on. |

### `Listener`

| Member | Type | Notes |
|---|---|---|
| `url` | `string` | Public HTTPS URL of the tunnel. |
| `fullName` | `string` | Public hostname (`myapp.example.com`). |
| `subdomain` | `string` | Just the subdomain part. |
| `port` | `number` | Local port currently forwarded. |
| `on(event, cb)` | `(payload) → void` | Register an event listener. Throws on an unknown event name. |
| `off(event, cb?)` | `void` | Remove one listener for an event, or every listener for it when no callback is given. |
| `removeAllListeners()` | `void` | Remove every listener. |
| `wait()` | `Promise<void>` | Block until the tunnel ends on its own. Does **not** prevent a later `close()`. |
| `update(port)` | `Promise<void>` | Repoint at another local port; public URL unchanged. |
| `close()` | `Promise<void>` | Trigger clean shutdown + wait. |

`await listener.wait(); await listener.close();` works: a pending `wait()`
does not starve a later `close()`.

### `cleanup(options) → Promise<CleanupReport>`

Reclaims dead tunnels and orphaned DNS records in one zone. A Node
application that dies without closing its listener leaves a tunnel and a
CNAME behind; this is how they get reclaimed from the same language.

```js
const { cleanup } = require('@xinggao/cloudpipe');

const report = await cleanup({ token, domain: 'example.com' });
console.log(`removed ${report.removed.length} resources`);

// Preview first — nothing is deleted.
const preview = await cleanup({ token, domain: 'example.com', dryRun: true });
console.log(preview.wouldRemove);
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `token` | `string` | — | **Required.** Cloudflare API token. |
| `domain` | `string` | auto | Base domain. |
| `account` | `string` | auto | Cloudflare account id. |
| `zone` | `string` | auto | Cloudflare zone id. |
| `dryRun` | `boolean` | `false` | Report what would be removed without removing anything. |

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

The error object also carries `err.code` and `err.kind` directly, so a
`switch` on `err.code` works without parsing the message:

```js
catch (err) {
 switch (err.code) {
 case 'MISSING_CREDENTIAL': ...
 case 'CLOUDFLARE_API':
   if (err.kind === 'RATE_LIMITED') ...
 }
}
```

The full top-level code list is in
[`index.d.ts`](./index.d.ts) (`CloudpipeErrorCode`); the
`CLOUDFLARE_API` sub-kind catalog is `CloudflareApiKind`. Both are also
available at runtime via `errorCodes()` and `cloudflareErrorKinds()`.

`parseErrorCode` is forward-compatible: a code this build does not know
about is returned verbatim (including any `:KIND` suffix) rather than
throwing, so a newer SDK does not break an older consumer.

## Cleanup on abandonment

Dropping the listener ends the session, which stops `cloudflared` and
releases the tunnel and DNS record. Calling `close()` explicitly is still
preferred — it waits for the teardown to finish — but a listener you lose
reference to is no longer a leaked tunnel.

## License

MIT — same as the parent project.