'use strict';

// Subscribe to lifecycle events to render your own UI.
//
// Prereqs:
//   - CLOUDFLARE_API_TOKEN env var
//   - CFP_DOMAIN env var
//   - something listening on localhost:8080 (e.g. python -m http.server 8080)
//
// Run with:
//   node examples/with-events.js

// Prefer the published package, so the example shows what a consumer
// writes. Fall back to the local build so it also runs in this
// repository, without a publish step in between.
const { connect, eventNames } = (() => {
  try {
    return require('@xinggao/cloudpipe');
  } catch {
    return require('../js/index.js');
  }
})();

function parse(json) {
  try {
    return JSON.parse(json);
  } catch {
    return json;
  }
}

(async () => {
  const token = process.env.CLOUDFLARE_API_TOKEN;
  const domain = process.env.CFP_DOMAIN;
  if (!token || !domain) {
    console.error(
      'set CLOUDFLARE_API_TOKEN and CFP_DOMAIN before running this example',
    );
    process.exit(1);
  }

  // The runtime validates event names, so this list is the same one
  // `listener.on()` accepts.
  console.log('subscribing to:', eventNames().join(', '), '\n');

  const listener = await connect({
    token,
    domain,
    port: 8080,
    autoRestart: true,
  });

  for (const name of eventNames()) {
    listener.on(name, (json) => {
      console.log(`[${name}]`, parse(json));
    });
  }

  console.log('live at', listener.url);
  console.log('forwarding localhost:', listener.port);

  // Move the tunnel to another local port without changing the URL.
  setTimeout(async () => {
    console.log('\nrepointing to port 9090 (same public URL)…');
    try {
      await listener.update(9090);
      console.log('now forwarding localhost:', listener.port);
    } catch (err) {
      console.error('update failed:', err.message);
    }
  }, 10_000).unref?.();

  const shutdown = async (signal) => {
    console.log(`\n${signal} — closing`);
    try {
      // `off` drops the handlers first so the teardown events do not
      // interleave with our own logging.
      listener.removeAllListeners();
      await listener.close();
    } catch (err) {
      console.error('close failed:', err.message);
      process.exitCode = 1;
    }
    process.exit(process.exitCode ?? 0);
  };

  process.on('SIGINT', () => shutdown('SIGINT'));
  process.on('SIGTERM', () => shutdown('SIGTERM'));
})();
