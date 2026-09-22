// Local smoke test: verifies the built .node module loads and exposes
// the documented API surface. Run after `npm run build:local`.

'use strict';

const path = require('node:path');

const mod = require(path.resolve(__dirname, '..', 'npm', 'index.js'));
const api = mod.default ?? mod;

console.log('module exports:', Object.keys(api));

const expected = ['connect', 'version'];
for (const key of expected) {
  if (typeof api[key] !== 'function') {
 console.error(`expected export "${key}" to be a function, got ${typeof api[key]}`);
 process.exit(1);
 }
}

console.log('version:', api.version());

if (typeof api.connect !== 'function') process.exit(1);

(async () => {
 try {
 await api.connect({ token: '', domain: '', port: 0 });
 console.error('expected connect() with empty token to fail, but it returned');
 process.exit(1);
 } catch (err) {
 // connect() should fail with [MISSING_CREDENTIAL] or fail to authenticate;
 // we just verify it surfaces SOMETHING with a [CODE] prefix.
 const msg = err && err.message ? String(err.message) : '';
 console.log('connect() rejected as expected:', msg.split('\n')[0]);
 if (!/^\[\w+/.test(msg)) {
 console.error('error message did not carry the [CODE] prefix');
 process.exit(1);
 }
 }

 console.log('OK');
})();