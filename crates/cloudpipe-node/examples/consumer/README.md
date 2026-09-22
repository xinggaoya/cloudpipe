# @xinggao/cloudpipe consumer demo

End-to-end smoke that exercises the **published** npm tarball
(`@xinggao/cloudpipe@0.1.0`) — not the local source — so it's a faithful
check of what other Node.js projects will see after `npm install`.

## What it does

1. Loads `@xinggao/cloudpipe` from `node_modules` and walks the
   documented surface (`connect`, `Listener`, `version`,
   `parseErrorCode`).
2. Validates `parseErrorCode` against the documented `[CODE]` /
   `[CODE:KIND]` prefix scheme (and the two
   `FUTURE_CODE` / `CLOUDFLARE_API:RATE_LIMITED_EXTENDED`
   forward-compat fall-through cases).
3. Confirms `Listener.prototype` exposes `url` / `fullName` /
   `subdomain` getters plus `on` / `wait` / `close` methods.
4. Hits `connect()` with no token (or an invalid subdomain) to
   verify the error path emits a `[CODE]`-prefixed message that
   `parseErrorCode` can round-trip.
5. **Optional**: if `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_DOMAIN`
   are set in the environment, it spins up a local `http.createServer`
   on a random port, asks the SDK to tunnel it, `fetch`es the
   resulting public URL, asserts the body round-trips, then closes.

## Run it

```bash
# Just the surface + error-path checks (no Cloudflare creds required):
npm install
npm start

# Full end-to-end (requires your own Cloudflare zone + token):
CLOUDFLARE_API_TOKEN=…  CLOUDFLARE_DOMAIN=example.com  npm start
```

Each check prints `[PASS]` / `[FAIL]` and the script exits non-zero
if anything failed, so `npm start` is suitable for CI.
