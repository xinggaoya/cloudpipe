'use strict';

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

/**
 * Writes a fake `cloudflared` that reports three edge connections and then
 * stays alive.
 *
 * The "Registered tunnel connection" lines are what the SDK turns into
 * `edgeConnected` events — which is what makes it possible to test that
 * `off(event, cb)` removes exactly one callback: the first two arrive
 * before the callback is removed, the third after.
 *
 * @returns {string} the path to the fake binary
 */
function writeFakeCloudflared() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'cfp-fake-'));
  const script = path.join(dir, 'cloudflared');
  fs.writeFileSync(
    script,
    [
      '#!/bin/sh',
      'echo "2025-11-01T12:00:00Z INF Registered tunnel connection" >&2',
      'sleep 1',
      'echo "2025-11-01T12:00:01Z INF Registered tunnel connection" >&2',
      'sleep 2',
      'echo "2025-11-01T12:00:03Z INF Registered tunnel connection" >&2',
      // `exec` replaces the shell with the sleep, so the pid the SDK
      // signals on shutdown *is* the sleep. A `while true; do sleep 1;
      // done` loop would leave the sleep orphaned, holding the inherited
      // stdout pipe open — and npm waits for that pipe to close.
      'exec sleep 300',
      '',
    ].join('\n'),
  );
  fs.chmodSync(script, 0o755);
  return script;
}

module.exports = { writeFakeCloudflared };
