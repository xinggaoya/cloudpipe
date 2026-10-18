// Local smoke test: verifies the built .node module loads and exposes
// the documented API surface. Run after `npm run build:local`.

'use strict';

const path = require('node:path');

// Path-based require: the smoke test loads from the locally built
// artifact rather than the registry, so the package name is irrelevant.
const mod = require(path.resolve(__dirname, '..', 'npm', 'index.js'));
const api = mod.default ?? mod;

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
for (const key of ['connect', 'version', 'parseErrorCode', 'eventNames', 'errorCodes', 'cloudflareErrorKinds']) {
  check(`${key} is a function`, typeof api[key] === 'function', typeof api[key]);
}

// --- parseErrorCode ----------------------------------------------------
const codeCases = [
  ['[MISSING_CREDENTIAL] need a token', 'MISSING_CREDENTIAL'],
  ['[CLOUDFLARE_API:RATE_LIMITED] slow down', 'CLOUDFLARE_API:RATE_LIMITED'],
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
  ['MISSING_CREDENTIAL', 'CLOUDFLARE_API', 'INVALID_SUBDOMAIN', 'SUBDOMAIN_IN_USE', 'ALREADY_SHUT_DOWN']
    .every((c) => api.errorCodes().includes(c)),
  api.errorCodes().join(','),
);
check(
  'cloudflareErrorKinds lists the documented set',
  ['RATE_LIMITED', 'DNS_EXISTS', 'AUTH_FAILED', 'INVALID_TOKEN'].every((k) =>
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
  if (problems.length > 0) {
    console.error(`${problems.length} check(s) failed:`);
    for (const p of problems) console.error(`  - ${p}`);
    process.exit(1);
  }
  console.log('OK — all checks passed');
})();
