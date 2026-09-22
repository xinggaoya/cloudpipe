'use strict';

// End-to-end consumer demo for @xinggao/cloudpipe@0.1.0.
//
// What this verifies:
//   1. The published npm tarball installs cleanly and loads.
//   2. The exposed CJS surface matches the documented API:
//      - `connect(options)` async function
//      - `Listener` class with url / fullName / subdomain getters,
//        on(event, cb), wait(), close()
//      - `version()` returns the published version string
//      - `parseErrorCode(message)` extracts the [CODE] prefix
//   3. Error paths: connect() against empty / bad tokens surfaces
//      a `[CODE]`-prefixed message and `parseErrorCode` round-trips
//      it correctly.
//   4. Real-path end-to-end: if CLOUDFLARE_API_TOKEN is set AND a
//      CLOUDFLARE_DOMAIN is set, spin up a local HTTP server, expose
//      it via a tunnel, and curl the public URL.
//
// Everything is wrapping try/catch and aggregator-state so the script
// exits 0 on success / 1 on failure — useful for CI sanity.

const path = require('node:path');
const http = require('node:http');

let cloudpipe;
let loadError = null;
try {
  // eslint-disable-next-line global-require
  cloudpipe = require('@xinggao/cloudpipe');
} catch (err) {
  loadError = err;
}

const REQUIRED_EXPORTS = ['connect', 'Listener', 'version', 'parseErrorCode'];

const checks = [];
function check(name, fn) {
  return Promise.resolve()
    .then(fn)
    .then((detail) => checks.push({ name, ok: true, detail }))
    .catch((err) =>
      checks.push({
        name,
        ok: false,
        detail: (err && err.message) || String(err),
      }),
    );
}

async function main() {
  // --- 1. The package itself can be required ---
  await check('package loads from published npm', async () => {
    if (loadError) throw loadError;
    if (!cloudpipe || typeof cloudpipe !== 'object') {
      throw new Error('expected @xinggao/cloudpipe exports to be an object');
    }
    return `loaded from ${require.resolve('@xinggao/cloudpipe')}`;
  });

  // --- 2. Surface shape ---
  await check('exposes connect, Listener, version, parseErrorCode', async () => {
    for (const key of REQUIRED_EXPORTS) {
      if (!(key in cloudpipe)) {
        throw new Error(`missing export "${key}"`);
      }
    }
    if (typeof cloudpipe.connect !== 'function') throw new Error('connect must be a function');
    if (typeof cloudpipe.version !== 'function') throw new Error('version must be a function');
    if (typeof cloudpipe.parseErrorCode !== 'function') {
      throw new Error('parseErrorCode must be a function');
    }
    if (typeof cloudpipe.Listener !== 'function') throw new Error('Listener must be a class');
    return `version() = ${cloudpipe.version()}`;
  });

  // --- 3. version() matches the package.json the consumer resolved ---
  await check('version() returns 0.1.0', async () => {
    const v = cloudpipe.version();
    if (v !== '0.1.0') throw new Error(`expected 0.1.0, got ${v}`);
    return v;
  });

  // --- 4. parseErrorCode round-trips documented codes ---
  await check('parseErrorCode on known [CODE] forms', async () => {
    const samples = [
      ['[MISSING_CREDENTIAL] need a token', 'MISSING_CREDENTIAL'],
      ['[CLOUDFLARE_API:RATE_LIMITED] too many', 'CLOUDFLARE_API:RATE_LIMITED'],
      ['[CLOUDFLARE_API:AUTH_FAILED] nope', 'CLOUDFLARE_API:AUTH_FAILED'],
      ['[SUBDOMAIN_IN_USE] foo.example.com', 'SUBDOMAIN_IN_USE'],
      ['[INVALID_SUBDOMAIN] x.y', 'INVALID_SUBDOMAIN'],
      ['[IO] eperm', 'IO'],
      ['plain text no prefix', null],
      ['', null],
      ['[unclosed', null],
      // forward-compat: unknown parents pass through verbatim
      ['[FUTURE_CODE] blah', 'FUTURE_CODE'],
      ['[CLOUDFLARE_API:RATE_LIMITED_EXTENDED] blah', 'CLOUDFLARE_API:RATE_LIMITED_EXTENDED'],
    ];
    const wrong = samples.find(([msg, expected]) => {
      const got = cloudpipe.parseErrorCode(msg);
      return got !== expected;
    });
    if (wrong) {
      throw new Error(
        `parseErrorCode(${JSON.stringify(wrong[0])}) => ${JSON.stringify(
          cloudpipe.parseErrorCode(wrong[0]),
        )}, expected ${JSON.stringify(wrong[1])}`,
      );
    }
    return `${samples.length} cases ok`;
  });

  // --- 5. Listener class shape (without actually running a tunnel) ---
  await check('Listener exposes url / fullName / subdomain getters', async () => {
    const proto = cloudpipe.Listener.prototype;
    const desc = Object.getOwnPropertyDescriptor(proto, 'url');
    if (!desc || typeof desc.get !== 'function') {
      throw new Error('Listener.prototype.url getter missing');
    }
    const fullName = Object.getOwnPropertyDescriptor(proto, 'fullName');
    if (!fullName || typeof fullName.get !== 'function') {
      throw new Error('Listener.prototype.fullName getter missing');
    }
    const subdomain = Object.getOwnPropertyDescriptor(proto, 'subdomain');
    if (!subdomain || typeof subdomain.get !== 'function') {
      throw new Error('Listener.prototype.subdomain getter missing');
    }
    if (typeof proto.on !== 'function') throw new Error('Listener.prototype.on missing');
    if (typeof proto.wait !== 'function') throw new Error('Listener.prototype.wait missing');
    if (typeof proto.close !== 'function') throw new Error('Listener.prototype.close missing');
    return 'all 6 members present';
  });

  // --- 6. Negative path: connect() with no token ---
  let firstErrorCode = null;
  await check('connect() with empty token surfaces [CODE] prefix', async () => {
    try {
      await cloudpipe.connect({ token: '', domain: 'example.com', port: 8080 });
      throw new Error('connect() unexpectedly succeeded with empty token');
    } catch (err) {
      const msg = (err && err.message) || '';
      if (!/^\[(\w+(?::\w+)?)\]/.test(msg)) {
        throw new Error(`error did not carry [CODE] prefix: ${msg.split('\n')[0]}`);
      }
      firstErrorCode = cloudpipe.parseErrorCode(msg);
      return `${firstErrorCode || '?'}: ${msg.split('\n')[0]}`;
    }
  });

  // --- 7. Negative path: connect() with subdomain-form-violating value ---
  await check('connect() with invalid subdomain returns [INVALID_SUBDOMAIN]', async () => {
    try {
      await cloudpipe.connect({
        token: 'placeholder',
        domain: 'example.com',
        subdomain: 'x y', // space invalid
        port: 8080,
      });
      throw new Error('connect() unexpectedly accepted invalid subdomain');
    } catch (err) {
      const code = cloudpipe.parseErrorCode((err && err.message) || '');
      if (code !== 'INVALID_SUBDOMAIN') {
        // Path could go through Cloudflare API auth instead — accept
        // CLOUDFLARE_API:* / MISSING_CREDENTIAL too.
        if (
          !code ||
          (!code.startsWith('CLOUDFLARE_API') && code !== 'MISSING_CREDENTIAL')
        ) {
          throw new Error(
            `expected INVALID_SUBDOMAIN (or auth failure), got: ${code || 'null'}`,
          );
        }
      }
      return code || 'null';
    }
  });

  // --- 8. Real-path (only when real credentials are set) ---
  const token = process.env.CLOUDFLARE_API_TOKEN;
  const domain = process.env.CLOUDFLARE_DOMAIN;
  if (token && domain) {
    await runRealTunnel(cloudpipe, token, domain);
  } else {
    checks.push({
      name: 'real tunnel (skip — set CLOUDFLARE_API_TOKEN + CLOUDFLARE_DOMAIN to enable)',
      ok: true,
      detail: 'skipped',
    });
  }

  // --- Report ---
  printReport();
  const failed = checks.filter((c) => !c.ok);
  if (failed.length > 0) {
    console.error(`\n${failed.length} check(s) failed`);
    process.exit(1);
  }
  console.log('\nall checks ok');
}

async function runRealTunnel(cloudpipe, token, domain) {
  // Tiny HTTP server — anything we hit returns a marker so we can
  // prove the tunnel actually routed.
  let hitCount = 0;
  const server = http.createServer((req, res) => {
    hitCount += 1;
    res.writeHead(200, { 'content-type': 'text/plain' });
    res.end(`hello from @xinggao/cloudpipe consumer demo at ${new Date().toISOString()}\n`);
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;

  let listener;
  try {
    listener = await cloudpipe.connect({ token, domain, port });
  } catch (err) {
    server.close();
    checks.push({
      name: 'real tunnel — connect succeeded',
      ok: false,
      detail: `${cloudpipe.parseErrorCode((err && err.message) || '') || '?'}: ${
        (err && err.message || '').split('\n')[0]
      }`,
    });
    return;
  }

  await check('real tunnel — connect succeeded', async () => {
    if (!listener || typeof listener.url !== 'string' || !listener.url.startsWith('https://')) {
      throw new Error(`bad listener.url = ${listener && listener.url}`);
    }
    return `${listener.url} (subdomain=${listener.subdomain}, fullName=${listener.fullName})`;
  });

  // Subscribe to lifecycle events for diagnostic output.
  // The first 'edgeConnected' event means the tunnel has a live QUIC
  // connection to the CF edge, so waiting for it before fetching
  // gives a much higher hit-rate on the first try.
  const firstEdgeWait = new Promise((resolve, reject) => {
    const t = setTimeout(
      () => reject(new Error('edgeConnected did not fire in 30s')),
      30_000,
    );
    listener.on('edgeConnected', (json) => {
      clearTimeout(t);
      try {
        const p = JSON.parse(json);
        resolve(`conn=${p.connIndex}/${p.total}`);
      } catch {
        resolve(json);
      }
    });
  });

  await check('real tunnel — edgeConnected lifecycle event', async () => {
    return await firstEdgeWait;
  });

  // Curl the public URL, with a small retry loop because the very
  // first request on a freshly-built tunnel sometimes loses to the
  // edge route propagation window.
  await check('real tunnel — public URL serves local server', async () => {
    const target = listener.url;
    let lastErr;
    for (let attempt = 1; attempt <= 5; attempt += 1) {
      try {
        const resp = await fetch(target, { redirect: 'manual' });
        if (resp.status >= 200 && resp.status < 400) {
          const body = await resp.text();
          if (!body.includes('hello from @xinggao/cloudpipe consumer demo')) {
            throw new Error(`unexpected body: ${body.slice(0, 60)}…`);
          }
          return `HTTP ${resp.status}, body len ${body.length}, local hits=${hitCount}`;
        }
        lastErr = new Error(`HTTP ${resp.status} from ${target}`);
      } catch (err) {
        lastErr = err;
      }
      await new Promise((r) => setTimeout(r, 1000 * attempt));
    }
    throw lastErr || new Error('fetch failed after 5 attempts');
  });

  await check('real tunnel — close shuts down cleanly', async () => {
    const start = Date.now();
    await Promise.race([
      listener.close(),
      new Promise((_, reject) =>
        setTimeout(() => reject(new Error('close() did not return in 15s')), 15_000),
      ),
    ]);
    return `closed in ${Date.now() - start}ms`;
  });

  server.close();
}

function printReport() {
  const w = Math.max(...checks.map((c) => c.name.length), 24);
  for (const c of checks) {
    const status = c.ok ? 'PASS' : 'FAIL';
    const detail = c.ok ? `— ${c.detail}` : `— ${c.detail}`;
    console.log(`[${status}] ${c.name.padEnd(w)} ${detail}`);
  }
}

main().catch((err) => {
  console.error('fatal:', err);
  process.exit(1);
});
