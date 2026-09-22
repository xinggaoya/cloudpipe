'use strict';

// E2E test for parseErrorCode helper. Loads the built npm/index.js
// (produced by `napi build`) and runs each Rust branch through the
// binding boundary so we exercise the same code path real consumers
// use.

const path = require('node:path');

// Path-based require — load from the local built artifact.
const mod = require(path.resolve(__dirname, '..', 'npm', 'index.js'));
const api = mod.default ?? mod;

if (typeof api.parseErrorCode !== 'function') {
  console.error('expected parseErrorCode to be a function');
  process.exit(1);
}

const cases = [
  // [input, expected, description]
  ['[MISSING_CREDENTIAL] need a token', 'MISSING_CREDENTIAL', 'top-level code'],
  ['[CLOUDFLARE_API:RATE_LIMITED] too many', 'CLOUDFLARE_API:RATE_LIMITED', 'cloudflare sub-kind'],
  ['[CLOUDFLARE_API:DNS_EXISTS] dup', 'CLOUDFLARE_API:DNS_EXISTS', 'cloudflare DNS exists'],
  ['[CLOUDFLARE_API:AUTH_FAILED] nope', 'CLOUDFLARE_API:AUTH_FAILED', 'cloudflare auth failed'],
  ['[CLOUDFLARE_API:INVALID_TOKEN] bad', 'CLOUDFLARE_API:INVALID_TOKEN', 'invalid token'],
  ['[CLOUDFLARE_BINARY] no dl', 'CLOUDFLARE_BINARY', 'binary fetch failure'],
  ['[INVALID_SUBDOMAIN] nope', 'INVALID_SUBDOMAIN', 'invalid subdomain'],
  ['[SUBDOMAIN_IN_USE] taken', 'SUBDOMAIN_IN_USE', 'subdomain in use'],
  ['[ALREADY_SHUT_DOWN]', 'ALREADY_SHUT_DOWN', 'shut-down sentinel'],
  ['[IO] eperm', 'IO', 'io failure'],
  ['[OTHER] anything', 'OTHER', 'fallback'],
  // forward-compat: prefix parser should NOT throw on unknown codes
  ['[FUTURE_CODE] blah', 'FUTURE_CODE', 'unknown parent -> verbatim'],
  ['[CLOUDFLARE_API:RATE_LIMITED_EXTENDED] blah', 'CLOUDFLARE_API:RATE_LIMITED_EXTENDED', 'unknown subkind -> verbatim'],
  // negative cases
  ['', null, 'empty message'],
  ['no prefix here', null, 'plain message'],
  ['[unclosed', null, 'malformed prefix'],
];

let failed = 0;
for (const [input, expected, desc] of cases) {
  const got = api.parseErrorCode(input);
  const ok = got === expected;
  console.log(`${ok ? 'ok  ' : 'FAIL'}  ${desc}: parseErrorCode(${JSON.stringify(input)}) === ${JSON.stringify(expected)} (got ${JSON.stringify(got)})`);
  if (!ok) failed++;
}

if (failed > 0) {
  console.error(`\n${failed} case(s) failed`);
  process.exit(1);
}
console.log(`\nall ${cases.length} cases passed`);
