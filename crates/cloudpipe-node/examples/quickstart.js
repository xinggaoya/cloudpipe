'use strict';

// Minimal end-to-end tunnel.
//
// Prereqs:
//   - CLOUDFLARE_API_TOKEN env var
//   - CFP_DOMAIN env var
//   - something listening on localhost:8080 (e.g. python -m http.server 8080)
//
// Run with:
//   node examples/quickstart.js

const { connect, parseErrorCode } = require('@xinggao/cloudpipe');

(async () => {
  const token = process.env.CLOUDFLARE_API_TOKEN;
  const domain = process.env.CFP_DOMAIN;
  if (!token || !domain) {
    console.error('set CLOUDFLARE_API_TOKEN and CFP_DOMAIN to run this example');
    process.exit(1);
  }

  let listener;
  try {
    listener = await connect({
      token,
      domain,
      port: 8080,
      subdomain: 'myapp',
      autoRestart: true,
    });
  } catch (err) {
    const code = parseErrorCode(err.message);
    switch (code) {
      case 'MISSING_CREDENTIAL':
        console.error('no token configured');
        break;
      case 'CLOUDFLARE_API:AUTH_FAILED':
        console.error('Cloudflare rejected the token — check its permissions');
        break;
      case 'CLOUDFLARE_API:RATE_LIMITED':
        console.error('rate limited; wait a moment and retry');
        break;
      case 'SUBDOMAIN_IN_USE':
        console.error('that subdomain already serves a live tunnel');
        break;
      default:
        console.error(`connect failed (${code ?? 'no code'}):`, err.message);
    }
    process.exit(1);
  }

  console.log(`live at ${listener.url}`);
  console.log(`forwarding localhost:${listener.port}`);

  process.on('SIGINT', async () => {
    console.log('\nshutting down…');
    await listener.close();
    console.log('cleaned up');
    process.exit(0);
  });
})();
