// Local smoke test: verifies the built .node module loads and exposes
// the documented API surface, and that a tunnel can actually be created
// against a mock Cloudflare API.
//
// The public entry point is `js/index.js` — the facade — because that is
// what an npm consumer gets. The raw addon is loaded alongside it to
// check that the native surface is still intact underneath.
//
// Run after `npm run build:local`.

'use strict';

const path = require('node:path');

const { startMockApi } = require('./mock-api.js');
const { writeFakeCloudflared } = require('./fake-cloudflared.js');

// Path-based require: the smoke test loads from the locally built
// artifact rather than the registry, so the package name is irrelevant.
const native = require(path.resolve(__dirname, '..', 'npm', 'index.js'));
const api = require(path.resolve(__dirname, '..', 'js', 'index.js'));

const problems = [];

function check(label, cond, detail) {
  if (cond) {
    console.log(`  ok   ${label}`);
  } else {
    problems.push(`${label}${detail ? `: ${detail}` : ''}`);
    console.error(`  FAIL ${label}${detail ? `: ${detail}` : ''}`);
  }
}

console.log('module exports:', Object.keys(api));
console.log('version:', api.version());
console.log('');

// --- Exported functions ------------------------------------------------
for (const key of [
  'connect',
  'cleanup',
  'version',
  'parseErrorCode',
  'eventNames',
  'errorCodes',
  'cloudflareErrorKinds',
]) {
  check(`${key} is a function`, typeof api[key] === 'function', typeof api[key]);
}
check(
  'the native addon still exposes connect',
  typeof native.connect === 'function',
  typeof native.connect,
);

// --- parseErrorCode ----------------------------------------------------
const codeCases = [
  ['[MISSING_CREDENTIAL] need a token', 'MISSING_CREDENTIAL'],
  ['[CLOUDFLARE_API:RATE_LIMITED] slow down', 'CLOUDFLARE_API:RATE_LIMITED'],
  ['[CLOUDFLARE_API:SERVER_ERROR] oops', 'CLOUDFLARE_API:SERVER_ERROR'],
  ['[CLOUDFLARE_BINARY] no binary', 'CLOUDFLARE_BINARY'],
  ['[INVALID_ARGUMENT] port 0', 'INVALID_ARGUMENT'],
  // forward-compat: an unknown parent must round-trip *including* its
  // sub-kind, which it did not when the suffix was dropped.
  ['[FUTURE_CODE:NEXT_KIND] newer sdk', 'FUTURE_CODE:NEXT_KIND'],
  ['[FUTURE_CODE] newer sdk', 'FUTURE_CODE'],
  ['plain text', null],
  ['[unclosed', null],
  ['', null],
];
for (const [input, expected] of codeCases) {
  const got = api.parseErrorCode(input);
  check(`parseErrorCode(${JSON.stringify(input)})`, got === expected, `got ${JSON.stringify(got)}`);
}

// --- Catalogs ----------------------------------------------------------
const names = api.eventNames();
check('eventNames is non-empty', Array.isArray(names) && names.length > 0);
for (const expected of ['banner', 'dnsCreated', 'restarted', 'bootstrapFailed', 'cleaned']) {
  check(`eventNames includes ${expected}`, names.includes(expected));
}
check(
  'errorCodes lists the documented set',
  [
    'MISSING_CREDENTIAL',
    'CLOUDFLARE_API',
    'INVALID_SUBDOMAIN',
    'INVALID_DOMAIN',
    'SUBDOMAIN_IN_USE',
    'ALREADY_SHUT_DOWN',
  ].every((c) => api.errorCodes().includes(c)),
  api.errorCodes().join(','),
);
check(
  'cloudflareErrorKinds lists the documented set',
  ['RATE_LIMITED', 'DNS_EXISTS', 'AUTH_FAILED', 'INVALID_TOKEN', 'SERVER_ERROR'].every((k) =>
    api.cloudflareErrorKinds().includes(k),
  ),
  api.cloudflareErrorKinds().join(','),
);

// --- Listener class shape ----------------------------------------------
const proto = api.Listener.prototype;
for (const getter of ['url', 'fullName', 'subdomain', 'port']) {
  const d = Object.getOwnPropertyDescriptor(proto, getter);
  check(`Listener#${getter} getter`, !!d && typeof d.get === 'function');
}
for (const method of ['on', 'off', 'removeAllListeners', 'wait', 'update', 'close']) {
  check(`Listener#${method}()`, typeof proto[method] === 'function', typeof proto[method]);
}

/** Polls until `predicate` is true, or gives up. */
async function waitUntil(predicate, budgetMs = 8000) {
  const deadline = Date.now() + budgetMs;
  while (Date.now() < deadline) {
    if (predicate()) return true;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  return false;
}

/** Opens a tunnel against the mock API. */
async function connectToMock(mock, extra = {}) {
  return api.connect({
    token: 'test-token',
    domain: 'example.com',
    port: 8080,
    subdomain: 'demo',
    apiBase: mock.base,
    cloudflaredPath: writeFakeCloudflared(),
    ...extra,
  });
}

// --- Argument validation (must fail before any network call) ----------
(async () => {
  const rejects = async (label, options) => {
    try {
      await api.connect(options);
      problems.push(`${label}: connect() unexpectedly succeeded`);
      console.error(`  FAIL ${label}: unexpectedly succeeded`);
    } catch (err) {
      const code = api.parseErrorCode((err && err.message) || '');
      check(`${label} → ${code}`, code !== null, (err && err.message || '').split('\n')[0]);
    }
  };

  console.log('');
  // `port: 0` produced `http://localhost:0` before; it now fails fast.
  await rejects('port 0 is rejected', { token: 'placeholder', domain: 'example.com', port: 0 });
  await rejects('unknown protocol', { token: 'placeholder', domain: 'example.com', protocol: 'gopher' });
  await rejects('empty token', { token: '', domain: 'example.com' });

  console.log('');
  console.log('--- end-to-end against the mock API ---');

  // The startup events fire while the tunnel is being created — before
  // `connect()` even resolves. Without the pending buffer they were
  // dropped, and no JS caller could ever see `dnsCreated`.
  {
    const mock = await startMockApi();
    const listener = await connectToMock(mock);

    // A typo in the event name must fail loudly. The facade registers
    // natively for every known name up front, so it never routes a
    // caller's `on()` through the native check — it has to validate
    // the name itself.
    let unknownErr = null;
    try {
      listener.on('dnsCreatedd', () => {});
    } catch (err) {
      unknownErr = err;
    }
    check('an unknown event name is rejected', unknownErr !== null);
    check(
      'the rejection carries INVALID_ARGUMENT',
      unknownErr && unknownErr.code === 'INVALID_ARGUMENT',
      unknownErr && String(unknownErr.message),
    );
    check(
      'the rejection message carries the code prefix',
      unknownErr && api.parseErrorCode(unknownErr.message) === 'INVALID_ARGUMENT',
      unknownErr && String(unknownErr.message),
    );

    const seen = [];
    listener.on('dnsCreated', (payload) => seen.push(payload));

    const delivered = await waitUntil(() => seen.length > 0);
    check('the startup event reaches a late handler', delivered, JSON.stringify(seen));

    let parsed = null;
    try {
      parsed = JSON.parse(seen[0]);
    } catch (err) {
      problems.push(`the payload is not a JSON string: ${err.message}`);
      console.error(`  FAIL the payload is not a JSON string: ${err.message}`);
    }
    check('the payload parses', parsed !== null);
    check(
      'the payload carries the hostname',
      parsed && parsed.fullName === 'demo.example.com',
      parsed && JSON.stringify(parsed),
    );

    await listener.close();
    mock.close();
  }

  // `off(event, cb)` must remove exactly one callback, not the whole
  // event. The fake cloudflared reports three connections: two before the
  // removal, one after.
  {
    const mock = await startMockApi();
    const listener = await connectToMock(mock);

    let first = 0;
    let second = 0;
    const a = () => first++;
    const b = () => second++;
    listener.on('edgeConnected', a);
    listener.on('edgeConnected', b);

    const both = await waitUntil(() => first >= 2 && second >= 1);
    check('both callbacks saw the first connections', both, `first=${first} second=${second}`);

    const beforeFirst = first;
    listener.off('edgeConnected', a);

    const later = await waitUntil(() => second >= 2);
    check('the surviving callback kept receiving', later, `second=${second}`);
    check(
      'the removed callback stopped receiving',
      first === beforeFirst,
      `first grew from ${beforeFirst} to ${first}`,
    );

    await listener.close();
    mock.close();
  }

  console.log('');
  if (problems.length > 0) {
    console.error(`${problems.length} check(s) failed:`);
    for (const p of problems) console.error(`  - ${p}`);
    process.exit(1);
  }
  console.log('OK — all checks passed');
})();
